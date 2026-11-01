import React, { useEffect } from "react";
import ReactDOM from "react-dom/client";
import { QueryClientProvider, useQuery } from "@tanstack/react-query";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { openUrl } from "@tauri-apps/plugin-opener";
import App from "./App";
import { ErrorBoundary } from "./components/ErrorBoundary";
import { api, type EmbedStatus, type IndexProgress } from "./lib/api";
import { EMBED_ERROR_KEY, INDEX_ERROR_KEY } from "./lib/queries";
import { AGGREGATE_KEYS, invalidateIndexDerived, queryClient } from "./lib/queryClient";
import { isSafeExternalHref } from "./lib/safeLinks";
import { track } from "./lib/telemetry";
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
  queryClient.setQueryData(EMBED_ERROR_KEY, String(e.payload ?? "") || "unknown error");
  track("error_shown", { kind: "index_failed" });
});
void listen("embed:done", () => {
  // One authoritative refetch for the final counts + running:false, plus Stats'
  // semantic-coverage figure.
  void queryClient.invalidateQueries({ queryKey: ["embed_status"] });
  void queryClient.invalidateQueries({ queryKey: ["index_stats"] });
});
// Background todo backfill (after enabling the Knowledge feature) finished.
void listen("knowledge:todos-ready", () => {
  void queryClient.invalidateQueries({ queryKey: ["open_todos"] });
});
// Per-source reindex progress → drives the progress bar.
void listen<IndexProgress>("index:progress", (e) => {
  queryClient.setQueryData<IndexProgress>(["index_progress"], e.payload);
});
// Per-thread project-distill progress + completion → drive the bar, refresh the memory.
void listen<{ done: number; total: number }>("distill:progress", (e) => {
  queryClient.setQueryData(["distill_progress"], e.payload);
});
void listen("distill:done", () => {
  queryClient.setQueryData(["distill_progress"], null);
  for (const key of ["distill_status", "project_memory", "projects", "knowledge_config"]) {
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

// Background re-index finished — clear progress, refresh results/stats/button state.
void listen("index:done", () => {
  queryClient.setQueryData(["index_progress"], null);
  if (!indexFailedThisRun && queryClient.getQueryData(INDEX_ERROR_KEY)) {
    queryClient.setQueryData(INDEX_ERROR_KEY, null);
  }
  indexFailedThisRun = false;
  // A reindex has now completed at least once this session. The empty-state
  // (Onboarding) reads this to say "no threads found" instead of re-offering the
  // first-run "Index my history" button, which read as "nothing happened".
  queryClient.setQueryData(["index_ran"], true);
  // embed_status too: new messages change how much of the semantic index is built.
  for (const key of ["results", "db_stats", "index_status", "embed_status"]) {
    void queryClient.invalidateQueries({ queryKey: [key] });
  }
  invalidateIndexDerived();
});

/**
 * Live updates from the file watcher. While agents work, `index:updated` fires on every
 * debounced batch of transcript writes, so refreshes are throttled (leading + trailing)
 * and limited to what is on screen:
 *  - results / db_stats / index_stats refetch only if a mounted view observes them;
 *  - open threads and the heavy aggregates are only marked stale (no refetch now), so
 *    they reload the next time a view mounts them. Refetching a multi-MB open thread
 *    every few seconds while an agent writes to it would cost more than it shows.
 * In-app chats (["chats"]) are not watcher-indexed, so they are left alone.
 */
const LIVE_UPDATE_THROTTLE_MS = 4_000;
let liveTimer: ReturnType<typeof setTimeout> | null = null;
let liveLastRun = 0;
function refreshAfterWatcherUpdate() {
  liveTimer = null;
  liveLastRun = Date.now();
  for (const key of ["results", "db_stats", "index_stats"]) {
    void queryClient.invalidateQueries({ queryKey: [key], refetchType: "active" });
  }
  for (const key of ["thread", ...AGGREGATE_KEYS]) {
    void queryClient.invalidateQueries({ queryKey: [key], refetchType: "none" });
  }
}
void listen("index:updated", () => {
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

// The main window is hidden at launch while the splash shows; the backend reveals it once
// both halves report ready. If the frontend never reports (a hang, or an error before
// AppShell's effect commits), reveal anyway after a grace period rather than leaving the
// user on an undecorated splash with no way to close it. set_complete is idempotent.
const SPLASH_FALLBACK_MS = 8_000;
setTimeout(() => {
  void invoke("set_complete", { task: "frontend" }).catch(() => {});
}, SPLASH_FALLBACK_MS);

/** The main window is hidden at launch (tauri.conf) while the splash WINDOW shows. The
 *  app loads here behind it; once our initial data settles we signal the backend, which
 *  closes the splash and reveals this window only when its own setup is also done. The
 *  db_stats query shares the header's ["db_stats"] cache, so this adds no extra fetch. */
function AppShell() {
  const stats = useQuery({ queryKey: ["db_stats"], queryFn: api.dbStats, retry: 0 });
  useEffect(() => {
    // Settled (success OR error) → frontend is ready. set_complete is idempotent, so an
    // extra call is harmless; the backend reveals the window only when its half is done too.
    if (!stats.isLoading) void invoke("set_complete", { task: "frontend" });
  }, [stats.isLoading]);
  return <App />;
}

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <QueryClientProvider client={queryClient}>
      <ErrorBoundary scope="app">
        <AppShell />
      </ErrorBoundary>
    </QueryClientProvider>
  </React.StrictMode>,
);
