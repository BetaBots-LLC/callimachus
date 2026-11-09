import { useId } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import { Switch } from "@/components/ui/switch";
import {
  type AgentHooksStatus,
  api,
  type HarvestSettings,
  type HookAgentId,
  type HookSettings,
} from "../../lib/api";
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
          <AgentHooksRows />
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
  const errorRecallId = useId();
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
  const setErrorRecall = useMutation({
    mutationFn: api.setErrorRecall,
    onSuccess: () => {
      refresh();
      track("setting_changed", { key: "error_recall" });
    },
    onError: (e) => notify.error("Couldn't change error playbooks", { description: errorText(e) }),
  });

  const s = status.data;
  // The Claude Code plugin brings the skill, the MCP server and the hooks; the app adds only
  // the `cal` CLI those hooks run, so `cal` alone means connected.
  const managed = !!s?.pluginManaged;
  const connected =
    !!s &&
    (managed
      ? s.calInstalled
      : s.skillInstalled && s.mcpRegistered && s.hookInstalled && !s.skillOutdated);
  const partial =
    !!s && !managed && (s.skillInstalled || s.mcpRegistered || s.hookInstalled) && !connected;
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
          managed ? (
            <>
              Managed by the Claude Code plugin, which provides the <code>/callimachus:recall</code>{" "}
              skill, the MCP server and the memory hooks. Callimachus installs only the{" "}
              <code>cal</code> CLI those hooks run, so memory is never injected twice.
            </>
          ) : (
            <>
              Installs the <code>/recall</code> skill, registers Callimachus as an MCP server, and
              adds the <code>cal</code> CLI the VS Code extension uses. Hooks inject each repo's
              memory when a session starts, snapshot a session before its context is compacted, and,
              when auto-distill is on, queue a session that ends for distillation.
            </>
          )
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
                  : managed
                    ? "Install the cal CLI"
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
              {!managed && (
                <>
                  <StatusItem
                    state={s?.skillInstalled ? (s.skillOutdated ? "warning" : "ok") : "missing"}
                  >
                    {s?.skillOutdated ? "skill outdated" : "skill"}
                  </StatusItem>
                  <StatusItem state={s?.mcpRegistered ? "ok" : "missing"}>MCP server</StatusItem>
                  <StatusItem state={s?.hookInstalled ? "ok" : "missing"}>memory hook</StatusItem>
                </>
              )}
              <StatusItem state={s?.calInstalled ? "ok" : s?.calProblem ? "warning" : "missing"}>
                cal CLI
              </StatusItem>
            </StatusList>
          )
        )}
        {s?.calProblem && <Note>{s.calProblem}</Note>}
        {s?.pathHint && <PathHint hint={s.pathHint} />}
        {unstable && s?.exeProblem && <InlineError>{s.exeProblem}</InlineError>}
        {install.isSuccess &&
          (managed ? (
            <Note>Done. New Claude Code sessions get each repo's memory from the plugin.</Note>
          ) : (
            <Note>
              Done. Restart Claude Code (or run <code>/mcp</code>) to pick up the server, then type{" "}
              <code>/recall</code>.
            </Note>
          ))}
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
      {connected && (
        <SettingRow
          htmlFor={errorRecallId}
          title="Error playbooks"
          description="When a shell command Claude runs fails with an error a past commit in this repo fixed, tell Claude that fix, once per session. Runs after every shell command: output without an error costs a few milliseconds, an error one indexed lookup."
          control={
            <Switch
              id={errorRecallId}
              checked={!!s?.errorRecallInstalled}
              disabled={setErrorRecall.isPending}
              onCheckedChange={(v) => setErrorRecall.mutate(v)}
            />
          }
        >
          {setErrorRecall.isError && (
            <InlineError>
              Couldn't change error playbooks: {errorText(setErrorRecall.error)}
            </InlineError>
          )}
        </SettingRow>
      )}
      {connected && <DigestRow />}
      <HarvestRow />
    </>
  );
}

/** The SessionStart hook's recent-sessions digest, for repos nothing has been distilled
 *  for yet. Only meaningful once the hook is installed, so it shows with the other
 *  connected-only rows. */
