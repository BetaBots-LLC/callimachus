import { type ReactNode, useId, useRef, useState } from "react";
import { useMutation, useMutationState, useQuery, useQueryClient } from "@tanstack/react-query";
import { useDebouncedCallback } from "@tanstack/react-pacer";
import {
  type AgentIntegration,
  api,
  type DataRoots,
  INDEXABLE_SOURCES,
  type IntegrationStatus,
  type KnowledgeConfig,
  PROVIDERS,
  SOURCE_LABELS,
  type SourceKind,
} from "../lib/api";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { useAppForm } from "@/lib/form";
import { describeKeySaveError } from "../lib/errors";
import { baseName } from "../lib/format";
import { invalidateIndexDerived } from "../lib/queryClient";
import {
  EMBED_ERROR_KEY,
  fetchJobsStatus,
  useEmbedError,
  useEmbedStatus,
  useIndexError,
  useIndexRewriting,
  reindexBlocked,
  usePushed,
  waitForReindexSlot,
} from "../lib/queries";
import { track } from "../lib/telemetry";
import { useSettings } from "../store/settings";
import { CleanupCard } from "./CleanupCard";
import { TelemetrySetting } from "./TelemetryConsent";

const INDEXABLE: SourceKind[] = INDEXABLE_SOURCES;

/** Every single-source reindex (the row buttons and the one after a data-folder change)
 *  runs under this key, so each row reads its own pending state from the mutation cache:
 *  two sources can be in flight at once, and the state survives leaving Settings. */
const INDEX_SOURCE_KEY = ["index_source"] as const;

/** Sources whose reindex after a data-folder change is waiting for another job to end. */
const REINDEX_WAITING_KEY = ["reindex_waiting"] as const;

