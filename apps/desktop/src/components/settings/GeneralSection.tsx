import { useId, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Toggle } from "@base-ui/react/toggle";
import { ToggleGroup } from "@base-ui/react/toggle-group";
import { Monitor, Moon, Sun } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { api } from "../../lib/api";
import { copyAsyncText } from "../../lib/clipboard";
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
        <TerminalGroup />
        <SettingsGroup title="Setup">
          <ChecklistRow />
        </SettingsGroup>
        <SettingsGroup title="Privacy">
          <TelemetryRow />
        </SettingsGroup>
        <SettingsGroup title="Support">
          <DiagnosticsRow />
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

/** Stands for "no choice stored": the Select needs a value for the default too. */
const DEFAULT_TERMINAL = "default";

/** Which terminal Resume and "Open in CLI" open the agent in. Only where there is a choice:
 *  a Mac with a terminal besides Terminal.app installed (Linux reads `$TERMINAL`, Windows
 *  opens a console). */
function TerminalGroup() {
  const id = useId();
  const queryClient = useQueryClient();
  const terminal = useQuery({ queryKey: ["cli_terminal"], queryFn: api.getCliTerminal });
  const setTerminal = useMutation({
    mutationFn: api.setCliTerminal,
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["cli_terminal"] }),
    onError: (e) => notify.error("Couldn't change the terminal", { description: errorText(e) }),
  });
  const choices = terminal.data?.choices ?? [];
  if (choices.length < 2) return null;
  // The stored one, unless it was uninstalled since: Resume then uses Terminal, so say so.
  const selected = choices.some((c) => c.id === terminal.data?.selected)
    ? (terminal.data?.selected ?? DEFAULT_TERMINAL)
    : DEFAULT_TERMINAL;
  const items = choices.map((c) => ({
    value: c.id === "terminal" ? DEFAULT_TERMINAL : c.id,
    label: c.label,
  }));
  return (
    <SettingsGroup title="Terminal">
      <SettingRow
        htmlFor={id}
        title="Open agents in"
        description="Where Resume and Open in CLI start the agent. A new window opens in the session's folder."
        control={
          <Select
            items={items}
            value={selected === "terminal" ? DEFAULT_TERMINAL : selected}
            onValueChange={(v) => {
              const next = !v || v === DEFAULT_TERMINAL ? null : v;
              if (next === (terminal.data?.selected ?? null)) return;
              setTerminal.mutate(next);
            }}
          >
            <SelectTrigger id={id} className="w-40" disabled={setTerminal.isPending}>
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {items.map((t) => (
                <SelectItem key={t.value} value={t.value}>
                  {t.label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        }
      />
    </SettingsGroup>
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

/** Copies `cal doctor`'s report as plain text, as the CLI prints it: the bug report form's
 *  field wraps what is pasted in its own code block (`render: text`), and a fence of ours
 *  inside it would close that block at once. Started straight from the click: WebKit allows
 *  clipboard writes only during the gesture, so the report goes to the clipboard as a
 *  promise (lib/clipboard). */
function DiagnosticsRow() {
  const [state, setState] = useState<"idle" | "copying" | "copied">("idle");
  const [error, setError] = useState<string | null>(null);
  const copy = () => {
    if (state === "copying") return;
    setError(null);
    setState("copying");
    copyAsyncText(() => api.diagnostics().then((d) => d.text)).then(
      () => {
        setState("copied");
        setTimeout(() => setState("idle"), 1500);
      },
      (e) => {
        setState("idle");
        setError(`Couldn't copy the diagnostics: ${errorText(e)}`);
      },
    );
  };
  return (
    <SettingRow
      title="Diagnostics"
      description="The report cal doctor prints: the index and its schema, each agent's hooks and MCP entry, hooks that gave up this week and parsing health. Paste it into a bug report. Paths show your home folder as ~, and it holds no project text or prompts."
      control={
        <Button size="sm" variant="outline" onClick={copy} disabled={state === "copying"}>
          {state === "copied" ? "Copied" : state === "copying" ? "Copying..." : "Copy diagnostics"}
        </Button>
      }
    >
      {error && <InlineError>{error}</InlineError>}
    </SettingRow>
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