function DigestRow() {
  const id = useId();
  const qc = useQueryClient();
  const hook = useQuery({ queryKey: ["hook_settings"], queryFn: api.hookSettings });
  const set = useMutation({
    mutationFn: api.setHookDigest,
    onMutate: (on: boolean) =>
      qc.setQueryData<HookSettings>(["hook_settings"], (prev) =>
        prev ? { ...prev, digestEnabled: on } : prev,
      ),
    onSettled: () => qc.invalidateQueries({ queryKey: ["hook_settings"] }),
  });
  const muted = hook.data?.mutedProjects.length ?? 0;
  return (
    <SettingRow
      htmlFor={id}
      title="Session digest"
      description="Inject a recent-sessions digest when a repo has no memory yet: the last few sessions' titles, branches, prompts and files, marked as notes rather than instructions. It draws on sessions from every agent that worked in the repo (Codex, Cursor and the rest), so their prompts reach Claude Code too."
      control={
        <Switch
          id={id}
          checked={!!hook.data?.digestEnabled}
          disabled={hook.isPending || set.isPending}
          onCheckedChange={(v) => set.mutate(v)}
        />
      }
    >
      <Note>
        Mute one repo with <code>cal hook --mute</code> from inside it.
        {muted > 0 && ` ${muted} ${muted === 1 ? "repo is" : "repos are"} muted.`}
      </Note>
      {hook.isError && (
        <InlineError>Couldn't load the digest setting: {errorText(hook.error)}</InlineError>
      )}
      {set.isError && (
        <InlineError>Couldn't change the session digest: {errorText(set.error)}</InlineError>
      )}
    </SettingRow>
  );
}

/** Harvest Claude Code's own auto-memory files into project memory. Runs in the app, not
 *  the hooks, so it does not depend on the integration being installed. */