export function SettingsView() {
  const queryClient = useQueryClient();
  const stats = useQuery({ queryKey: ["db_stats"], queryFn: api.dbStats });
  // Event-driven; never polls while idle (see lib/queries.ts).
  const embed = useEmbedStatus();
  // Reindex and Build refuse or do nothing while Shrink index / Reclaim space runs.
  const rewriting = useIndexRewriting();

  const onIndexed = () => {
    queryClient.invalidateQueries({ queryKey: ["db_stats"] });
    queryClient.invalidateQueries({ queryKey: ["results"] });
    // index_source emits no index:done, so refresh Stats / Coach / Projects here.
    invalidateIndexDerived();
  };
  const indexOne = useMutation({
    mutationKey: INDEX_SOURCE_KEY,
    mutationFn: (kind: SourceKind) => api.indexSource(kind),
    onSuccess: onIndexed,
    onError: () => track("error_shown", { kind: "index_failed" }),
  });
  // After a data-folder change. index_source refuses while another reindex holds the flag
  // (a full reindex, the launch catch-up scan, the other folder's reindex) or the index is
  // being rewritten, and the new folder is already saved, so wait for that job and scan
  // then rather than leave the relocated history unindexed until the next launch.
  const reindexMoved = useMutation({
    mutationKey: INDEX_SOURCE_KEY,
    mutationFn: async (kind: SourceKind) => {
      const setWaiting = (on: boolean) =>
        // `?? []`, not a default parameter: the slot reads as null before anything is set.
        queryClient.setQueryData<SourceKind[] | null>(REINDEX_WAITING_KEY, (prev) => {
          const others = (prev ?? []).filter((k) => k !== kind);
          return on ? [...others, kind] : others;
        });
      try {
        for (;;) {
          try {
            return await api.indexSource(kind);
          } catch (e) {
            // Only a held flag is worth waiting out; anything else is a real failure.
            const jobs = await fetchJobsStatus(queryClient).catch(() => null);
            if (!reindexBlocked(jobs)) throw e;
          }
          setWaiting(true);
          await waitForReindexSlot(queryClient);
          setWaiting(false);
        }
      } finally {
        setWaiting(false);
      }
    },
    onSuccess: onIndexed,
    onError: () => track("error_shown", { kind: "index_failed" }),
  });
  const indexRuns = useMutationState({
    filters: { mutationKey: INDEX_SOURCE_KEY },
    select: (m) => ({
      kind: m.state.variables as SourceKind,
      status: m.state.status,
      error: m.state.error,
    }),
  });
  const indexing = new Set(indexRuns.filter((r) => r.status === "pending").map((r) => r.kind));
  const lastIndexRun = indexRuns.length > 0 ? indexRuns[indexRuns.length - 1] : null;
  const waiting = usePushed<SourceKind[]>(REINDEX_WAITING_KEY) ?? [];
  const embedError = useEmbedError();
  // Why the last full reindex failed (index:error, see main.tsx), until one succeeds.
  const indexError = useIndexError();
  const buildIndex = useMutation({
    mutationFn: api.buildEmbeddings,
    // A retry starts clean; a new failure arrives as a fresh embed:error.
    onMutate: () => queryClient.setQueryData(EMBED_ERROR_KEY, null),
    // Pick up running:true now; progress then streams in via embed:progress.
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["embed_status"] }),
  });
  const vaultDir = useSettings((s) => s.vaultDir);
  const setVaultDir = useSettings((s) => s.setVaultDir);
  const synthProvider = useSettings((s) => s.synthProvider);
  const setSynthProvider = useSettings((s) => s.setSynthProvider);
  const synthModel = useSettings((s) => s.synthModel);
  const setSynthModel = useSettings((s) => s.setSynthModel);
  const vaults = useQuery({ queryKey: ["obsidian_vaults"], queryFn: api.obsidianVaults });
  // The vault input saves on every keystroke, so its analytics event is sent once per
  // edit (on blur, if the value changed since focus) rather than per character.
  const vaultAtFocus = useRef<string | null>(null);
  const chooseVault = (path: string) => {
    if (path === vaultDir) return;
    setVaultDir(path);
    track("setting_changed", { key: "vault_dir" });
  };

  return (
    <div className="mx-auto w-full max-w-3xl space-y-6 overflow-y-auto p-6">
      <Card>
        <CardHeader>
          <CardTitle>Providers &amp; keys</CardTitle>
          <p className="text-sm text-muted-foreground">
            Keys are stored in your OS keychain, never on disk.
          </p>
        </CardHeader>
        <CardContent className="space-y-2">
          {PROVIDERS.map((p) => (
            <ProviderRow key={p.id} id={p.id} label={p.label} />
          ))}
        </CardContent>
      </Card>

      <RecallIntegrationCard />

      <OtherAgentsCard />

      <Card>
        <CardHeader>
          <CardTitle>Sources</CardTitle>
          <p className="text-sm text-muted-foreground">
            {stats.data
              ? `${stats.data.threads.toLocaleString()} threads · ${stats.data.messages.toLocaleString()} messages indexed`
              : ""}
          </p>
        </CardHeader>
        <CardContent className="space-y-2">
          {INDEXABLE.map((k) => (
            <div key={k} className="flex items-center justify-between border-b pb-2 last:border-0">
              <span className="text-sm">{SOURCE_LABELS[k]}</span>
              <Button
                size="sm"
                variant="outline"
                onClick={() => indexOne.mutate(k)}
                disabled={indexing.has(k) || rewriting}
              >
                {waiting.includes(k) ? "Waiting…" : indexing.has(k) ? "Indexing…" : "Reindex"}
              </Button>
            </div>
          ))}
          {waiting.length > 0 && (
            <p className="text-xs text-muted-foreground">
              Another reindex or an index rewrite is running.{" "}
              {waiting.map((k) => SOURCE_LABELS[k]).join(" and ")} will reindex from the new folder
              as soon as it finishes.
            </p>
          )}
          {lastIndexRun?.status === "error" && (
            <p role="alert" className="text-xs text-destructive">
              Couldn't reindex {SOURCE_LABELS[lastIndexRun.kind]}: {String(lastIndexRun.error)}
            </p>
          )}
          {indexError && (
            <p role="alert" className="text-xs break-words text-destructive">
              The last full reindex failed: {indexError}
            </p>
          )}
          <DataRootFields onChanged={(kind) => reindexMoved.mutate(kind)} />
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>Semantic index</CardTitle>
          <p className="text-sm text-muted-foreground">
            {embed.data
              ? `${embed.data.done.toLocaleString()} / ${embed.data.total.toLocaleString()} messages embedded`
              : ""}
          </p>
        </CardHeader>
        <CardContent className="space-y-2">
          <Button
            size="sm"
            variant="outline"
            onClick={() => buildIndex.mutate()}
            disabled={embed.data?.running || rewriting}
          >
            {embed.data?.running ? "Building…" : "Build / update semantic index"}
          </Button>
          {embedError && !embed.data?.running && (
            <p role="alert" className="text-xs text-destructive">
              The last build stopped: {embedError}. Nothing was marked as embedded, so building
              again picks up where it left off.
            </p>
          )}
          {buildIndex.isError && (
            <p role="alert" className="text-xs text-destructive">
              Couldn't start the build: {String(buildIndex.error)}
            </p>
          )}
        </CardContent>
      </Card>

      <DistillationCard />

      <Card>
        <CardHeader>
          <CardTitle>Obsidian export</CardTitle>
          <p className="text-sm text-muted-foreground">
            Vault folder that “Export to Obsidian” writes into, and which LLM does the synthesized
            version.
          </p>
        </CardHeader>
        <CardContent className="space-y-4">
          {vaults.data && vaults.data.length > 0 && (
            <Field label="Detected vaults">
              <div className="flex flex-wrap gap-1.5">
                {vaults.data.map((path) => {
                  const name = baseName(path);
                  return (
                    <Button
                      key={path}
                      size="xs"
                      variant={vaultDir === path ? "secondary" : "outline"}
                      onClick={() => chooseVault(path)}
                      title={path}
                    >
                      {name}
                    </Button>
                  );
                })}
              </div>
            </Field>
          )}
          <Field label="Vault folder">
            <Input
              value={vaultDir}
              onChange={(e) => setVaultDir(e.target.value)}
              onFocus={() => {
                vaultAtFocus.current = vaultDir;
              }}
              onBlur={() => {
                const before = vaultAtFocus.current;
                vaultAtFocus.current = null;
                if (before !== null && before !== useSettings.getState().vaultDir) {
                  track("setting_changed", { key: "vault_dir" });
                }
              }}
              placeholder="/Users/you/Documents/Obsidian Vault/Vault"
              spellCheck={false}
            />
          </Field>
          <div className="grid grid-cols-2 gap-3">
            <Field label="Synthesis provider">
              <select
                value={synthProvider}
                onChange={(e) => {
                  setSynthProvider(e.target.value);
                  track("setting_changed", { key: "synth_engine" });
                }}
                className="h-8 w-full rounded-lg border border-input bg-transparent px-2.5 text-sm text-foreground outline-none transition-colors focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50 dark:bg-input/30"
              >
                <option value="">Auto (first key)</option>
                {PROVIDERS.map((p) => (
                  <option key={p.id} value={p.id}>
                    {p.label}
                  </option>
                ))}
              </select>
            </Field>
            <Field label={synthProvider ? "Model" : "Model (auto)"}>
              <Input
                value={synthModel}
                onChange={(e) => setSynthModel(e.target.value)}
                placeholder="default for provider"
                disabled={!synthProvider}
                spellCheck={false}
              />
            </Field>
          </div>
          <p className="text-xs text-muted-foreground">
            Auto uses the first provider you've added a key for, at its cheapest model.
          </p>
        </CardContent>
      </Card>

      <GeneralCard />
      <CleanupCard />
    </div>
  );
}

