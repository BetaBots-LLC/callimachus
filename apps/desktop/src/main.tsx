import React, { useEffect } from "react";
import ReactDOM from "react-dom/client";
import { QueryClientProvider } from "@tanstack/react-query";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { openUrl } from "@tauri-apps/plugin-opener";
import App from "./App";
import { ErrorBoundary } from "./components/ErrorBoundary";
import { Toaster } from "@/components/ui/toast";
import type { EmbedStatus, IndexProgress, IndexReport, JobsStatus } from "./lib/api";
import {
  EMBED_ERROR_KEY,
  INDEX_ERROR_KEY,
  isSearchResultsKey,
  takeUserStarted,
} from "./lib/queries";
import { invalidateDomain, memoryWritePending } from "./lib/keys";
import { AGGREGATE_KEYS, invalidateIndexDerived, queryClient } from "./lib/queryClient";
import { isSafeExternalHref } from "./lib/safeLinks";
import { track } from "./lib/telemetry";
import { errorText, notify } from "./lib/toast";
import { useUi } from "./store/ui";
import { startUpdater } from "./store/updater";
import "./index.css";

// Push-based embedding progress: the backend emits counts per batch, so the UI never
// re-runs `embedding_status` (two COUNT(*) scans) per tick while a build runs. See
// lib/queries.ts for the matching query policy.
void listen<{ done: number; total: number }>("embed:progress", (e) => {
  queryClient.setQueryData<EmbedStatus>(["embed_status"], {
    done: e.payload.done,
    total: e.payload.total,
    running: true,
  });
  // A build is making progress again, so an error from an earlier attempt is stale.
  if (queryClient.getQueryData(EMBED_ERROR_KEY)) queryClient.setQueryData(EMBED_ERROR_KEY, null);
});
// The build stopped because the model couldn't load (offline first run, proxy, full disk)
// or every message failed inference. Nothing was marked embedded, so the user can retry.
// `embed:done` still follows and clears the running state as usual.
void listen<string>("embed:error", (e) => {
  const reason = String(e.payload ?? "") || "unknown error";
  queryClient.setQueryData(EMBED_ERROR_KEY, reason);
  track("error_shown", { kind: "index_failed" });
  if (takeUserStarted("embed")) {
    notify.error("Semantic index build stopped", {
      description: `${reason}. Nothing was marked as embedded, so building again picks up where it left off.`,
    });
  }
});
void listen("embed:done", () => {
  // One authoritative refetch for the final counts + running:false, plus Stats'
  // semantic-coverage figure.
  void queryClient.invalidateQueries({ queryKey: ["embed_status"] });
  void queryClient.invalidateQueries({ queryKey: ["index_stats"] });
  if (takeUserStarted("embed") && !queryClient.getQueryData(EMBED_ERROR_KEY)) {
    notify.success("Semantic index is up to date", {
      description: "Natural-language search now covers your latest messages.",
    });
  }
});
// The one-time TODO backfill finished (at launch on an upgraded index, or after enabling
// distillation). Projects shows those TODOs too, so a user sitting there sees them now.
void listen("knowledge:todos-ready", () => {
  invalidateDomain("memory");
  invalidateDomain("projects");
});
// Per-source reindex progress → drives the progress bar (JobProgress). A reindex emits
// after each of the first 50 threads and then every 10, about 2,100 events for 21k
// threads and often faster than 60 per second on a no-op rescan, so only the latest
// payload is written, once per animation frame.
let pendingIndexProgress: IndexProgress | null = null;
let indexProgressFrame = 0;
function flushIndexProgress() {
  indexProgressFrame = 0;
  queryClient.setQueryData<IndexProgress | null>(["index_progress"], pendingIndexProgress);
  pendingIndexProgress = null;
  // A reindex started elsewhere (the command palette, Settings, another window) reaches
  // the search bar only through jobs_status, which is not polled while it last read idle.
  const jobs = queryClient.getQueryState<JobsStatus>(["jobs_status"]);
  if (jobs && jobs.fetchStatus === "idle" && !jobs.data?.index && !jobs.data?.catchUp) {
    void queryClient.invalidateQueries({ queryKey: ["jobs_status"] });
  }
}
void listen<IndexProgress>("index:progress", (e) => {
  pendingIndexProgress = e.payload;
  if (!indexProgressFrame) indexProgressFrame = requestAnimationFrame(flushIndexProgress);
});
// Per-thread project-distill progress + completion → drive the bar, refresh the memory.
void listen<{ done: number; total: number }>("distill:progress", (e) => {
  queryClient.setQueryData(["distill_progress"], e.payload);
});
void listen("distill:done", () => {
  queryClient.setQueryData(["distill_progress"], null);
  // auto_distill_budget: an auto-distill round spends today's allowance (Settings shows it).
  for (const key of [
    "distill_status",
    "project_memory",
    "projects",
    "knowledge_config",
    "auto_distill_budget",
  ]) {
    void queryClient.invalidateQueries({ queryKey: [key] });
  }
});

