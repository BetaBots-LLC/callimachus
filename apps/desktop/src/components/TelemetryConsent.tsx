import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { openUrl } from "@tauri-apps/plugin-opener";
import { X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { PRIVACY_URL, setTelemetry, telemetryState, type TelemetryStatus } from "../lib/telemetry";
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

export function openPrivacy() {
  void openUrl(PRIVACY_URL).catch(() => {
    /* no browser available: nothing useful to do */
  });
}

/**
 * App-level, opt-in consent prompt for anonymous usage data. Rendered next to the
 * update banner, independent of view and thread count, so upgraded users with an
 * existing index see it too (Onboarding never renders for them). Shown only while no
 * choice has been made and only in builds that can send at all. Until the user clicks
 * Allow, nothing is queued or sent. Closing it with the X just defers the question to
 * the next launch.
 */
export function TelemetryConsent() {
  const status = useTelemetryStatus();
  const choose = useTelemetryChoice();
  const [deferred, setDeferred] = useState(false);
  // Settings › General carries the same switch in place, so the card would only cover
  // the section list there.
  const inSettings = useUi((st) => st.view === "settings");

  const s = status.data;
  const shouldAsk = previewForced() || (!!s && s.available && s.consent === "unset");
  if (deferred || inSettings || !shouldAsk) return null;

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
