import { useId } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import { Switch } from "@/components/ui/switch";
import { api } from "../../lib/api";
import { track } from "../../lib/telemetry";
import { errorText, notify } from "../../lib/toast";
import {
  CommandText,
  InlineError,
  Note,
  SectionHeader,
  SettingRow,
  SettingsGroup,
  StatePill,
  StatusItem,
  StatusList,
} from "./shared";

/** Connect the agents that read this history: Claude Code fully, the others over MCP. */
export function AgentsSection() {
  return (
    <>
      <SectionHeader
        title="Agents"
        description="Let Claude Code and your other agents search this history and get each repo's memory injected into their sessions."
      />
      <div className="space-y-8">
        <SettingsGroup aria-label="Claude Code">
          <ClaudeCodeRows />
        </SettingsGroup>
        <SettingsGroup title="Other agents">
          <OtherAgentsRow />
        </SettingsGroup>
      </div>
    </>
  );
}

/** Same query everywhere (the data-folder fields read it too), so no extra fetch. */
function useRecallStatus() {
  return useQuery({
    queryKey: ["recall_integration"],
    queryFn: api.recallIntegrationStatus,
  });
}

/** One-click Claude Code integration: install the /recall skill + register this
 *  app as the `callimachus` MCP server, no terminal or cargo. */
function ClaudeCodeRows() {
  const recallId = useId();
  const queryClient = useQueryClient();
  const status = useRecallStatus();
  const refresh = () => queryClient.invalidateQueries({ queryKey: ["recall_integration"] });
  const install = useMutation({
    mutationFn: api.installRecallIntegration,
    onSuccess: () => {
      refresh();
      notify.success("Claude Code connected", {
        description: "Restart Claude Code or run /mcp to pick up the server, then type /recall.",
      });
    },
    onError: (e) => notify.error("Couldn't connect Claude Code", { description: errorText(e) }),
  });
  const uninstall = useMutation({
    mutationFn: api.uninstallRecallIntegration,
    onSuccess: () => {
      refresh();
      // The "Done. Restart Claude Code…" note describes an install that no longer exists.
      install.reset();
      notify.success("Claude Code integration removed");
    },
    onError: (e) =>
      notify.error("Couldn't remove the Claude Code integration", { description: errorText(e) }),
  });
  const setRecall = useMutation({
    mutationFn: api.setProactiveRecall,
    onSuccess: () => {
      refresh();
      track("setting_changed", { key: "proactive_recall" });
    },
    onError: (e) => notify.error("Couldn't change proactive recall", { description: errorText(e) }),
  });

  const s = status.data;
  const connected =
    !!s && s.skillInstalled && s.mcpRegistered && s.hookInstalled && !s.skillOutdated;
  const partial = !!s && (s.skillInstalled || s.mcpRegistered || s.hookInstalled) && !connected;
  // Running from a disk image, App Translocation or an AppImage mount: install would
  // register a path that disappears, so the backend refuses with this same message.
  // `=== false`, not `!`: a status without the field (an older backend) must not lock
  // the install button.
  const unstable = s?.exeStable === false;

  return (
    <>
      <SettingRow
        title={
          <span className="inline-flex items-center gap-2">
            Claude Code
            {connected && <StatePill>connected</StatePill>}
          </span>
        }
        description={
          <>
            Installs the <code>/recall</code> skill, registers Callimachus as an MCP server, and
            adds the <code>cal</code> CLI the VS Code extension uses. Hooks inject each repo's
            memory when a session starts and snapshot a session before its context is compacted.
          </>
        }
        control={
          <>
            {(s?.skillInstalled || s?.mcpRegistered) && (
              <Button
                size="sm"
                variant="ghost"
                onClick={() => uninstall.mutate()}
                disabled={uninstall.isPending || install.isPending}
              >
                {uninstall.isPending && <Spinner data-icon="inline-start" />}
                {uninstall.isPending ? "Removing…" : "Remove"}
              </Button>
            )}
            <Button
              size="sm"
              variant={connected ? "outline" : "default"}
              onClick={() => install.mutate()}
              disabled={install.isPending || unstable || status.isPending}
            >
              {install.isPending && <Spinner data-icon="inline-start" />}
              {install.isPending
                ? "Installing…"
                : connected
                  ? "Reinstall"
                  : partial
                    ? "Finish setup"
                    : "Enable for Claude Code"}
            </Button>
          </>
        }
      >
        {status.isPending ? (
          <Note>Checking what's installed…</Note>
        ) : status.isError ? (
          <InlineError>Couldn't read the Claude Code setup: {errorText(status.error)}</InlineError>
        ) : (
          // Only when something needs attention: when all is well the "connected" pill says so.
          (!connected || s?.calProblem) && (
            <StatusList>
              <StatusItem
                state={s?.skillInstalled ? (s.skillOutdated ? "warning" : "ok") : "missing"}
              >
                {s?.skillOutdated ? "skill outdated" : "skill"}
              </StatusItem>
              <StatusItem state={s?.mcpRegistered ? "ok" : "missing"}>MCP server</StatusItem>
              <StatusItem state={s?.hookInstalled ? "ok" : "missing"}>memory hook</StatusItem>
              <StatusItem state={s?.calInstalled ? "ok" : s?.calProblem ? "warning" : "missing"}>
                cal CLI
              </StatusItem>
            </StatusList>
          )
        )}
        {s?.calProblem && <Note>{s.calProblem}</Note>}
        {s?.pathHint && <PathHint hint={s.pathHint} />}
        {unstable && s?.exeProblem && <InlineError>{s.exeProblem}</InlineError>}
        {install.isSuccess && (
          <Note>
            Done. Restart Claude Code (or run <code>/mcp</code>) to pick up the server, then type{" "}
            <code>/recall</code>.
          </Note>
        )}
        {install.isError && (
          <InlineError>Couldn't connect Claude Code: {errorText(install.error)}</InlineError>
        )}
        {uninstall.isError && (
          <InlineError>Couldn't remove the integration: {errorText(uninstall.error)}</InlineError>
        )}
      </SettingRow>

      {connected && (
        <SettingRow
          htmlFor={recallId}
          title="Proactive recall"
          description="Before each prompt, silently tell Claude when you've solved it in a past session. Reads every prompt (a brief on-device lookup)."
          control={
            <Switch
              id={recallId}
              checked={!!s?.proactiveRecallInstalled}
              disabled={setRecall.isPending}
              onCheckedChange={(v) => setRecall.mutate(v)}
            />
          }
        >
          {setRecall.isError && (
            <InlineError>
              Couldn't change proactive recall: {errorText(setRecall.error)}
            </InlineError>
          )}
        </SettingRow>
      )}
    </>
  );
}