// A full reindex failed (unreadable agent dir, DB error, locked index). The backend still
// follows with an empty `index:done`, which alone would read as "you have no history", so
// keep the reason for Onboarding / Settings / the search bar to show instead.
// `indexFailedThisRun` pairs it with that `index:done` (same backend thread, so the two
// arrive in order): a later clean run clears the message, this run's done does not.
let indexFailedThisRun = false;
void listen<string>("index:error", (e) => {
  indexFailedThisRun = true;
  queryClient.setQueryData(INDEX_ERROR_KEY, String(e.payload ?? "") || "unknown error");
  track("error_shown", { kind: "index_failed" });
});

// Background re-index finished: clear progress, refresh results/stats/button state.
void listen<IndexReport | null>("index:done", (e) => {
  // Drop a progress frame still queued from this run, or it would bring the bar back.
  if (indexProgressFrame) cancelAnimationFrame(indexProgressFrame);
  indexProgressFrame = 0;
  pendingIndexProgress = null;
  queryClient.setQueryData(["index_progress"], null);
  if (!indexFailedThisRun && queryClient.getQueryData(INDEX_ERROR_KEY)) {
    queryClient.setQueryData(INDEX_ERROR_KEY, null);
  }
  // Only a reindex the user asked for gets a toast; the launch scan stays quiet.
  if (takeUserStarted("index")) {
    const r = e.payload;
    if (indexFailedThisRun) {
      notify.error("Reindex failed", {
        description: errorText(queryClient.getQueryData(INDEX_ERROR_KEY)),
      });
    } else if (r) {
      notify.success("Reindex finished", {
        description: `${r.threadsIndexed.toLocaleString()} threads and ${r.messagesIndexed.toLocaleString()} messages indexed${r.errors > 0 ? `, ${r.errors} skipped with errors` : ""}.`,
      });
    } else {
      notify.success("Reindex finished");
    }
  }
  indexFailedThisRun = false;
  // A reindex has now completed at least once this session. The empty-state
  // (Onboarding) reads this to say "no threads found" instead of re-offering the
  // first-run "Index my history" button, which read as "nothing happened".
  queryClient.setQueryData(["index_ran"], true);
  // embed_status too: new messages change how much of the semantic index is built.
  // thread_commits: `cal commits` and a reindex can both change an open thread's links.
  for (const key of [
    "results",
    "db_stats",
    "index_status",
    "jobs_status",
    "embed_status",
    "thread_commits",
  ]) {
    void queryClient.invalidateQueries({ queryKey: [key] });
  }
  invalidateIndexDerived();
});

/**
 * Live updates from the file watcher. While agents work, `index:updated` fires on every
 * debounced batch of transcript writes, so refreshes are throttled (leading + trailing)
 * and limited to what is on screen and cheap:
 *  - the recent list (no query), db_stats, index_stats and the open thread's commit links
 *    (`cal commits` can write them from a terminal) refetch only if a mounted view
 *    observes them;
 *  - a typed search is only marked stale, and ResultsList offers "Refresh results"
 *    instead. Re-running it every 4 s cost 0.6-0.7 s of search per run on a 21k-thread
 *    index (about 15% of the time spent fetching) and reordered rows under the cursor;
 *  - an open Project page's memory and TODO lists refetch too: a decision or TODO an agent
 *    records (MCP, `cal remember`) lands while its session writes, and showed up only after
 *    the next navigation. Not while one of the user's own optimistic writes to them is
 *    pending (`memoryWritePending`): the refetch would read the index before that write
 *    commits and put the row back. They are marked stale instead and the write's own
 *    refresh picks up both;
 *  - Today is not among them. Its command also links commits (a `git log` per checkout and
 *    a write, up to 2 s), which the watcher already does once a minute, so it stays one of
 *    the aggregates: marked stale, reloaded on the next visit;
 *  - open threads and the heavy aggregates are only marked stale (no refetch now), so
 *    they reload the next time a view mounts them. Refetching a multi-MB open thread
 *    every few seconds while an agent writes to it would cost more than it shows;
 *  - while the window is hidden (tray, minimized) nothing refetches: everything is marked
 *    stale, and one refresh runs when it is shown again.
 * In-app chats (["chats"]) are not watcher-indexed, so they are left alone.
 */