type RootKey = "claudeConfigDir" | "codexHome";

const DATA_ROOTS: {
  key: RootKey;
  effective: "effectiveClaudeConfigDir" | "effectiveCodexHome";
  kind: SourceKind;
  label: string;
  fallback: string;
}[] = [
  {
    key: "claudeConfigDir",
    effective: "effectiveClaudeConfigDir",
    kind: "claude_code",
    label: "Claude Code data folder",
    fallback: "~/.claude",
  },
  {
    key: "codexHome",
    effective: "effectiveCodexHome",
    kind: "codex",
    label: "Codex data folder",
    fallback: "~/.codex",
  },
];

/** How many parts of the Claude Code integration a status shows installed. */
function claudeSetupParts(s: IntegrationStatus | undefined): number {
  return s ? [s.skillInstalled, s.mcpRegistered, s.hookInstalled].filter(Boolean).length : 0;
}

function codexRegistered(agents: AgentIntegration[] | undefined): boolean {
  return !!agents?.some((a) => a.id === "codex" && a.registered);
}

/** Overrides for relocated Claude Code / Codex data. An app opened from the Dock, Finder or
 *  Start menu never sees the shell's CLAUDE_CONFIG_DIR / CODEX_HOME, so without these it
 *  indexes the default folder. A change reindexes that source, since the watcher only
 *  sees new writes. */