/** Register the `callimachus` MCP server with the OTHER detected agents (Codex / Cursor /
 *  Gemini), so they can search the history too. Only touches agents installed on this machine. */
function OtherAgentsRow() {
  const queryClient = useQueryClient();
  const status = useQuery({
    queryKey: ["agent_integrations"],
    queryFn: api.agentIntegrationsStatus,
  });
  const refresh = () => queryClient.invalidateQueries({ queryKey: ["agent_integrations"] });
  const install = useMutation({
    mutationFn: api.installAgentIntegrations,
    onSuccess: () => {
      refresh();
      notify.success("Registered with your other agents", {
        description: "Restart them or reload their MCP config to pick up the server.",
      });
    },
    onError: (e) =>
      notify.error("Couldn't register with your other agents", { description: errorText(e) }),
  });
  const uninstall = useMutation({
    mutationFn: api.uninstallAgentIntegrations,
    onSuccess: () => {
      refresh();
      // The "Done. Restart the agent…" note describes a registration that's gone now.
      install.reset();
      notify.success("Registration removed");
    },
    onError: (e) => notify.error("Couldn't remove the registration", { description: errorText(e) }),
  });
  // Registering other agents refuses from an unstable app path too; the Claude Code status
  // (same query key as that row, so no extra fetch) carries the reason.
  const recall = useRecallStatus();
  const exeProblem = recall.data?.exeStable === false ? recall.data.exeProblem : null;

  const present = (status.data ?? []).filter((a) => a.present);
  const anyRegistered = present.some((a) => a.registered);
  const allRegistered = present.length > 0 && present.every((a) => a.registered);

  if (status.isPending) {
    return (
      <SettingRow title="Detected on this machine">
        <Note>Looking for Codex, Cursor and Gemini CLI…</Note>
      </SettingRow>
    );
  }
  if (status.isError) {
    return (
      <SettingRow title="Detected on this machine">
        <InlineError>Couldn't check for other agents: {errorText(status.error)}</InlineError>
      </SettingRow>
    );
  }
  if (present.length === 0) {
    return (
      <SettingRow
        title="Detected on this machine"
        description="No other agents detected (Codex, Cursor or Gemini CLI)."
      />
    );
  }

  return (
    <SettingRow
      title={
        <span className="inline-flex items-center gap-2">
          Detected on this machine
          {allRegistered && <StatePill>connected</StatePill>}
        </span>
      }
      description={
        <>
          Register the <code>callimachus</code> MCP server with them so they can search your history
          too. Only agents installed on this machine are touched.
        </>
      }
      control={
        <>
          {anyRegistered && (
            <Button
              size="sm"
              variant="ghost"
              onClick={() => uninstall.mutate()}
              disabled={uninstall.isPending || install.isPending}
            >
              {uninstall.isPending && <Spinner data-icon="inline-start" />}
              {uninstall.isPending ? "Removing…" : "Remove"}
            </Button>
          )}
          <Button
            size="sm"
            variant={allRegistered ? "outline" : "default"}
            onClick={() => install.mutate()}
            disabled={install.isPending || !!exeProblem}
          >
            {install.isPending && <Spinner data-icon="inline-start" />}
            {install.isPending
              ? "Registering…"
              : allRegistered
                ? "Re-register"
                : "Register with detected agents"}
          </Button>
        </>
      }
    >
      {/* The per-agent list only while one is missing; the pill covers "all registered". */}
      {!allRegistered && (
        <StatusList>
          {present.map((a) => (
            <StatusItem key={a.id} state={a.registered ? "ok" : "missing"}>
              {a.label}
            </StatusItem>
          ))}
        </StatusList>
      )}
      {exeProblem && <InlineError>{exeProblem}</InlineError>}
      {install.isSuccess && (
        <Note>Done. Restart the agent (or reload its MCP config) to pick up the server.</Note>
      )}
      {install.isError && (
        <InlineError>
          Couldn't register with your other agents: {errorText(install.error)}
        </InlineError>
      )}
      {uninstall.isError && (
        <InlineError>Couldn't remove the registration: {errorText(uninstall.error)}</InlineError>
      )}
    </SettingRow>
  );
}

/**
 * The backend's PATH advice for `cal`, with its trailing `export PATH=...` line set apart
 * as a selectable command so it can be copied into a shell profile in one go.
 */
function PathHint({ hint }: { hint: string }) {
  const at = hint.indexOf("export PATH=");
  const prose = at >= 0 ? hint.slice(0, at).trimEnd() : hint;
  const command = at >= 0 ? hint.slice(at).trim() : null;
  return (
    <div className="space-y-1.5">
      {prose && <Note>{prose}</Note>}
      {command && <CommandText>{command}</CommandText>}
    </div>
  );
}
