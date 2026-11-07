import { useId, useState } from "react";
import { useMutation, useMutationState, useQuery, useQueryClient } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import { Field, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Spinner } from "@/components/ui/spinner";
import {
  type AgentIntegration,
  api,
  type DataRoots,
  INDEXABLE_SOURCES,
  type IndexReport,
  type IntegrationStatus,
  SOURCE_LABELS,
  type SourceKind,
} from "../../lib/api";
import { AGGREGATE_STALE_MS, invalidateIndexDerived } from "../../lib/queryClient";
import {
  fetchJobsStatus,
  reindexBlocked,
  useIndexError,
  useIndexRewriting,
  usePushed,
  waitForReindexSlot,
} from "../../lib/queries";
import { track } from "../../lib/telemetry";
import { errorText, notify } from "../../lib/toast";
import { InlineError, Note, SectionHeader, SettingRow, SettingsGroup } from "./shared";

const INDEXABLE: SourceKind[] = INDEXABLE_SOURCES;

/** Every single-source reindex (the row buttons and the one after a data-folder change)
 *  runs under this key, so each row reads its own pending state from the mutation cache:
 *  two sources can be in flight at once, and the state survives leaving Settings. */
const INDEX_SOURCE_KEY = ["index_source"] as const;

/** Sources whose reindex after a data-folder change is waiting for another job to end. */
const REINDEX_WAITING_KEY = ["reindex_waiting"] as const;

/** The result toast for a single-source reindex. Fired from the mutation's own callbacks
 *  (not `mutate`'s), so it still lands after the user has left Settings. */
function notifyReindexed(kind: SourceKind, report: IndexReport) {
  const errors =
    report.errors > 0
      ? `, ${report.errors.toLocaleString()} error${report.errors === 1 ? "" : "s"}`
      : "";
  notify.success(`${SOURCE_LABELS[kind]} reindexed`, {
    description: `${report.threadsIndexed.toLocaleString()} threads, ${report.messagesIndexed.toLocaleString()} messages${errors}`,
  });
}

function notifyReindexFailed(kind: SourceKind, e: unknown) {
  notify.error(`Couldn't reindex ${SOURCE_LABELS[kind]}`, { description: errorText(e) });
}