function DataRootFields({ onChanged }: { onChanged: (kind: SourceKind) => void }) {
  const queryClient = useQueryClient();
  const roots = useQuery({ queryKey: ["data_roots"], queryFn: api.getDataRoots });
  // No draft = not editing: show the saved value. A draft overrides it while typing, so a
  // refetch landing mid-edit can't clobber the input (same as the Ollama URL field).
  const [drafts, setDrafts] = useState<Partial<Record<RootKey, string>>>({});
  // The skill, hooks and MCP entries live in the agent's own folder, and their status now
  // reads the new one. Same queries as the integration cards above (no extra fetch).
  const recall = useQuery({
    queryKey: ["recall_integration"],
    queryFn: api.recallIntegrationStatus,
  });
  const agents = useQuery({
    queryKey: ["agent_integrations"],
    queryFn: api.agentIntegrationsStatus,
  });
  // What was set up when a folder moved, to tell whether that setup stayed behind.
  const [setupBefore, setSetupBefore] = useState<{ claudeParts?: number; codex?: boolean }>({});
  const leftBehind = [
    setupBefore.claudeParts !== undefined &&
      claudeSetupParts(recall.data) < setupBefore.claudeParts &&
      "Claude Code",
    setupBefore.codex && !codexRegistered(agents.data) && "Codex",
  ].filter((name): name is string => !!name);
  function onFoldersMoved(moved: SourceKind[]) {
    setSetupBefore({
      claudeParts: moved.includes("claude_code") ? claudeSetupParts(recall.data) : undefined,
      codex: moved.includes("codex") && codexRegistered(agents.data),
    });
    // Neither card refetches on its own (no window-focus refetch), so they would keep
    // showing the old folder's setup while recall quietly stops working.
    void queryClient.invalidateQueries({ queryKey: ["recall_integration"] });
    void queryClient.invalidateQueries({ queryKey: ["agent_integrations"] });
  }
  const save = useMutation({
    mutationFn: api.setDataRoots,
    // Show the new value at once (the draft is gone after blur). Only the saved fields
    // change here; the effective ones still hold the old paths for the comparison below.
    onMutate: (vars) =>
      queryClient.setQueryData<DataRoots>(["data_roots"], (prev) =>
        prev ? { ...prev, ...vars } : prev,
      ),
    onSuccess: (next) => {
      const prev = queryClient.getQueryData<DataRoots>(["data_roots"]);
      queryClient.setQueryData(["data_roots"], next);
      const moved = DATA_ROOTS.filter((r) => prev && prev[r.effective] !== next[r.effective]);
      for (const r of moved) onChanged(r.kind);
      if (moved.length > 0) onFoldersMoved(moved.map((r) => r.kind));
    },
    onError: () => queryClient.invalidateQueries({ queryKey: ["data_roots"] }),
  });

  const r = roots.data;
  return (
    <div className="space-y-2 border-t pt-3">
      <div className="grid grid-cols-2 gap-3">
        {DATA_ROOTS.map((root) => (
          <Field key={root.key} label={root.label}>
            <Input
              value={drafts[root.key] ?? r?.[root.key] ?? ""}
              onChange={(e) => {
                const value = e.target.value;
                setDrafts((d) => ({ ...d, [root.key]: value }));
              }}
              onBlur={(e) => {
                const value = e.target.value.trim();
                setDrafts(({ [root.key]: _, ...rest }) => rest);
                if (r && value !== r[root.key]) save.mutate({ [root.key]: value });
              }}
              onKeyDown={(e) => {
                if (e.key === "Enter") e.currentTarget.blur();
              }}
              placeholder={r?.[root.effective] ?? root.fallback}
              disabled={!r}
              spellCheck={false}
            />
          </Field>
        ))}
      </div>
      <p className="text-xs text-muted-foreground">
        Only needed if you moved them (<code>CLAUDE_CONFIG_DIR</code> / <code>CODEX_HOME</code>).
        Leave blank for the default.
      </p>
      {roots.isError && (
        <p role="alert" className="text-xs text-destructive">
          Couldn't read the data folders: {String(roots.error)}
        </p>
      )}
      {save.isError && (
        <p role="alert" className="text-xs text-destructive">
          Couldn't save the folder: {String(save.error)}
        </p>
      )}
      {leftBehind.length > 0 && (
        <p role="status" className="text-xs text-muted-foreground">
          The {leftBehind.join(" and ")} integration is still set up in the old folder. Set it up
          again above so it works from the new one.
        </p>
      )}
    </div>
  );
}

/** App-level behavior toggles (e.g. close-to-system-tray). */
function GeneralCard() {
  const queryClient = useQueryClient();
  const closeToTray = useQuery({ queryKey: ["close_to_tray"], queryFn: api.getCloseToTray });
  const setCloseToTray = useMutation({
    mutationFn: api.setCloseToTray,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["close_to_tray"] });
      track("setting_changed", { key: "close_to_tray" });
    },
  });
  return (
    <Card>
      <CardHeader>
        <CardTitle>General</CardTitle>
      </CardHeader>
      <CardContent className="space-y-2">
        <div className="flex items-center gap-2 text-sm">
          <Switch
            checked={!!closeToTray.data}
            disabled={closeToTray.isPending || setCloseToTray.isPending}
            onCheckedChange={(v) => setCloseToTray.mutate(v)}
          />
          Keep running in the system tray when the window is closed
        </div>
        <p className="text-xs text-muted-foreground">
          When on, closing the window hides Callimachus to the system tray instead of quitting.
          Click the tray icon to reopen it, or use the tray menu to quit.
        </p>
        <section aria-labelledby="usage-analytics-title" className="space-y-1 border-t pt-3">
          <h3 id="usage-analytics-title" className="text-sm font-medium">
            Usage analytics
          </h3>
          {/* Shares the consent card's ["telemetry_state"] query, so both always agree. */}
          <TelemetrySetting />
        </section>
      </CardContent>
    </Card>
  );
}

/** One-click Claude Code integration: install the /recall skill + register this
 *  app as the `callimachus` MCP server, no terminal or cargo. */
