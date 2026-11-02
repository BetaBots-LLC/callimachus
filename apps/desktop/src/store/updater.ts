// The auto-updater, as one piece of state the whole app reads: the toast that announces a
// release, the dot on the Settings nav, and the Updates section with its progress bar all
// come from here. The Tauri v2 updater flow is `check()`, then on the user's go
// `update.download(...)` + `update.install()`, then `relaunch()`.
//
// Checks run at launch, every 6 hours, and when the window regains focus or becomes
// visible again (restored from the tray) if the last attempt is over an hour old. A failed
// background check retries once after 5 minutes and is otherwise silent (offline at launch,
// or a dev build with no signed artifacts). A manual check (Settings, command palette)
// always runs, even while a background check is in flight, and is answered by the Updates
// section itself: "up to date", the release, or the failure reason, in place. Install is
// gated behind a click: a restart is disruptive.
import { getVersion } from "@tauri-apps/api/app";
import { openUrl } from "@tauri-apps/plugin-opener";
import { relaunch } from "@tauri-apps/plugin-process";
import { check as checkForUpdate, type Update } from "@tauri-apps/plugin-updater";
import { create } from "zustand";
import { flushTelemetry, track } from "../lib/telemetry";
import { errorText, notify } from "../lib/toast";
import { useUi } from "./ui";

export type UpdaterStatus =
  | "idle" // nothing known (or up to date)
  | "checking" // a manual check is in flight
  | "available" // a newer release is ready to install
  | "downloading"
  | "installing"
  | "installed"; // the new build is on disk but relaunch() failed: quit and reopen

export interface UpdateInfo {
  version: string;
  /** Release notes (markdown), when the manifest carries them. */
  notes: string | null;
  /** RFC 3339 publish date, when the manifest carries one. */
  date: string | null;
}

interface UpdaterState {
  status: UpdaterStatus;
  /** The running build's version, once the backend has answered. */
  currentVersion: string | null;
  update: UpdateInfo | null;
  /** Download progress, 0..100, while `downloading`. */
  pct: number;
  /** Why the last check or install failed, for the Updates section. */
  error: string | null;
  /** When a check last answered (found a release or confirmed none). */
  lastCheckedAt: number | null;
  check: (opts?: { manual?: boolean }) => Promise<void>;
  install: () => Promise<void>;
  /** "Later" on the toast: that version stays quiet, across launches, until a newer one. */
  dismiss: () => void;
}

const HOUR = 60 * 60_000;
/** Background re-check cadence: with close-to-tray the app can run for weeks. */
const CHECK_INTERVAL_MS = 6 * HOUR;
/** Window focus / restore-from-tray re-checks at most this often (counted from the last
 *  attempt, so an offline machine isn't asked again on every focus). */
const FOCUS_CHECK_MIN_GAP_MS = HOUR;
/** One retry after a failed background check (offline at launch, mid-release artifacts). */
const RETRY_AFTER_FAILURE_MS = 5 * 60_000;
/** A manual check that answers in 80 ms reads as "did it even run?"; hold the state briefly. */
const MANUAL_CHECK_MIN_MS = 600;
/** Where "What's new" and "All releases" go. */
export const RELEASES_URL = "https://github.com/BetaBots-LLC/callimachus/releases";
export const releaseUrl = (version: string) => `${RELEASES_URL}/tag/v${version}`;
/** The one toast about the pending release; updated in place as the install proceeds. */
const UPDATE_TOAST_ID = "app-update";
const LAST_VERSION_KEY = "cal:last-version";
const DISMISSED_KEY = "cal:update-dismissed";

// The plugin's update handle holds a backend resource; it lives outside the store so a
// re-render never clones it and `close()` releases the previous one when a newer release
// replaces it.
let handle: Update | null = null;
/** The check in flight, so a manual check can wait for it instead of being skipped. */
let inflight: Promise<void> | null = null;
let retryTimer: ReturnType<typeof setTimeout> | undefined;
let retried = false;
let lastAttemptAt = 0;
let started = false;
/** Whether the release toast is on screen (so install progress updates it, not a ghost). */
let toastOpen = false;

function toInfo(u: Update): UpdateInfo {
  return { version: u.version, notes: u.body?.trim() || null, date: u.date ?? null };
}

function readStorage(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}
function writeStorage(key: string, value: string) {
  try {
    localStorage.setItem(key, value);
  } catch {
    /* private mode or full storage: the preference just doesn't persist */
  }
}