function HarvestRow() {
  const id = useId();
  const qc = useQueryClient();
  const harvest = useQuery({ queryKey: ["harvest_settings"], queryFn: api.harvestSettings });
  const set = useMutation({
    mutationFn: api.setHarvestClaudeMemory,
    onMutate: (on: boolean) =>
      qc.setQueryData<HarvestSettings>(["harvest_settings"], (prev) =>
        prev ? { ...prev, claudeMemory: on } : prev,
      ),
    onSettled: () => {
      void qc.invalidateQueries({ queryKey: ["harvest_settings"] });
      // Turning it off removes the imported facts; on imports them again.
      void qc.invalidateQueries({ queryKey: ["project_memory"] });
    },
  });
  const h = harvest.data;
  return (
    <SettingRow
      htmlFor={id}
      title="Import Claude Code memory"
      description="Import Claude Code auto memory into project memory: its feedback memories become gotchas and its project memories become decisions. The session hook leaves them out, since Claude Code already loads its own."
      control={
        <Switch
          id={id}
          checked={!!h?.claudeMemory}
          disabled={harvest.isPending || set.isPending}
          onCheckedChange={(v) => set.mutate(v)}
        />
      }
    >
      {h?.claudeMemory && (
        <Note>
          {h.importedFacts === 0
            ? "No Claude Code memories imported yet."
            : `${h.importedFacts.toLocaleString()} ${h.importedFacts === 1 ? "memory" : "memories"} imported.`}
        </Note>
      )}
      {harvest.isError && (
        <InlineError>Couldn't load the import setting: {errorText(harvest.error)}</InlineError>
      )}
      {set.isError && <InlineError>Couldn't change the import: {errorText(set.error)}</InlineError>}
    </SettingRow>
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

const HOOK_AGENTS: Record<
  HookAgentId,
  {
    label: string;
    injects: string;
    /** `configPath` is the row's hooks file, under the agent's config dir as the backend
     *  resolved it (CODEX_HOME, or the Settings override), so the fix names the real file. */
    blockedFix: (configPath: string) => string;
    afterInstall: string;
  }
> = {
  codex: {
    label: "Codex",
    injects:
      "Injects the repo's memory when a Codex session starts, clears or compacts (imported Claude Code memories included, marked as such), or a recent-sessions digest while it has none. Also snapshots a session before compaction and queues an ended session for auto-distill.",
    blockedFix: (configPath) =>
      `Remove hooks = false (or codex_hooks = false) from the [features] table in ${siblingFile(configPath, "config.toml")}, then try again.`,
    afterInstall:
      "Codex runs a new hook only after you trust it: run /hooks in Codex and approve the Callimachus entries.",
  },
  gemini: {
    label: "Gemini CLI",
    injects:
      "Injects the repo's memory when a Gemini CLI session starts or is cleared (imported Claude Code memories included, marked as such), or a recent-sessions digest while it has none. Also queues an ended session for auto-distill.",
    blockedFix: (configPath) => `Set hooksConfig.enabled to true in ${configPath}, then try again.`,
    afterInstall: "New Gemini CLI sessions pick them up.",
  },
  copilot: {
    label: "GitHub Copilot",
    injects:
      "Injects the repo's memory when a Copilot CLI or VS Code Copilot session starts (imported Claude Code memories included, marked as such), or a recent-sessions digest while it has none. Also queues an ended Copilot CLI session for auto-distill; VS Code Copilot sessions are not indexed from Copilot's session store yet.",
    blockedFix: (configPath) => `Set disableAllHooks to false in ${configPath}, then try again.`,
    afterInstall: "New Copilot CLI and VS Code Copilot sessions pick them up.",
  },
  cursor: {
    label: "Cursor",
    injects:
      "Injects the repo's memory when a Cursor agent chat starts (imported Claude Code memories included, marked as such), or a recent-sessions digest while it has none. Also queues an ended chat for auto-distill.",
    blockedFix: () => "Check the Hooks tab in Cursor's Customize view, then try again.",
    afterInstall:
      "Cursor reloads its hooks by itself, so new chats pick them up. Cursor does not wait for them, though: the first chat after Cursor starts, while its hooks load, or a first prompt sent right away can miss the memory.",
  },
};

/** `name` next to `file`: Codex's hooks.json and config.toml share its config dir. */
function siblingFile(file: string, name: string): string {
  const cut = Math.max(file.lastIndexOf("/"), file.lastIndexOf("\\"));
  return cut < 0 ? name : `${file.slice(0, cut + 1)}${name}`;
}

/** Memory hooks in Codex's, Gemini CLI's, GitHub Copilot's and Cursor's own hook systems,
 *  one switch per detected agent. */
function AgentHooksRows() {
  const integrations = useQuery({
    queryKey: ["agent_integrations"],
    queryFn: api.agentIntegrationsStatus,
  });
  const hooks = useQuery({ queryKey: ["agent_hooks"], queryFn: api.agentHooksStatus });
  // Installing refuses from an unstable app path, like the MCP registration above.
  const recall = useRecallStatus();
  const exeProblem = recall.data?.exeStable === false ? recall.data.exeProblem : null;

  const present = new Set((integrations.data ?? []).filter((a) => a.present).map((a) => a.id));
  // `present` comes with each row; an older backend leaves it out, and then the MCP rows
  // (which have no Copilot) decide.
  const rows = (hooks.data ?? []).filter((r) => r.present ?? present.has(r.agent));
  if (hooks.isError) {
    return (
      <SettingRow title="Memory hooks">
        <InlineError>
          Couldn't read the other agents' memory hooks: {errorText(hooks.error)}
        </InlineError>
      </SettingRow>
    );
  }
  return (
    <>
      {rows.map((row) => (
        <AgentHookRow key={row.agent} row={row} exeProblem={exeProblem ?? null} />
      ))}
    </>
  );
}

function AgentHookRow({ row, exeProblem }: { row: AgentHooksStatus; exeProblem: string | null }) {
  const id = useId();
  const qc = useQueryClient();
  const info = HOOK_AGENTS[row.agent];
  const set = useMutation({
    mutationFn: (on: boolean) =>
      on ? api.installAgentHooks(row.agent) : api.uninstallAgentHooks(row.agent),
    onSuccess: (next, on) => {
      qc.setQueryData<AgentHooksStatus[]>(["agent_hooks"], (prev) =>
        prev?.map((r) => (r.agent === next.agent ? next : r)),
      );
      if (on)
        notify.success(`${info.label} memory hooks added`, { description: info.afterInstall });
      else notify.success(`${info.label} memory hooks removed`);
    },
    onError: (e, on) =>
      notify.error(
        on ? `Couldn't add the ${info.label} hooks` : `Couldn't remove the ${info.label} hooks`,
        { description: errorText(e) },
      ),
    onSettled: () => qc.invalidateQueries({ queryKey: ["agent_hooks"] }),
  });
  // A blocked agent can still be switched off (hooks added before the agent's config
  // turned them off), never on.
  const cantTurnOn = !row.installed && (!!row.blocked || !!exeProblem);
  return (
    <SettingRow
      htmlFor={id}
      title={`${info.label} memory hooks`}
      description={info.injects}
      control={
        <Switch
          id={id}
          checked={row.installed}
          disabled={set.isPending || cantTurnOn}
          onCheckedChange={(v) => set.mutate(v)}
        />
      }
    >
      {row.blocked ? (
        <InlineError>
          Can't turn these on: {row.blocked}. {info.blockedFix(row.configPath)}
        </InlineError>
      ) : (
        <Note>
          {row.installed && !row.needsApproval ? `${info.afterInstall} ` : ""}Writes{" "}
          <code>{row.configPath}</code>. Mute the digest for one repo with{" "}
          <code className="whitespace-nowrap">cal hook --mute</code> from inside it; that works for
          every agent.
        </Note>
      )}
      {/* Codex trusts a hook by its command, and an app update or move changes the path the
          hooks run, so Codex holds them until the user approves them again. */}
      {row.installed && row.needsApproval && (
        <Note>
          Codex asks you to approve the hooks again in <code>/hooks</code> after an update or move:
          run <code>/hooks</code> in Codex and approve the Callimachus entries.
        </Note>
      )}
      {set.isError && (
        <InlineError>
          Couldn't change the {info.label} hooks: {errorText(set.error)}
        </InlineError>
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
