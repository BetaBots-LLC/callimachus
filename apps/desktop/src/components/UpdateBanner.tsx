import { useEffect, useState } from "react";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { Button } from "@/components/ui/button";

type Phase = "idle" | "downloading" | "installing";

/**
 * Auto-updater UI. Follows the Tauri v2 updater guide exactly: `check()` on startup, then on
 * the user's go `update.downloadAndInstall(...)` with the Started/Progress/Finished callback,
 * then `relaunch()`. Install is gated behind a click (a restart is disruptive); the check
 * itself fails silently when offline or in a dev build (no signed updater artifacts).
 */
export function UpdateBanner() {
  const [update, setUpdate] = useState<Update | null>(null);
  const [phase, setPhase] = useState<Phase>("idle");
  const [pct, setPct] = useState(0);
  const [dismissed, setDismissed] = useState(false);

  useEffect(() => {
    check()
      .then((u) => {
        if (u) setUpdate(u);
      })
      .catch(() => {
        /* offline / dev build / no artifacts — nothing to do */
      });
  }, []);

  if (!update || dismissed) return null;

  const install = async () => {
    setPhase("downloading");
    let contentLength = 0;
    let downloaded = 0;
    try {
      await update.downloadAndInstall((event) => {
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
      await relaunch();
    } catch (e) {
      console.error("update failed:", e);
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

      {phase === "idle" ? (
        <div className="flex items-center gap-2">
          <Button size="sm" onClick={install}>
            Install &amp; restart
          </Button>
          <Button size="sm" variant="ghost" onClick={() => setDismissed(true)}>
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