/** Which agent transcripts are indexed, a per-source reindex, and the data-folder overrides. */
export function SourcesSection() {
  const queryClient = useQueryClient();
  const stats = useQuery({ queryKey: ["db_stats"], queryFn: api.dbStats });
  // Per-source counts for the row descriptions. Same aggregate as the Stats view, so it is
  // usually cached already; index_source invalidates it through invalidateIndexDerived.
  const indexStats = useQuery({
    queryKey: ["index_stats"],
    queryFn: api.indexStats,
    staleTime: AGGREGATE_STALE_MS,
  });
  // Reindex refuses or does nothing while Shrink index / Reclaim space runs.
  const rewriting = useIndexRewriting();

  const onIndexed = () => {
    queryClient.invalidateQueries({ queryKey: ["db_stats"] });
    queryClient.invalidateQueries({ queryKey: ["results"] });
    // index_source emits no index:done, so refresh Stats / Today / Projects here.
    invalidateIndexDerived();
  };
  const indexOne = useMutation({
    mutationKey: INDEX_SOURCE_KEY,
    mutationFn: (kind: SourceKind) => api.indexSource(kind),
    onSuccess: (report, kind) => {
      onIndexed();
      notifyReindexed(kind, report);
    },
    onError: (e, kind) => {
      track("error_shown", { kind: "index_failed" });
      notifyReindexFailed(kind, e);
    },
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
    onSuccess: (report, kind) => {
      onIndexed();
      notifyReindexed(kind, report);
    },
    onError: (e, kind) => {
      track("error_shown", { kind: "index_failed" });
      notifyReindexFailed(kind, e);
    },
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
  // Why the last full reindex failed (index:error, see main.tsx), until one succeeds.
  const indexError = useIndexError();

  const s = stats.data;
  return (
    <>
      <SectionHeader
        title="Sources"
        description={
          s
            ? `${s.threads.toLocaleString()} threads · ${s.messages.toLocaleString()} messages indexed`
            : "Which agent transcripts are indexed, and where they're read from."
        }
      />
      <div className="space-y-8">
        <div className="space-y-2">
          <SettingsGroup title="Agent transcripts">
            {INDEXABLE.map((k) => {
              const busy = indexing.has(k);
              const perSource = indexStats.data?.perSource.find((p) => p.kind === k);
              // Absent from perSource once stats have loaded = nothing indexed from it.
              const threads = indexStats.data ? (perSource?.threads ?? 0) : undefined;
              return (
                <SettingRow
                  key={k}
                  compact
                  title={SOURCE_LABELS[k]}
                  description={
                    threads === undefined
                      ? undefined
                      : threads > 0 && perSource
                        ? `${perSource.threads.toLocaleString()} threads · ${perSource.messages.toLocaleString()} messages`
                        : "Nothing indexed from this agent yet"
                  }
                  control={
                    <Button
                      size="sm"
                      variant={threads === 0 ? "ghost" : "outline"}
                      onClick={() => indexOne.mutate(k)}
                      disabled={busy || rewriting}
                    >
                      {busy && <Spinner data-icon="inline-start" />}
                      {waiting.includes(k) ? "Waiting…" : busy ? "Indexing…" : "Reindex"}
                    </Button>
                  }
                />
              );
            })}
          </SettingsGroup>
          {waiting.length > 0 && (
            <Note className="px-0.5">
              Another reindex or an index rewrite is running.{" "}
              {waiting.map((k) => SOURCE_LABELS[k]).join(" and ")} will reindex from the new folder
              as soon as it finishes.
            </Note>
          )}
          {lastIndexRun?.status === "error" && (
            <InlineError className="px-0.5">
              Couldn't reindex {SOURCE_LABELS[lastIndexRun.kind]}: {errorText(lastIndexRun.error)}
            </InlineError>
          )}
          {indexError && (
            <InlineError className="px-0.5">The last full reindex failed: {indexError}</InlineError>
          )}
        </div>

        <DataRootFields onChanged={(kind) => reindexMoved.mutate(kind)} />
      </div>
    </>
  );
}

type RootKey = "claudeConfigDir" | "codexHome" | "copilotHome";

const DATA_ROOTS: {
  key: RootKey;
  effective: "effectiveClaudeConfigDir" | "effectiveCodexHome" | "effectiveCopilotHome";
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
  {
    key: "copilotHome",
    effective: "effectiveCopilotHome",
    kind: "copilot_cli",
    label: "Copilot CLI data folder",
    fallback: "~/.copilot",
  },
];

/** How many parts of the Claude Code integration a status shows installed. */
function claudeSetupParts(s: IntegrationStatus | undefined): number {
  return s ? [s.skillInstalled, s.mcpRegistered, s.hookInstalled].filter(Boolean).length : 0;
}

function codexRegistered(agents: AgentIntegration[] | undefined): boolean {
  return !!agents?.some((a) => a.id === "codex" && a.registered);
}

/** Overrides for relocated Claude Code / Codex / Copilot CLI data. An app opened from the
 *  Dock, Finder or Start menu never sees the shell's CLAUDE_CONFIG_DIR / CODEX_HOME /
 *  COPILOT_HOME, so without these it indexes the default folder. A change reindexes that source, since the watcher only
 *  sees new writes. */
function DataRootFields({ onChanged }: { onChanged: (kind: SourceKind) => void }) {
  const idBase = useId();
  const queryClient = useQueryClient();
  const roots = useQuery({ queryKey: ["data_roots"], queryFn: api.getDataRoots });
  // No draft = not editing: show the saved value. A draft overrides it while typing, so a
  // refetch landing mid-edit can't clobber the input (same as the Ollama URL field).
  const [drafts, setDrafts] = useState<Partial<Record<RootKey, string>>>({});
  // The skill, hooks and MCP entries live in the agent's own folder, and their status now
  // reads the new one. Same queries as the Agents section (no extra fetch).
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
    // Neither integration refetches on its own (no window-focus refetch), so the Agents
    // section would keep showing the old folder's setup while recall quietly stops working.
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
    onSuccess: (next, vars) => {
      const prev = queryClient.getQueryData<DataRoots>(["data_roots"]);
      queryClient.setQueryData(["data_roots"], next);
      const moved = DATA_ROOTS.filter((r) => prev && prev[r.effective] !== next[r.effective]);
      for (const r of DATA_ROOTS.filter((r) => r.key in vars)) {
        // The save lands in a backend file the field can't show, so every save gets a toast.
        // Only a real move reindexes: clearing a field that already matched the default
        // changes nothing the user could check.
        const didMove = moved.includes(r);
        notify.success(`${r.label} saved`, {
          description: didMove ? "Reindexing from the new folder…" : undefined,
        });
      }
      for (const r of moved) onChanged(r.kind);
      if (moved.length > 0) onFoldersMoved(moved.map((r) => r.kind));
    },
    onError: (e) => {
      notify.error("Couldn't save the folder", { description: errorText(e) });
      return queryClient.invalidateQueries({ queryKey: ["data_roots"] });
    },
  });

  const r = roots.data;
  return (
    <SettingsGroup
      title="Data folders"
      description={
        <>
          Only if you moved them with <code className="font-mono text-xs">CLAUDE_CONFIG_DIR</code>,{" "}
          <code className="font-mono text-xs">CODEX_HOME</code> or{" "}
          <code className="font-mono text-xs">COPILOT_HOME</code>. Leave blank for the default.
        </>
      }
    >
      <SettingRow>
        <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
          {DATA_ROOTS.map((root) => {
            const id = `${idBase}-${root.key}`;
            return (
              <Field key={root.key}>
                <FieldLabel htmlFor={id}>{root.label}</FieldLabel>
                <Input
                  id={id}
                  className="font-mono text-[13px]"
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
            );
          })}
        </div>
        {roots.isError && (
          <InlineError>Couldn't read the data folders: {errorText(roots.error)}</InlineError>
        )}
        {save.isError && (
          <InlineError>Couldn't save the folder: {errorText(save.error)}</InlineError>
        )}
        {leftBehind.length > 0 && (
          <Note>
            <span role="status">
              The {leftBehind.join(" and ")} integration is still set up in the old folder. Set it
              up again in Agents so it works from the new one.
            </span>
          </Note>
        )}
      </SettingRow>
    </SettingsGroup>
  );
}