/** A readable reason for a failed check. Raw messages are reqwest/serde errors. */
export function describeCheckError(e: unknown): string {
  const raw = errorText(e);
  if (/__TAURI_INTERNALS__|not a function|undefined/i.test(raw) && !/http/i.test(raw)) {
    return "Updates are only available in the desktop app.";
  }
  if (/could not fetch a valid release json|release not found|No such file|not found/i.test(raw)) {
    return "No release manifest was found for this build. Development builds can't update themselves.";
  }
  if (/dns|resolve|connect|network|timed? ?out|offline|certificate|sending request/i.test(raw)) {
    return "Couldn't reach GitHub. Check your internet connection and try again.";
  }
  if (/signature|minisign|public key/i.test(raw)) {
    return "The release's signature didn't verify, so it was not installed.";
  }
  return raw;
}

function delay(ms: number) {
  return new Promise<void>((resolve) => setTimeout(resolve, ms));
}

async function closeHandle(h: Update | null) {
  await h?.close().catch(() => {});
}

function closeReleaseToast() {
  if (!toastOpen) return;
  toastOpen = false;
  notify.close(UPDATE_TOAST_ID);
}

function showReleaseToast(version: string) {
  toastOpen = true;
  notify.info(`Callimachus ${version} is available`, {
    id: UPDATE_TOAST_ID,
    description: "Install now, or later from Settings › Updates.",
    timeout: 0,
    // Stays open: install() turns this toast into the progress toast.
    action: {
      label: "Install",
      keepOpen: true,
      onClick: () => void useUpdater.getState().install(),
    },
    onClose: () => {
      const wasOpen = toastOpen;
      toastOpen = false;
      // Only the X reaches here with the status still `available`; the Install action has
      // already moved it on. That X means "not now".
      if (wasOpen && useUpdater.getState().status === "available") useUpdater.getState().dismiss();
    },
  });
}

/** One check round, shared by background and manual callers. */
async function runCheck(
  manual: boolean,
  set: (p: Partial<UpdaterState>) => void,
  get: () => UpdaterState,
) {
  lastAttemptAt = Date.now();
  clearTimeout(retryTimer);
  try {
    const [found] = await Promise.all([
      checkForUpdate(),
      manual ? delay(MANUAL_CHECK_MIN_MS) : undefined,
    ]);
    retried = false;
    // An install started while this check was out: nothing here may touch it.
    const now = get().status;
    if (now === "downloading" || now === "installing" || now === "installed") {
      await closeHandle(found);
      return;
    }
    set({ lastCheckedAt: Date.now() });
    if (!found) {
      // Up to date (or a release was pulled): drop what we held and its toast.
      const old = handle;
      handle = null;
      await closeHandle(old);
      closeReleaseToast();
      set({ status: "idle", update: null, error: null });
      return;
    }
    if (found.version === handle?.version) {
      // Same release as last time: keep the handle we have (the toast is already up, or
      // was dismissed), just make sure the state says so.
      await closeHandle(found);
      set({ status: "available", error: null });
      return;
    }
    const old = handle;
    handle = found;
    await closeHandle(old);
    set({ status: "available", update: toInfo(found), error: null });
    // A manual check is answered by the Updates section (the caller is looking at it, or is
    // about to be); the toast is for finding out while doing something else, and stays
    // quiet for a version the user already said "Later" to.
    if (!manual && readStorage(DISMISSED_KEY) !== found.version) showReleaseToast(found.version);
  } catch (e) {
    const reason = describeCheckError(e);
    if (manual) {
      set({ status: handle ? "available" : "idle", error: reason });
    } else if (!retried) {
      retried = true;
      retryTimer = setTimeout(() => void get().check(), RETRY_AFTER_FAILURE_MS);
    }
  }
}