function RecallIntegrationCard() {
  const queryClient = useQueryClient();
  const status = useQuery({
    queryKey: ["recall_integration"],
    queryFn: api.recallIntegrationStatus,
  });
  const refresh = () => queryClient.invalidateQueries({ queryKey: ["recall_integration"] });
  const install = useMutation({ mutationFn: api.installRecallIntegration, onSuccess: refresh });
  const uninstall = useMutation({ mutationFn: api.uninstallRecallIntegration, onSuccess: refresh });
  const setRecall = useMutation({
    mutationFn: api.setProactiveRecall,
    onSuccess: () => {
      refresh();
      track("setting_changed", { key: "proactive_recall" });
    },
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
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          Claude Code
          {connected && (
            <span className="rounded-full bg-emerald-500/15 px-2 py-0.5 text-[0.6rem] font-medium uppercase tracking-wide text-emerald-600 dark:text-emerald-400">
              connected
            </span>
          )}
        </CardTitle>
        <p className="text-sm text-muted-foreground">
          Let Claude Code (and other agents) search your history. Installs the <code>/recall</code>{" "}
          skill, registers Callimachus as an MCP server, adds hooks that{" "}
          <strong>auto-inject each repo's memory</strong> at the start of a session and{" "}
          <strong>auto-snapshot</strong> a session before its context is compacted, and adds the{" "}
          <code>cal</code> CLI (used by the VS Code extension) — no terminal, no setup.
        </p>
      </CardHeader>
      <CardContent className="space-y-3">
        <div className="flex flex-wrap gap-x-4 gap-y-1 text-xs text-muted-foreground">
          <span>
            {s?.skillInstalled ? (s.skillOutdated ? "⚠ skill outdated" : "✓ skill") : "○ skill"}
          </span>
          <span>{s?.mcpRegistered ? "✓ MCP server" : "○ MCP server"}</span>
          <span>{s?.hookInstalled ? "✓ memory hook" : "○ memory hook"}</span>
          <span>{s?.calInstalled ? "✓ cal CLI" : "○ cal CLI"}</span>
          {s?.calProblem && <span className="basis-full">{s.calProblem}</span>}
        </div>
        {s?.pathHint && <PathHint hint={s.pathHint} />}
        {unstable && s?.exeProblem && (
          <p role="alert" className="text-xs text-destructive">
            {s.exeProblem}
          </p>
        )}
        <div className="flex items-center gap-2">
          <Button
            size="sm"
            onClick={() => install.mutate()}
            disabled={install.isPending || unstable}
          >
            {install.isPending
              ? "Installing…"
              : connected
                ? "Reinstall"
                : partial
                  ? "Finish setup"
                  : "Enable for Claude Code"}
          </Button>
          {(s?.skillInstalled || s?.mcpRegistered) && (
            <Button
              size="sm"
              variant="ghost"
              onClick={() => uninstall.mutate()}
              disabled={uninstall.isPending}
            >
              Remove
            </Button>
          )}
        </div>
        {install.isSuccess && (
          <p className="text-xs text-muted-foreground">
            Done. Restart Claude Code (or run <code>/mcp</code>) to pick up the server, then type{" "}
            <code>/recall</code>.
          </p>
        )}
        {install.isError && <p className="text-xs text-destructive">{String(install.error)}</p>}
        {connected && (
          <div className="flex items-center justify-between gap-3 border-t border-border/60 pt-3">
            <div className="space-y-0.5">
              <div className="text-sm font-medium">Proactive recall</div>
              <p className="text-xs text-muted-foreground">
                Before each prompt, silently tell Claude when you've solved it in a past session.
                Reads every prompt (a brief on-device lookup); off by default.
              </p>
            </div>
            <Switch
              checked={!!s?.proactiveRecallInstalled}
              disabled={setRecall.isPending}
              onCheckedChange={(v) => setRecall.mutate(v)}
            />
          </div>
        )}
        {setRecall.isError && <p className="text-xs text-destructive">{String(setRecall.error)}</p>}
      </CardContent>
    </Card>
  );
}

/** Register the `callimachus` MCP server with the OTHER detected agents (Codex / Cursor /
 *  Gemini), so they can search the history too. Only touches agents installed on this machine. */
function OtherAgentsCard() {
  const queryClient = useQueryClient();
  const status = useQuery({
    queryKey: ["agent_integrations"],
    queryFn: api.agentIntegrationsStatus,
  });
  const refresh = () => queryClient.invalidateQueries({ queryKey: ["agent_integrations"] });
  const install = useMutation({ mutationFn: api.installAgentIntegrations, onSuccess: refresh });
  const uninstall = useMutation({ mutationFn: api.uninstallAgentIntegrations, onSuccess: refresh });
  // Registering other agents refuses from an unstable app path too; the Claude Code status
  // (same query key as that card, so no extra fetch) carries the reason.
  const recall = useQuery({
    queryKey: ["recall_integration"],
    queryFn: api.recallIntegrationStatus,
  });
  const exeProblem = recall.data?.exeStable === false ? recall.data.exeProblem : null;

  const present = (status.data ?? []).filter((a) => a.present);
  const anyRegistered = present.some((a) => a.registered);
  const allRegistered = present.length > 0 && present.every((a) => a.registered);

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          Other agents
          {allRegistered && (
            <span className="rounded-full bg-emerald-500/15 px-2 py-0.5 text-[0.6rem] font-medium uppercase tracking-wide text-emerald-600 dark:text-emerald-400">
              connected
            </span>
          )}
        </CardTitle>
        <p className="text-sm text-muted-foreground">
          Register the <code>callimachus</code> MCP server with your other agents so they can search
          your history too. Only agents installed on this machine are touched.
        </p>
      </CardHeader>
      <CardContent className="space-y-3">
        {present.length === 0 ? (
          <p className="text-xs text-muted-foreground">
            No other agents detected (Codex, Cursor, or Gemini CLI).
          </p>
        ) : (
          <>
            <div className="flex flex-wrap gap-x-4 gap-y-1 text-xs text-muted-foreground">
              {present.map((a) => (
                <span key={a.id}>
                  {a.registered ? "✓" : "○"} {a.label}
                </span>
              ))}
            </div>
            {exeProblem && (
              <p role="alert" className="text-xs text-destructive">
                {exeProblem}
              </p>
            )}
            <div className="flex items-center gap-2">
              <Button
                size="sm"
                onClick={() => install.mutate()}
                disabled={install.isPending || !!exeProblem}
              >
                {install.isPending
                  ? "Registering…"
                  : allRegistered
                    ? "Re-register"
                    : "Register with detected agents"}
              </Button>
              {anyRegistered && (
                <Button
                  size="sm"
                  variant="ghost"
                  onClick={() => uninstall.mutate()}
                  disabled={uninstall.isPending}
                >
                  Remove
                </Button>
              )}
            </div>
            {install.isSuccess && (
              <p className="text-xs text-muted-foreground">
                Done. Restart the agent (or reload its MCP config) to pick up the server.
              </p>
            )}
            {install.isError && <p className="text-xs text-destructive">{String(install.error)}</p>}
          </>
        )}
      </CardContent>
    </Card>
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
    <div className="space-y-1 text-xs text-muted-foreground">
      <p>{prose}</p>
      {command && (
        <code className="block w-fit max-w-full select-all break-all rounded bg-muted px-1.5 py-0.5 font-mono text-foreground">
          {command}
        </code>
      )}
    </div>
  );
}