const LIVE_UPDATE_THROTTLE_MS = 4_000;
// project_recent is the Project page's Recent sessions: a cheap ordered read of 10 threads,
// and the list an agent working in that repo changes. project_memory and open_todos are the
// memory domain (lib/keys.ts) minus today (see above) and agent_view, which renders the
// whole hook preview.
const LIVE_KEYS = [
  "results",
  "db_stats",
  "index_stats",
  "thread_commits",
  "project_recent",
  "project_memory",
  "open_todos",
] as const;
/** The live keys a pending optimistic write holds back (`memoryWritePending`). */
const MEMORY_LIVE_KEYS = new Set<string>(["project_memory", "open_todos"]);
/** Refetch the mounted queries of a live key, or only mark them stale while one of the
 *  user's own writes to that list is pending. */
function refreshLiveKey(key: (typeof LIVE_KEYS)[number], memoryBusy: boolean) {
  void queryClient.invalidateQueries({
    queryKey: [key],
    refetchType: memoryBusy && MEMORY_LIVE_KEYS.has(key) ? "none" : "active",
  });
}
let liveTimer: ReturnType<typeof setTimeout> | null = null;
let liveLastRun = 0;
let missedWhileHidden = false;
function refreshAfterWatcherUpdate() {
  liveTimer = null;
  liveLastRun = Date.now();
  for (const key of ["thread", ...AGGREGATE_KEYS]) {
    void queryClient.invalidateQueries({ queryKey: [key], refetchType: "none" });
  }
  if (document.visibilityState === "hidden") {
    missedWhileHidden = true;
    for (const key of LIVE_KEYS) {
      void queryClient.invalidateQueries({ queryKey: [key], refetchType: "none" });
    }
    return;
  }
  const isSearch = (q: { queryKey: readonly unknown[] }) => isSearchResultsKey(q.queryKey);
  void queryClient.invalidateQueries({
    queryKey: ["results"],
    predicate: (q) => !isSearch(q),
    refetchType: "active",
  });
  void queryClient.invalidateQueries({
    queryKey: ["results"],
    predicate: isSearch,
    refetchType: "none",
  });
  if (
    queryClient
      .getQueryCache()
      .findAll({ queryKey: ["results"], type: "active", predicate: isSearch }).length > 0
  ) {
    useUi.getState().noteSearchActivity();
  }
  const memoryBusy = memoryWritePending();
  for (const key of LIVE_KEYS) {
    if (key === "results") continue; // handled above: a typed search is only marked stale
    refreshLiveKey(key, memoryBusy);
  }
}
// Back from hidden after the watcher reported writes: one refresh of what is on screen,
// search included (the user was away, so nothing reorders under the cursor).
document.addEventListener("visibilitychange", () => {
  if (document.visibilityState !== "visible" || !missedWhileHidden) return;
  missedWhileHidden = false;
  // This refresh covers a trailing one already scheduled; firing it right after would
  // refetch everything again and raise "New activity" over results that are current.
  if (liveTimer) {
    clearTimeout(liveTimer);
    liveTimer = null;
  }
  liveLastRun = Date.now();
  const memoryBusy = memoryWritePending();
  for (const key of LIVE_KEYS) refreshLiveKey(key, memoryBusy);
});
void listen<string | null>("index:updated", (e) => {
  // No source kind: startup housekeeping rewrote rows (re-keyed projects), or the
  // background tool-output trim freed pages (db_stats' Reclaimable, via the live refresh
  // below), not a scan. Both are rare (once per launch, a few passes once per index), so
  // refresh what groups by project now, including an open Project Memory list, which would
  // otherwise keep showing keys that no longer exist.
  if (e.payload == null) {
    invalidateIndexDerived(); // ["projects"] and the other aggregates, refetched if mounted
    void queryClient.invalidateQueries({ queryKey: ["project_memory"], refetchType: "active" });
  }
  if (liveTimer) return; // a trailing refresh is already scheduled; it covers this event
  const wait = liveLastRun + LIVE_UPDATE_THROTTLE_MS - Date.now();
  if (wait <= 0)
    refreshAfterWatcherUpdate(); // leading edge
  else liveTimer = setTimeout(refreshAfterWatcherUpdate, wait); // trailing edge
});

