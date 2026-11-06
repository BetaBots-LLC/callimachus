import { useId } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Toggle } from "@base-ui/react/toggle";
import { ToggleGroup } from "@base-ui/react/toggle-group";
import { Monitor, Moon, Sun } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import { api } from "../../lib/api";
import { track } from "../../lib/telemetry";
import { errorText, notify } from "../../lib/toast";
import { useChecklist } from "../../store/checklist";
import { type Theme, useTheme } from "../../store/theme";
import { cn } from "@/lib/utils";
import { openPrivacy, useTelemetryChoice, useTelemetryStatus } from "../TelemetryConsent";
import { InlineError, SectionHeader, SettingRow, SettingsGroup } from "./shared";

/** App-level behavior: appearance, the tray, the setup checklist and the opt-in analytics. */
export function GeneralSection() {
  return (
    <>
      <SectionHeader
        title="General"
        description="How Callimachus looks and behaves on this machine."
      />
      <div className="space-y-8">
        <SettingsGroup title="Appearance">
          <ThemeRow />
        </SettingsGroup>
        <SettingsGroup title="Window">
          <CloseToTrayRow />
        </SettingsGroup>
        <SettingsGroup title="Setup">
          <ChecklistRow />
        </SettingsGroup>
        <SettingsGroup title="Privacy">
          <TelemetryRow />
        </SettingsGroup>
      </div>
    </>
  );
}

const THEMES: { id: Theme; label: string; icon: typeof Sun }[] = [
  { id: "light", label: "Light", icon: Sun },
  { id: "dark", label: "Dark", icon: Moon },
  { id: "system", label: "System", icon: Monitor },
];

function ThemeRow() {
  const theme = useTheme((s) => s.theme);
  const setTheme = useTheme((s) => s.setTheme);
  return (
    <SettingRow
      title="Theme"
      control={
        <ToggleGroup
          aria-label="Theme"
          value={[theme]}
          onValueChange={(v) => {
            const next = v[0];
            if (next) setTheme(next);
          }}
          className="inline-flex rounded-lg bg-muted p-0.5"
        >
          {THEMES.map((t) => (
            <Toggle
              key={t.id}
              value={t.id}
              aria-label={t.label}
              className={cn(
                "inline-flex h-7 items-center gap-1.5 rounded-[min(var(--radius-md),12px)] px-2.5 text-[0.8rem] font-medium whitespace-nowrap text-muted-foreground transition-colors outline-none select-none",
                "hover:text-foreground focus-visible:ring-3 focus-visible:ring-ring/50",
                "data-pressed:bg-background data-pressed:text-foreground data-pressed:shadow-sm",
                "[&_svg]:size-3.5 [&_svg]:shrink-0",
              )}
            >
              <t.icon aria-hidden="true" />
              {t.label}
            </Toggle>
          ))}
        </ToggleGroup>
      }
    />
  );
}

function CloseToTrayRow() {
  const id = useId();
  const queryClient = useQueryClient();
  const closeToTray = useQuery({ queryKey: ["close_to_tray"], queryFn: api.getCloseToTray });
  const setCloseToTray = useMutation({
    mutationFn: api.setCloseToTray,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["close_to_tray"] });
      track("setting_changed", { key: "close_to_tray" });
    },
    onError: (e) => notify.error("Couldn't change the tray setting", { description: errorText(e) }),
  });
  return (
    <SettingRow
      htmlFor={id}
      title="Keep running in the system tray"
      description="Closing the window hides Callimachus in the tray instead of quitting, so agents keep their memory and indexing continues. Reopen from the tray icon, or quit from its menu."
      control={
        <Switch
          id={id}
          checked={!!closeToTray.data}
          disabled={closeToTray.isPending || setCloseToTray.isPending}
          onCheckedChange={(v) => setCloseToTray.mutate(v)}
        />
      }
    />
  );
}

/** Reopens the first-run checklist, for a step that was skipped or never finished. */
function ChecklistRow() {
  const show = useChecklist((s) => s.show);
  return (
    <SettingRow
      title="Setup checklist"
      description="Index your history, connect your agents, turn on memory and build semantic search. Reopen it to finish a step you skipped."
      control={
        <Button size="sm" variant="outline" onClick={() => show()}>
          Open setup checklist
        </Button>
      }
    />
  );
}

/** Same query as the consent card, so answering either one updates both. */
function TelemetryRow() {
  const id = useId();
  const status = useTelemetryStatus();
  const choose = useTelemetryChoice();
  const s = status.data;
  // Builds that can't send (source builds, `tauri dev`, env opt-out) can't turn sharing
  // ON, so nobody stores a consent they never saw explained in a real build. Turning it
  // OFF always works: a grant stored by an official build must stay revocable here, or
  // the next normal launch would resume sending after the user tried to stop it.
  // Dev builds stay togglable to exercise the flow.
  const granted = s?.consent === "granted";
  const locked = !s || (!s.available && !import.meta.env.DEV && !granted);

  let detail: string;
  if (s?.envOptOut) {
    detail =
      "Turned off by the DO_NOT_TRACK or CALLIMACHUS_TELEMETRY environment variable, so nothing is sent.";
  } else if (s && !s.available) {
    detail = "This build doesn't include usage analytics, so nothing is sent.";
  } else {
    detail =
      "Anonymous counts, like searches run and threads indexed (rounded into ranges), plus your OS and app version. Never conversations, file paths, prompts, searches or API keys. The cal CLI and the MCP server never send anything.";
  }
  if (granted && s && !s.available && !import.meta.env.DEV) {
    detail +=
      " Sharing is still on for normal launches of the official app; switch it off here to stop that too.";
  }

  return (
    <SettingRow
      htmlFor={id}
      title="Share anonymous usage data"
      description={
        <>
          {detail}{" "}
          <Button
            size="xs"
            variant="link"
            className="h-auto p-0 text-[13px] align-baseline"
            onClick={openPrivacy}
          >
            What's collected
          </Button>
        </>
      }
      control={
        <Switch
          id={id}
          checked={granted}
          disabled={locked || choose.isPending}
          onCheckedChange={(v) => choose.mutate(v)}
        />
      }
    >
      {granted && s?.anonId && (
        <p className="text-[13px] text-muted-foreground">
          Anonymous ID <span className="ml-1 font-mono select-all">{s.anonId}</span>
        </p>
      )}
      {choose.isError && <InlineError>Couldn't save that change. Try again.</InlineError>}
    </SettingRow>
  );
}