/** A uniform labeled field: muted label above a control. */
function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="space-y-1.5">
      <span className="block text-xs font-medium text-muted-foreground">{label}</span>
      {children}
    </div>
  );
}

const DISTILL_ENGINES = [
  { id: "", label: "Auto (first API key)" },
  { id: "ollama", label: "Ollama (local · private)" },
  { id: "ollama_cloud", label: "Ollama Cloud" },
  { id: "anthropic", label: "Anthropic" },
  { id: "openai", label: "OpenAI" },
  { id: "gemini", label: "Gemini" },
  { id: "openrouter", label: "OpenRouter" },
];

const SELECT_CLASS =
  "h-8 w-full rounded-lg border border-input bg-transparent px-2.5 text-sm text-foreground outline-none transition-colors focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50 dark:bg-input/30";

/** Opt-in LLM distillation: enable + pick engine (local Ollama or a cloud key). */
function DistillationCard() {
  const queryClient = useQueryClient();
  const cfg = useQuery({ queryKey: ["knowledge_config"], queryFn: api.knowledgeConfig });
  const cliEngines = useQuery({ queryKey: ["cli_engines"], queryFn: api.cliEngines });
  // Set when the engine picker changes, reported once the debounced save lands. Turning
  // knowledge on is reported by the backend (knowledge_enabled), not here.
  const engineChanged = useRef(false);
  const save = useMutation({
    mutationFn: (next: { enabled: boolean; provider: string; model: string }) =>
      api.setKnowledgeConfig(next.enabled, next.provider || undefined, next.model || undefined),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["knowledge_config"] });
      if (engineChanged.current) {
        engineChanged.current = false;
        track("setting_changed", { key: "knowledge_engine" });
      }
    },
  });
  // Debounce the actual write (toggling ON kicks a background backfill) so rapid flips
  // coalesce into one; the cache is updated optimistically so the UI stays instant.
  const debouncedSave = useDebouncedCallback(
    (next: { enabled: boolean; provider: string; model: string }) => save.mutate(next),
    { wait: 400 },
  );

  const c = cfg.data;
  const enabled = c?.enabled ?? false;
  const provider = c?.provider ?? "";
  const model = c?.model ?? "";
  const update = (next: Partial<{ enabled: boolean; provider: string; model: string }>) => {
    const merged = { enabled, provider, model, ...next };
    if (merged.provider !== provider) engineChanged.current = true;
    queryClient.setQueryData<KnowledgeConfig>(["knowledge_config"], {
      enabled: merged.enabled,
      provider: merged.provider || null,
      model: merged.model || null,
      autoDistill: c?.autoDistill ?? false,
    });
    debouncedSave(merged);
  };
  const local = provider === "ollama";
  // CLI backends (claude-cli / codex-cli) are keyless like Ollama, but the text DOES leave the
  // machine (via the CLI's subscription), so they're "no key" without Ollama's privacy claim.
  const isCli = provider.endsWith("-cli");
  const keyless = local || isCli;
  // Offer installed CLI backends alongside the static engines.
  const installedClis = (cliEngines.data ?? []).filter((e) => e.installed);
  const engineOptions = [
    ...DISTILL_ENGINES,
    ...installedClis.map((e) => ({ id: e.id, label: `${e.label} (subscription · no key)` })),
  ];
  const autoDistill = c?.autoDistill ?? false;
  // Refreshed on distill:done (main.tsx), when an auto-distill round spends the allowance.
  const budget = useQuery({
    queryKey: ["auto_distill_budget"],
    queryFn: api.autoDistillBudget,
    enabled,
  });
  const setCap = useMutation({
    mutationFn: (cap: number) => api.setAutoDistillDailyCap(cap),
    onSuccess: (next) => queryClient.setQueryData(["auto_distill_budget"], next),
  });
  const b = budget.data;
  const capId = useId();
  const toggleAuto = useMutation({
    mutationFn: (on: boolean) => api.setAutoDistill(on),
    onMutate: (on: boolean) =>
      queryClient.setQueryData<KnowledgeConfig>(["knowledge_config"], (prev) =>
        prev ? { ...prev, autoDistill: on } : prev,
      ),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["knowledge_config"] });
      track("setting_changed", { key: "auto_distill" });
    },
  });

  return (
    <Card>
      <CardHeader>
        <CardTitle>Knowledge</CardTitle>
        <p className="text-sm text-muted-foreground">
          Surface what matters from your history. When on, Callimachus extracts open TODOs from your
          threads (free, on-device) and adds a Todos tab. Optionally distill decisions, gotchas
          &amp; summaries with an LLM — local (Ollama) or your own API key. Off by default.
        </p>
      </CardHeader>
      <CardContent className="space-y-3">
        <div className="flex items-center gap-2 text-sm">
          <Switch checked={enabled} onCheckedChange={(v) => update({ enabled: v })} />
          Enable knowledge
        </div>
        {enabled && (
          <>
            <p className="text-xs text-muted-foreground">
              Distillation engine (optional — TODOs need none). Pick one to distill decisions &amp;
              gotchas per thread.
            </p>
            <div className="grid grid-cols-2 gap-3">
              <Field label="Engine">
                <select
                  value={provider}
                  onChange={(e) => update({ provider: e.target.value })}
                  className={SELECT_CLASS}
                >
                  {engineOptions.map((e) => (
                    <option key={e.id} value={e.id}>
                      {e.label}
                    </option>
                  ))}
                </select>
              </Field>
              <Field label={keyless ? "Model" : "Model (optional)"}>
                <Input
                  key={model}
                  defaultValue={model}
                  onBlur={(e) => {
                    if (e.target.value !== model) update({ model: e.target.value });
                  }}
                  placeholder={
                    local ? "llama3.1" : isCli ? "default for CLI" : "default for engine"
                  }
                  spellCheck={false}
                />
              </Field>
            </div>
            <p className="text-xs text-muted-foreground">
              {local
                ? "Runs locally via Ollama — thread text never leaves your machine."
                : isCli
                  ? "Runs through your logged-in CLI subscription — no API key needed. Thread text is sent to that CLI's provider, on demand per thread."
                  : provider
                    ? `Sends thread text to ${provider} using your stored API key, on demand per thread.`
                    : "Uses the first provider you've added a key for. Sends thread text to that provider, on demand per thread."}
            </p>
            <div className="flex items-center gap-2 pt-1 text-sm">
              <Switch checked={autoDistill} onCheckedChange={(v) => toggleAuto.mutate(v)} />
              Auto-distill in the background
              {b && (
                <div className="ml-auto flex items-center gap-1.5 text-xs text-muted-foreground">
                  <label htmlFor={capId}>Up to</label>
                  <Input
                    id={capId}
                    // Keyed on the saved cap so a save (or a rejected entry) resets the field.
                    key={b.dailyCap}
                    type="number"
                    min={0}
                    step={1}
                    inputMode="numeric"
                    defaultValue={b.dailyCap}
                    onBlur={(e) => {
                      const cap = Number.parseInt(e.target.value, 10);
                      if (Number.isFinite(cap) && cap >= 0 && cap !== b.dailyCap)
                        setCap.mutate(cap);
                      else e.target.value = String(b.dailyCap);
                    }}
                    onKeyDown={(e) => {
                      if (e.key === "Enter") e.currentTarget.blur();
                    }}
                    disabled={setCap.isPending}
                    className="h-7 w-16 text-right tabular-nums"
                  />
                  <span>threads a day</span>
                </div>
              )}
            </div>
            <p className="text-xs text-muted-foreground">
              Distill new and changed threads automatically as they're indexed, so Ask, recall, and
              Project Memory stay populated without clicking. Paced, cancellable, and yields to
              indexing.{" "}
              {local
                ? "Free and on-device via Ollama."
                : isCli
                  ? "Runs through your CLI subscription (no API key)."
                  : "Uses your provider key, so it has a per-thread cost."}
            </p>
            {b && (
              <p className="text-xs text-muted-foreground">
                {b.dailyCap === 0
                  ? "A daily limit of 0 pauses auto-distill."
                  : autoDistill
                    ? `${b.remainingToday} of ${b.dailyCap} left today (resets at midnight UTC) · ${b.eligibleThreads} threads from the last ${b.recentDays} days still to distill.`
                    : `${b.eligibleThreads} threads from the last ${b.recentDays} days would be distilled, up to ${b.dailyCap} a day.`}
              </p>
            )}
            {setCap.isError && (
              <p role="alert" className="text-xs text-destructive">
                Couldn't save the daily limit: {String(setCap.error)}
              </p>
            )}
          </>
        )}
      </CardContent>
    </Card>
  );
}