/**
 * Keep the app window from ever navigating away. Rendered transcripts can contain links,
 * and a plain click on one would replace the whole app with that page (no back button)
 * or a dead relative URL. Every anchor click is handled here: http(s)/mailto open in the
 * system browser through the opener plugin; anything else (relative paths, javascript:,
 * file:) does nothing. In-page fragment links (#footnote) keep their default behavior.
 * Registered on `document`, so it runs before the opener plugin's own window-level
 * listener, which skips events that are already defaultPrevented.
 */
function guardLinkNavigation(e: MouseEvent) {
  if (e.defaultPrevented) return;
  // Primary button for click, middle button for auxclick. WebView2 and WebKit also fire
  // auxclick on right-click release; acting on that would open the browser every time
  // someone right-clicks a link to copy it.
  if (e.button !== (e.type === "auxclick" ? 1 : 0)) return;
  const anchor = e
    .composedPath()
    .find((n): n is HTMLAnchorElement => n instanceof HTMLAnchorElement && n.hasAttribute("href"));
  if (!anchor) return;
  const href = anchor.getAttribute("href") ?? "";
  if (href.startsWith("#")) return;
  e.preventDefault();
  if (isSafeExternalHref(href)) {
    openUrl(href.trim()).catch((err) => console.error("[ui] couldn't open link:", err));
  }
}
document.addEventListener("click", guardLinkNavigation);
// Middle-click would otherwise ask the webview to open a new window (only button 1 is
// handled; see the filter above).
document.addEventListener("auxclick", guardLinkNavigation);

// Surface errors outside React's render path (event handlers, unawaited promises) in
// the devtools console with a recognizable prefix instead of failing silently.
window.addEventListener("error", (e) => console.error("[ui] uncaught error:", e.error ?? e));
window.addEventListener("unhandledrejection", (e) =>
  console.error("[ui] unhandled rejection:", e.reason),
);

// Background update checks (launch, every 6 h, on focus) and the "Updated to …" toast.
startUpdater();

// The main window is hidden at launch while the splash shows; the backend reveals it once
// both halves report ready. If the frontend never reports (a hang, or an error before
// AppShell's effect commits), reveal anyway after a grace period rather than leaving the
// user on an undecorated splash with no way to close it. set_complete is idempotent.
const SPLASH_FALLBACK_MS = 8_000;
setTimeout(() => {
  void invoke("set_complete", { task: "frontend" }).catch(() => {});
}, SPLASH_FALLBACK_MS);

/** The main window is hidden at launch (tauri.conf) while the splash WINDOW shows. The
 *  app loads here behind it; once it has mounted we signal the backend, which closes the
 *  splash and reveals this window only when its own setup is also done. Deliberately not
 *  gated on any query: on a cold page cache db_stats' message count took 0.6-0.8 s before
 *  the covering indexes (0031; 4 ms now), and a gate would have kept the splash up that
 *  long. The header count fills in a moment later. */
function AppShell() {
  useEffect(() => {
    // set_complete is idempotent, so StrictMode's double effect (dev) is harmless.
    void invoke("set_complete", { task: "frontend" }).catch(() => {});
  }, []);
  return <App />;
}

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <QueryClientProvider client={queryClient}>
      <ErrorBoundary scope="app">
        <AppShell />
      </ErrorBoundary>
      <Toaster />
    </QueryClientProvider>
  </React.StrictMode>,
);