export const useUpdater = create<UpdaterState>((set, get) => ({
  status: "idle",
  currentVersion: null,
  update: null,
  pct: 0,
  error: null,
  lastCheckedAt: null,

  check: async ({ manual = false } = {}) => {
    const { status } = get();
    if (status === "downloading" || status === "installing" || status === "installed") return;
    if (inflight) {
      // A background check is out. Background callers are satisfied by it; a manual one
      // waits for it, then runs its own round so it always answers.
      if (!manual) return;
      set({ status: "checking", error: null });
      await inflight;
    }
    if (manual) set({ status: "checking", error: null });
    const round = runCheck(manual, set, get);
    inflight = round;
    try {
      await round;
    } finally {
      if (inflight === round) inflight = null;
    }
  },

  install: async () => {
    const { status, update } = get();
    const h = handle; // a check finishing mid-install must not swap it out from under us
    if (!h || !update || status === "downloading" || status === "installing") return;
    set({ status: "downloading", pct: 0, error: null });
    if (toastOpen) {
      notify.update(UPDATE_TOAST_ID, {
        type: "loading",
        title: `Downloading Callimachus ${update.version}`,
        description: "Starting…",
        timeout: 0,
        action: null,
      });
    }
    let contentLength = 0;
    let downloaded = 0;
    try {
      await h.download((event) => {
        switch (event.event) {
          case "Started":
            contentLength = event.data.contentLength ?? 0;
            break;
          case "Progress": {
            downloaded += event.data.chunkLength;
            if (contentLength > 0) {
              const pct = Math.min(100, Math.round((downloaded / contentLength) * 100));
              if (pct !== get().pct) {
                set({ pct });
                if (toastOpen)
                  notify.update(UPDATE_TOAST_ID, { description: `${pct}% downloaded` });
              }
            }
            break;
          }
          case "Finished":
            set({ pct: 100 });
            break;
        }
      });
      set({ status: "installing", pct: 100 });
      if (toastOpen) {
        notify.update(UPDATE_TOAST_ID, {
          title: `Installing Callimachus ${update.version}`,
          description: "Callimachus restarts in a moment.",
        });
      }
      // Send buffered analytics before install, not after: on Windows install() launches
      // the installer and exits the process (no RunEvent::Exit, so the backend's exit
      // flush never runs), and on macOS/Linux relaunch() skips that flush too. Bounded to
      // ~1.5 s, a no-op when analytics are off. update_installed itself is reported by
      // the backend on the next launch.
      await flushTelemetry();
      await h.install();
    } catch (e) {
      console.error("update failed:", e);
      track("error_shown", { kind: "update_failed" });
      const reason = errorText(e);
      set({ status: "available", pct: 0, error: `The update couldn't be installed: ${reason}` });
      const action = {
        label: "Details",
        onClick: () => useUi.getState().openSettings("updates"),
      };
      if (toastOpen) {
        notify.update(UPDATE_TOAST_ID, {
          type: "error",
          title: "The update couldn't be installed",
          description: reason,
          action,
        });
        toastOpen = false; // it now closes like any error toast
      } else {
        notify.error("The update couldn't be installed", { description: reason, action });
      }
      return;
    }
    // The new build is in place (on Windows the installer has taken over by now). A failed
    // relaunch must not read as a failed install: a second click would download it again.
    try {
      await relaunch();
    } catch (e) {
      console.error("relaunch failed:", e);
      set({ status: "installed", error: null });
      const description = "Quit and reopen Callimachus to finish the update.";
      if (toastOpen) {
        notify.update(UPDATE_TOAST_ID, { type: "success", title: "Update installed", description });
        toastOpen = false;
      } else {
        notify.success("Update installed", { description });
      }
    }
  },

  dismiss: () => {
    const v = get().update?.version;
    if (v) writeStorage(DISMISSED_KEY, v);
    closeReleaseToast();
  },
}));

/** First launch of a new build: say so, with a way to see what changed. */
function announceUpgrade(version: string) {
  const previous = readStorage(LAST_VERSION_KEY);
  writeStorage(LAST_VERSION_KEY, version);
  if (!previous || previous === version) return;
  // notify queues until the Toaster has mounted, so this survives running at launch.
  notify.success(`Updated to Callimachus ${version}`, {
    timeout: 8_000,
    action: {
      label: "What's new",
      onClick: () => void openUrl(releaseUrl(version)).catch(() => {}),
    },
  });
}

/**
 * Start the background checks. Idempotent; called once from main.tsx (not from an
 * effect, so StrictMode's double mount in dev can't register two schedules).
 */
export function startUpdater() {
  if (started) return;
  started = true;
  void getVersion()
    .then((v) => {
      useUpdater.setState({ currentVersion: v });
      announceUpgrade(v);
    })
    .catch(() => {
      /* not inside Tauri (browser dev): the Updates section shows "dev" */
    });

  const { check } = useUpdater.getState();
  void check();
  setInterval(() => void check(), CHECK_INTERVAL_MS);
  const onFocusOrVisible = () => {
    if (document.visibilityState === "hidden") return;
    if (Date.now() - lastAttemptAt >= FOCUS_CHECK_MIN_GAP_MS) void check();
  };
  window.addEventListener("focus", onFocusOrVisible);
  document.addEventListener("visibilitychange", onFocusOrVisible);
}
