import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { openUrl } from "@tauri-apps/plugin-opener";
import { X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { api } from "../lib/api";
import { PRIVACY_URL, setTelemetry, telemetryState, type TelemetryStatus } from "../lib/telemetry";
import { useChecklist } from "../store/checklist";
import { useUi } from "../store/ui";

/** Shared by the card and the Settings switch so both always show the same answer. */
const TELEMETRY_KEY = ["telemetry_state"] as const;

// DEV-only preview of the consent card (dev builds have no token, so it is otherwise
// hidden): devtools `localStorage.setItem("cal:telemetry-card", "1")`, then reload.
function previewForced(): boolean {
  if (!import.meta.env.DEV) return false;
  try {
    return localStorage.getItem("cal:telemetry-card") === "1";
  } catch {
    return false;
  }
}

export function useTelemetryStatus() {
  return useQuery({
    queryKey: TELEMETRY_KEY,
    queryFn: telemetryState,
    staleTime: Infinity,
    // Another running instance can revoke consent; re-read it when this window regains
    // focus so the switch doesn't keep showing the old choice.
    refetchOnWindowFocus: "always",
  });
}

export function useTelemetryChoice() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: setTelemetry,
    onSuccess: (status: TelemetryStatus) => qc.setQueryData(TELEMETRY_KEY, status),
  });
}

/** Whether to ask at all: only while no choice has been made and only in builds that can
 *  send (or the DEV preview). */
function useShouldAsk(): boolean {
  const s = useTelemetryStatus().data;
  return previewForced() || (!!s && s.available && s.consent === "unset");
}

export function openPrivacy() {
  void openUrl(PRIVACY_URL).catch(() => {
    /* no browser available: nothing useful to do */
  });
}

/**
 * App-level, opt-in consent prompt for anonymous usage data. Rendered next to the
 * update banner, independent of view and thread count, so upgraded users with an
 * existing index see it too (the setup checklist only opens by itself on an empty index).
 * Shown only while no choice has been made and only in builds that can send at all. Until
 * the user clicks Allow, nothing is queued or sent. Closing it with the X just defers the
 * question to the next launch.
 */
export function TelemetryConsent() {
  const choose = useTelemetryChoice();
  const [deferred, setDeferred] = useState(false);
  // Settings › General carries the same switch in place, so the card would only cover
  // the section list there.
  const view = useUi((st) => st.view);
  // The checklist asks in its footer (TelemetryConsentLine). On first launch the card sat
  // fixed over the checklist's lower steps, so it steps aside while the checklist is on
  // screen: open (or forced by an empty index, App.tsx) and the Search view showing. A
  // first-run user who spends the session in another tab still gets asked there.
  const checklistOpen = useChecklist((st) => st.open);
  const { data: stats } = useQuery({ queryKey: ["db_stats"], queryFn: api.dbStats });
  const checklistShown = view === "search" && (checklistOpen || stats?.threads === 0);

  const shouldAsk = useShouldAsk();
  if (deferred || view === "settings" || checklistShown || !shouldAsk) return null;

  return (
    <section
      aria-labelledby="telemetry-consent-title"
      className="fixed bottom-4 left-4 z-50 flex w-80 flex-col gap-3 rounded-xl border bg-card p-4 text-card-foreground shadow-lg"
    >
      <div className="flex items-start justify-between gap-2">
        <h2 id="telemetry-consent-title" className="text-sm font-medium">
          Help improve Callimachus?
        </h2>
        <Button
          size="icon-xs"
          variant="ghost"
          aria-label="Decide later"
          title="Decide later"
          className="-mt-1 -mr-1 text-muted-foreground"
          onClick={() => setDeferred(true)}
        >
          <X />
        </Button>
      </div>
      <p className="text-[13px] leading-normal text-muted-foreground">
        Share anonymous usage data, like which features get used and whether indexing succeeds.
        Never your conversations, file paths, prompts, searches or API keys.
      </p>
      {choose.isError && (
        <p role="alert" className="text-xs text-destructive">
          Couldn't save your choice. You can set it in Settings.
        </p>
      )}
      <div className="flex items-center gap-2">
        <Button size="sm" disabled={choose.isPending} onClick={() => choose.mutate(true)}>
          Allow
        </Button>
        <Button
          size="sm"
          variant="ghost"
          disabled={choose.isPending}
          onClick={() => choose.mutate(false)}
        >
          No thanks
        </Button>
        <Button size="sm" variant="link" className="ml-auto px-0" onClick={openPrivacy}>
          Learn more
        </Button>
      </div>
      <p className="text-[11px] text-muted-foreground">You can change this anytime in Settings.</p>
    </section>
  );
}

/**
 * The same question as one line, for the setup checklist's footer. Allow and No record the
 * choice exactly as the card does; leaving it unanswered asks again in the card once the
 * checklist closes.
 */
export function TelemetryConsentLine() {
  const choose = useTelemetryChoice();
  if (!useShouldAsk()) return null;
  return (
    <section
      aria-label="Anonymous usage data"
      className="flex flex-wrap items-center justify-center gap-x-2 gap-y-1 text-xs text-muted-foreground"
    >
      <span>Share anonymous usage data? Never your conversations.</span>
      <span className="inline-flex items-center gap-1">
        <Button
          size="xs"
          variant="outline"
          disabled={choose.isPending}
          onClick={() => choose.mutate(true)}
        >
          Allow
        </Button>
        <Button
          size="xs"
          variant="ghost"
          disabled={choose.isPending}
          onClick={() => choose.mutate(false)}
        >
          No
        </Button>
        <Button size="xs" variant="link" className="px-1" onClick={openPrivacy}>
          What's collected
        </Button>
      </span>
      {choose.isError && (
        <span role="alert" className="w-full text-destructive">
          Couldn't save your choice. Set it in Settings &gt; General.
        </span>
      )}
    </section>
  );
}
