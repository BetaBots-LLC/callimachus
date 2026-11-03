import { ArrowDownToLine, ChevronRight, ExternalLink, RefreshCw } from "lucide-react";
import { openUrl } from "@tauri-apps/plugin-opener";
import { Button } from "@/components/ui/button";
import { Progress } from "@/components/ui/progress";
import { Spinner } from "@/components/ui/spinner";
import { Markdown } from "../LazyMarkdown";
import { IS_WINDOWS } from "../../lib/platform";
import { RELEASES_URL, releaseUrl, useUpdater } from "../../store/updater";
import { InlineError, Note, SectionHeader, SettingRow, SettingsGroup } from "./shared";

function ago(at: number | null): string {
  if (!at) return "not checked yet";
  const s = Math.max(0, Math.round((Date.now() - at) / 1000));
  if (s < 45) return "just now";
  const m = Math.round(s / 60);
  if (m < 60) return `${m} min ago`;
  const h = Math.round(m / 60);
  if (h < 24) return `${h} hour${h === 1 ? "" : "s"} ago`;
  return new Date(at).toLocaleDateString([], { month: "short", day: "numeric" });
}

function releasedOn(date: string | null): string | null {
  if (!date) return null;
  const d = new Date(date);
  if (Number.isNaN(d.getTime())) return null;
  return d.toLocaleDateString([], { year: "numeric", month: "long", day: "numeric" });
}

/** The manifest's notes start with the release heading; the row above already names it. */
function stripHeading(notes: string): string {
  return notes.replace(/^#{1,3}\s+[^\n]*\n+/, "");
}

function openExternal(url: string) {
  void openUrl(url).catch(() => {
    /* no browser available: nothing useful to do */
  });
}

/** Version, check, install: the whole update flow on one screen. State lives in store/updater. */
export function UpdatesSection() {
  const status = useUpdater((s) => s.status);
  const currentVersion = useUpdater((s) => s.currentVersion);
  const update = useUpdater((s) => s.update);
  const pct = useUpdater((s) => s.pct);
  const error = useUpdater((s) => s.error);
  const lastCheckedAt = useUpdater((s) => s.lastCheckedAt);
  const check = useUpdater((s) => s.check);
  const install = useUpdater((s) => s.install);

  const busy = status === "downloading" || status === "installing";
  // On `update`, not the status: a manual re-check must not blank the release row.
  const available = !!update && status !== "installed";
  const released = releasedOn(update?.date ?? null);

  let state: string;
  if (status === "checking") state = "Checking for updates…";
  else if (status === "installed") state = "Updated. Quit and reopen Callimachus to finish.";
  else if (error)
    state = lastCheckedAt ? `Last checked ${ago(lastCheckedAt)}.` : "Not checked yet.";
  else if (available)
    state = `You're on ${currentVersion ?? "this build"}. Checked ${ago(lastCheckedAt)}.`;
  else if (lastCheckedAt) state = `You're up to date. Checked ${ago(lastCheckedAt)}.`;
  else state = "Callimachus checks for new releases on launch and every few hours.";

  return (
    <>
      <SectionHeader
        title="Updates"
        description="New releases are downloaded from GitHub and verified against the app's signing key before anything is installed. Nothing installs until you say so."
      >
        <Button
          size="sm"
          variant="outline"
          onClick={() => openExternal(RELEASES_URL)}
          aria-label="All releases on GitHub"
        >
          All releases
          <ExternalLink data-icon="inline-end" />
        </Button>
      </SectionHeader>

      <div className="space-y-8">
        <SettingsGroup aria-label="Version">
          <SettingRow
            title={
              <span className="inline-flex items-center gap-2">
                Callimachus
                <span className="font-mono text-[13px] font-normal text-muted-foreground tabular-nums">
                  {currentVersion ?? "dev"}
                </span>
              </span>
            }
            description={state}
            control={
              <Button
                size="sm"
                variant="outline"
                onClick={() => void check({ manual: true })}
                disabled={status === "checking" || busy || status === "installed"}
              >
                {status === "checking" ? (
                  <Spinner data-icon="inline-start" />
                ) : (
                  <RefreshCw data-icon="inline-start" />
                )}
                {status === "checking" ? "Checking…" : "Check for updates"}
              </Button>
            }
          >
            {error && <InlineError>{error}</InlineError>}
          </SettingRow>

          {available && update && (
            <SettingRow
              title={`Version ${update.version}`}
              description={released ? `Released ${released}.` : undefined}
              control={
                <Button size="sm" onClick={() => void install()} disabled={busy}>
                  {busy ? (
                    <Spinner data-icon="inline-start" />
                  ) : (
                    <ArrowDownToLine data-icon="inline-start" />
                  )}
                  {status === "downloading"
                    ? "Downloading…"
                    : status === "installing"
                      ? "Installing…"
                      : "Install and restart"}
                </Button>
              }
            >
              {busy && (
                <Progress
                  value={status === "installing" ? null : pct}
                  aria-label="Update download"
                  className="gap-1.5"
                >
                  <span className="sr-only">
                    {status === "installing" ? "Installing" : `${pct}% downloaded`}
                  </span>
                </Progress>
              )}
              {busy && (
                <Note>
                  {status === "installing"
                    ? IS_WINDOWS
                      ? "The installer is running. Callimachus closes and reopens on its own."
                      : "Installing. Callimachus restarts in a moment."
                    : `${pct}% downloaded`}
                </Note>
              )}
              {update.notes && (
                <details className="group/notes rounded-lg border bg-muted/40" open>
                  <summary className="flex cursor-default list-none items-center gap-1.5 px-3 py-2 text-[13px] font-medium select-none [&::-webkit-details-marker]:hidden">
                    <ChevronRight
                      aria-hidden="true"
                      className="size-3.5 text-muted-foreground transition-transform group-open/notes:rotate-90"
                    />
                    What's new
                    <Button
                      size="xs"
                      variant="link"
                      className="ml-auto h-auto p-0 text-[13px] font-normal text-muted-foreground"
                      onClick={(e) => {
                        e.preventDefault();
                        openExternal(releaseUrl(update.version));
                      }}
                    >
                      On GitHub
                    </Button>
                  </summary>
                  <div className="border-t px-3 py-2 text-sm">
                    <Markdown>{stripHeading(update.notes)}</Markdown>
                  </div>
                </details>
              )}
            </SettingRow>
          )}
        </SettingsGroup>
      </div>
    </>
  );
}
