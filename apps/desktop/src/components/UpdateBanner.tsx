import { useEffect, useRef, useState } from "react";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { Button } from "@/components/ui/button";
import { flushTelemetry, track } from "../lib/telemetry";

type Phase = "idle" | "downloading" | "installing";

const HOUR = 60 * 60_000;
/** Background re-check cadence: with close-to-tray the app can run for weeks. */
const CHECK_INTERVAL_MS = 6 * HOUR;
/** Window focus / restore-from-tray re-checks at most this often. */
const FOCUS_CHECK_MIN_GAP_MS = HOUR;
/** One retry after a failed check (offline at launch, or mid-release artifacts). */
const RETRY_AFTER_FAILURE_MS = 5 * 60_000;

/**
 * Auto-updater UI. Follows the Tauri v2 updater guide: `check()`, then on the user's go
 * `update.downloadAndInstall(...)` with the Started/Progress/Finished callback, then
 * `relaunch()`. Install is gated behind a click (a restart is disruptive); a failed check
 * (offline, or a dev build with no signed updater artifacts) stays silent.
 *
 * Checks run at startup, every 6 hours, and when the window regains focus or becomes
 * visible again (e.g. restored from the tray) if the last check is over an hour old.
 * A newer release found later replaces the one shown; checks pause while installing.
 */
export function UpdateBanner() {
  const [update, setUpdate] = useState<Update | null>(null);
  const [phase, setPhase] = useState<Phase>("idle");
  const [pct, setPct] = useState(0);
  // "Later" hides this version only; a newer release found later shows the banner again.
  const [dismissedVersion, setDismissedVersion] = useState<string | null>(null);

  // Refs so the timers and listeners (registered once) see current values.
  const updateRef = useRef<Update | null>(null);
  const phaseRef = useRef<Phase>("idle");
  useEffect(() => {
    phaseRef.current = phase;
  }, [phase]);
  const [installError, setInstallError] = useState<string | null>(null);

  useEffect(() => {
    let lastCheckAt = 0;
    let checking = false;
    let retryTimer: ReturnType<typeof setTimeout> | undefined;
    let disposed = false;

    const runCheck = async (isRetry = false) => {
      if (checking || phaseRef.current !== "idle") return;
      checking = true;
      lastCheckAt = Date.now();
      try {
        const found = await check();
        if (disposed) {
          await found?.close().catch(() => {});
          return;
        }
        const current = updateRef.current;
        if (found && found.version !== current?.version) {
          // Release the previous update's backend resource before replacing it.
          await current?.close().catch(() => {});
          updateRef.current = found;
          setUpdate(found);
        } else {
          await found?.close().catch(() => {});
        }
      } catch {
        // Offline / dev build / no artifacts. Retry once a little later.
        if (!isRetry && !disposed) {
          clearTimeout(retryTimer);
          retryTimer = setTimeout(() => void runCheck(true), RETRY_AFTER_FAILURE_MS);
        }
      } finally {
        checking = false;
      }
    };

    const onFocusOrVisible = () => {
      if (document.visibilityState === "hidden") return;
      if (Date.now() - lastCheckAt >= FOCUS_CHECK_MIN_GAP_MS) void runCheck();
    };

    void runCheck();
    const interval = setInterval(() => void runCheck(), CHECK_INTERVAL_MS);
    window.addEventListener("focus", onFocusOrVisible);
    document.addEventListener("visibilitychange", onFocusOrVisible);
    return () => {
      disposed = true;
      clearInterval(interval);
      clearTimeout(retryTimer);
      window.removeEventListener("focus", onFocusOrVisible);
      document.removeEventListener("visibilitychange", onFocusOrVisible);
    };
  }, []);

  const dismissed = update != null && dismissedVersion === update.version;
  if (!update || dismissed) return null;

  const install = async () => {
    setInstallError(null);
    setPhase("downloading");
    let contentLength = 0;
    let downloaded = 0;
    try {
      await update.download((event) => {
        switch (event.event) {
          case "Started":
            contentLength = event.data.contentLength ?? 0;
            break;
          case "Progress":
            downloaded += event.data.chunkLength;
            if (contentLength > 0) setPct(Math.round((downloaded / contentLength) * 100));
            break;
          case "Finished":
            setPct(100);
            setPhase("installing");
            break;
        }
      });
      // Send buffered analytics before install, not after: on Windows install() launches
      // the installer and exits the process (no RunEvent::Exit, so the backend's exit
      // flush never runs), and on macOS/Linux relaunch() skips that flush too. Bounded to
      // ~1.5 s, a no-op when analytics are off. update_installed itself is reported by
      // the backend on the next launch.
      await flushTelemetry();
      await update.install();
      await relaunch();
    } catch (e) {
      console.error("update failed:", e);
      track("error_shown", { kind: "update_failed" });
      setInstallError(`The update couldn't be installed: ${e instanceof Error ? e.message : e}`);
      setPhase("idle");
    }
  };

  return (
    <div className="fixed right-4 bottom-4 z-50 flex w-72 flex-col gap-2 rounded-xl border bg-card p-4 text-card-foreground shadow-lg">
      <div>
        <div className="text-sm font-medium">Update available</div>
        <div className="text-xs text-muted-foreground">
          Version {update.version} is ready to install.
        </div>
      </div>

      {installError && <div className="text-xs text-destructive">{installError}</div>}

      {phase === "idle" ? (
        <div className="flex items-center gap-2">
          <Button size="sm" onClick={install}>
            Install &amp; restart
          </Button>
          <Button size="sm" variant="ghost" onClick={() => setDismissedVersion(update.version)}>
            Later
          </Button>
        </div>
      ) : (
        <div className="space-y-1.5">
          <div className="relative h-1 w-full overflow-hidden rounded-full bg-muted">
            <div
              className="h-full rounded-full bg-primary transition-[width] duration-150"
              style={{ width: `${pct}%` }}
            />
          </div>
          <div className="text-xs text-muted-foreground">
            {phase === "installing" ? "Installing, restarting…" : `Downloading… ${pct}%`}
          </div>
        </div>
      )}
    </div>
  );
}