function ProviderRow({ id, label }: { id: string; label: string }) {
  const queryClient = useQueryClient();
  const hasKey = useQuery({ queryKey: ["hasKey", id], queryFn: () => api.providerHasKey(id) });
  const invalidate = () => queryClient.invalidateQueries({ queryKey: ["hasKey", id] });
  const remove = useMutation({
    mutationFn: () => api.deleteApiKey(id),
    onSuccess: () => {
      invalidate();
      track("setting_changed", { key: "api_key" });
    },
  });

  // Local Ollama and Ollama Cloud accept a custom server URL (persisted; shared with the chat
  // header). Local Ollama is keyless; Ollama Cloud needs a key like any cloud provider.
  const isOllamaLike = id === "ollama" || id === "ollama_cloud";
  const baseUrl = useQuery({
    queryKey: ["providerBaseUrl", id],
    queryFn: () => api.providerBaseUrl(id),
    enabled: isOllamaLike,
  });
  const saveUrl = useMutation({
    mutationFn: (url: string) => api.setProviderBaseUrl(id, url),
    // Reflect the new URL immediately (also mirrors to the chat header, same query key); the
    // invalidate reconciles with what the backend actually stored.
    onMutate: (url: string) => queryClient.setQueryData(["providerBaseUrl", id], url),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["providerBaseUrl", id] });
      track("setting_changed", { key: "provider_base_url" });
    },
  });
  // `null` = not editing → show the saved value; a local draft overrides it while typing so an
  // in-flight query result can't clobber the input.
  const [urlEdit, setUrlEdit] = useState<string | null>(null);

  // A failed save (e.g. no Secret Service keyring on Linux) must be visible: without it,
  // Save looks like it did nothing and chat later fails with "No API key set".
  const saveKey = useMutation({
    mutationFn: (key: string) => api.setApiKey(id, key),
    onSuccess: () => {
      invalidate();
      track("setting_changed", { key: "api_key" });
    },
  });
  const form = useAppForm({
    defaultValues: { key: "" },
    onSubmit: async ({ value }) => {
      const k = value.key.trim();
      if (!k) return;
      try {
        await saveKey.mutateAsync(k);
        form.reset();
      } catch {
        // Shown below via saveKey.error; swallowing here avoids an unhandled rejection
        // from the (unawaited) form.handleSubmit().
      }
    },
  });

  // Fixed-width slots (label · status · input · Save · Remove) so a row never
  // reflows when a key is saved — Remove is always rendered, just hidden when unset.
  return (
    <div className="space-y-2 border-b pb-2 last:border-0">
      <form
        className="flex items-center gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          form.handleSubmit();
        }}
      >
        <div className="flex w-40 shrink-0 items-center gap-2">
          <span className="truncate text-sm">{label}</span>
          {hasKey.data && (
            <span className="rounded-full bg-emerald-500/15 px-2 py-0.5 text-[0.6rem] font-medium uppercase tracking-wide text-emerald-600 dark:text-emerald-400">
              set
            </span>
          )}
        </div>
        <form.AppField name="key">
          {(field) => (
            <field.TextField
              type="password"
              className="ml-auto w-56"
              placeholder={
                id === "ollama"
                  ? "no key needed"
                  : hasKey.data
                    ? "saved — type to replace"
                    : "API key"
              }
              disabled={id === "ollama"}
            />
          )}
        </form.AppField>
        <form.AppForm>
          <form.SubmitButton>Save</form.SubmitButton>
        </form.AppForm>
        <Button
          size="sm"
          variant="ghost"
          type="button"
          onClick={() => remove.mutate()}
          className={hasKey.data ? "" : "invisible"}
        >
          Remove
        </Button>
      </form>
      {saveKey.isError && (
        <p role="alert" className="text-xs text-destructive">
          {describeKeySaveError(saveKey.error)}
        </p>
      )}
      {remove.isError && (
        <p role="alert" className="text-xs text-destructive">
          Couldn't remove the key: {String(remove.error)}
        </p>
      )}
      {isOllamaLike && (
        <div className="flex items-center gap-2">
          <span className="w-40 shrink-0 text-xs text-muted-foreground">Server URL</span>
          <Input
            value={urlEdit ?? baseUrl.data ?? ""}
            onChange={(e) => setUrlEdit(e.target.value)}
            onBlur={(e) => {
              const url = e.target.value.trim();
              setUrlEdit(null);
              if (url !== (baseUrl.data ?? "")) saveUrl.mutate(url);
            }}
            placeholder={id === "ollama_cloud" ? "https://ollama.com" : "http://localhost:11434"}
            spellCheck={false}
            className="ml-auto w-56"
          />
        </div>
      )}
    </div>
  );
}
