// Typed wrappers around Tauri commands. Keep all `invoke` calls here so the rest
// of the app talks to a typed surface, not stringly-named commands.
import { invoke, Channel } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
// telemetry.ts only imports a *type* from this file, so there is no runtime import cycle.
import { setTelemetry, telemetryState } from "./telemetry";

// Opt-in usage analytics. The wrappers live in telemetry.ts next to the event schema;
// they're re-exported here so callers can reach the whole backend surface from one place.
export {
  classifyError,
  commitSearch,
  flushTelemetry,
  noteSearch,
  track,
  trackError,
  type Consent,
  type ErrorKind,
  type SettingKey,
  type TelemetryStatus,
} from "./telemetry";

export type SourceKind =
  | "claude_code"
  | "codex"
  | "cursor"
  | "gemini"
  | "qwen"
  | "goose"
  | "opencode"
  | "continue"
  | "cline"
  | "roo"
  | "kilo"
  | "copilot"
  | "copilot_cli"
  | "in_app";

export interface DbStats {
  threads: number;
  messages: number;
  sources: number;
  /** Free space inside the index file (null if it couldn't be read). */
  reclaimable: Reclaimable | null;
}

/** What Reclaim space would give back. `worthwhile`: at least 50 MB and 1% of the file are
 *  free, the backend's rule for running the rewrite at all (`cleanup::RECLAIM_MIN_BYTES`). */
export interface Reclaimable {
  bytes: number;
  fileBytes: number;
  worthwhile: boolean;
}

/** What a Reclaim space run did. `ran` is false when too little was free to rewrite. */
export interface VacuumReport {
  ran: boolean;
  freedBytes: number; // file size before minus after
  reclaimableBytes: number;
  // The closing checkpoint was held back by a reader, so the file has not shrunk yet:
  // reclaimableBytes comes back to disk at a later checkpoint. Absent from older backends.
  pending?: boolean;
}

export interface IndexReport {
  threadsIndexed: number;
  threadsSkipped: number;
  messagesIndexed: number;
  errors: number;
}

/** Per-source progress pushed during a background reindex (the `index:progress` event). */
export interface IndexProgress {
  done: number; // sources finished
  total: number; // total sources
  current: string; // source kind about to scan ("" when finishing)
}

export interface SearchFilters {
  sources?: string[];
  project?: string | null;
  after?: number | null;
  before?: number | null;
  limit?: number | null;
  includeSubagents?: boolean;
  hybrid?: boolean;
  starred?: boolean | null; // true = only starred
  tags?: string[]; // threads having ANY of these tags
}

export interface EmbedStatus {
  done: number;
  total: number;
  running: boolean;
}

export interface SearchHit {
  threadId: number;
  messageId: number;
  source: SourceKind;
  title: string | null;
  projectPath: string | null;
  role: string;
  snippet: string; // HTML with <mark>
  ts: number | null;
}

/** search_threads' result. */
export interface SearchResponse {
  hits: SearchHit[];
  /**
   * A hybrid search ran keyword-only: "loading" while the embedding model loads (retry
   * shortly), "unavailable" when it is not downloaded, failed to load, or no reader was free.
   */
  semanticSkipped: "loading" | "unavailable" | null;
  /** A newer search started first, so this one was cut short (hits is empty). Ignore it. */
  superseded: boolean;
}

export interface ThreadSummary {
  id: number;
  source: SourceKind;
  title: string | null;
  projectPath: string | null;
  messageCount: number;
  updatedAt: number | null;
  starred: boolean;
  /** Absent when the source records no branch (and from older backends). */
  gitBranch?: string | null;
}

export interface MessageRow {
  id: number;
  role: string;
  text: string;
  toolName: string | null;
  ts: number | null;
  model: string | null;
  /**
   * The full text's length (UTF-16 units, like `text.length`). get_thread sends tool rows
   * (role "tool" or toolName set) cut to their first 300 characters, so a larger textLen
   * means `text` is a preview: fetch the rest with `api.messageText(id)`.
   */
  textLen: number;
}

export interface ThreadDetail {
  id: number;
  source: SourceKind;
  externalId: string;
  title: string | null;
  projectPath: string | null;
  /** The canonical project (worktrees resolve to the main repo): the key Projects groups
   *  by. Absent when the thread has no project, and from older backends. */
  projectKey?: string | null;
  gitBranch: string | null;
  createdAt: number | null;
  updatedAt: number | null;
  starred: boolean;
  tags: string[];
  messages: MessageRow[];
}

/** Where a fact came from: "llm" (distilled), "heuristic" (TODO extraction), "user"
 *  (Remember), "agent" (an MCP write), "claude-memory" (a harvested Claude Code auto-memory
 *  file) or "claude-compact" (a PreCompact compaction summary). Older backends omit it. */
export type FactExtractor =
  | "llm"
  | "heuristic"
  | "user"
  | "agent"
  | "claude-memory"
  | "claude-compact"
  | (string & {});

/** An open TODO extracted from history (knowledge layer). */
export interface TodoFact {
  id: number;
  threadId: number;
  text: string;
  source: SourceKind;
  title: string | null;
  projectPath: string | null;
  createdAt: number;
  extractor?: FactExtractor;
  originRef?: string | null;
}

/** Distillation engine config (shared across app/cal/MCP via the DB). */
export interface KnowledgeConfig {
  enabled: boolean;
  provider: string | null;
  model: string | null;
  autoDistill: boolean; // auto-distill new/changed threads in the background
}

export interface KFact {
  id: number;
  text: string;
  pinned: boolean;
  extractor?: FactExtractor;
  originRef?: string | null;
}

/** Two distilled decisions the LLM flagged as conflicting / superseding. */
export interface Conflict {
  aId: number;
  aText: string;
  bId: number;
  bText: string;
  reason: string;
}

/** A distilled fact in a project's aggregated memory, with its source thread. */
export interface MemoryFact {
  id: number;
  threadId: number;
  text: string;
  title: string | null;
  createdAt: number;
  pinned: boolean;
  extractor: FactExtractor;
  /** e.g. "claude-memory:<slug>/<file>" for a harvested fact; null for distilled ones. */
  originRef: string | null;
}

/** Durable, aggregated knowledge for one project + distillation coverage. */
export interface ProjectMemory {
  project: string;
  decisions: MemoryFact[];
  gotchas: MemoryFact[];
  openTodos: MemoryFact[];
  threadCount: number;
  distilledCount: number;
  pendingCount: number;
}

/** A project (by path) with thread + distillation-coverage counts, for the picker. */
export interface ProjectInfo {
  project: string;
  threadCount: number;
  distilledCount: number;
  lastActivity: number;
}

/** A thread cited as a source in an "ask your history" answer. */
export interface AskSource {
  threadId: number;
  title: string | null;
  source: SourceKind;
  projectPath: string | null;
  /** The thread's best-matching message, to open it there; absent or null when unknown. */
  messageId?: number | null;
}

export interface AskAnswer {
  answer: string;
  sources: AskSource[];
  /** `answer` plus a "Sources:" footer of `[thread N] title` lines: what the chat saves
   *  (and re-sends), so a reopened chat still knows the cited threads. */
  answerWithSources: string;
}

/** A semantically-recalled fact (decision/gotcha) with its source thread. */
export interface RecallHit {
  id: number;
  threadId: number;
  kind: string;
  text: string;
  source: SourceKind;
  title: string | null;
  projectPath: string | null;
  similarity: number;
  extractor?: FactExtractor;
  originRef?: string | null;
}

/** Distilled knowledge for one thread. */
export interface ThreadKnowledge {
  summary: string | null;
  decisions: KFact[];
  gotchas: KFact[];
  todos: KFact[];
  extracted: boolean;
  stale: boolean;
  error: string | null;
  canDistill: boolean;
}

export interface SourceStat {
  kind: SourceKind;
  threads: number;
  messages: number;
}

export interface RoleStat {
  role: string;
  messages: number;
}

export interface ProjectStat {
  project: string;
  threads: number;
}

// Rich index-wide stats (the `index_stats` command / `cal stats`), as opposed to
// the lightweight `db_stats` counts in the header.
export interface Stats {
  threads: number;
  messages: number;
  embedded: number; // distinct messages with a vector chunk
  embeddable: number; // user/assistant messages eligible for embedding
  earliest: number | null;
  latest: number | null;
  perSource: SourceStat[];
  perRole: RoleStat[];
  topProjects: ProjectStat[];
}

// One day's message activity, for the Stats activity heatmap.
export interface DayActivity {
  /** The local calendar day, "YYYY-MM-DD" (the machine's time zone). */
  day: string;
  /** Unix seconds of that day's local midnight. */
  start: number;
  /** Unix seconds of the day's last second, inclusive (a DST day is 23 or 25 hours). */
  end: number;
  messages: number;
}

// A distilled decision or gotcha in the Coach "this week" digest.
export interface CoachFact {
  id: number;
  threadId: number;
  text: string;
  title: string | null;
  project: string | null;
  createdAt: number;
}

export interface CoachOverview {
  heatmap: DayActivity[];
  decisions: CoachFact[];
  gotchas: CoachFact[];
  since: number;
}

// A prior session similar to a task ("have I done this before?"), rolled up from its
// matching decisions/gotchas.
export interface PriorWork {
  threadId: number;
  title: string | null;
  projectPath: string | null;
  source: string;
  kind: string;
  snippet: string;
  similarity: number;
  matches: number;
}

// A thread in the storage-cleanup list (oldest-first, with size).
export interface CleanupRow {
  id: number;
  source: SourceKind;
  title: string | null;
  projectPath: string | null;
  messageCount: number;
  bytes: number;
  updatedAt: number | null;
}

export interface CommitLink {
  sha: string;
  shortSha: string;
  subject: string | null;
  committedAt: number;
  overlap: number;
}

export interface IssueCluster {
  example: string;
  count: number;
  threads: number;
  firstSeen: number;
  lastSeen: number;
}

export interface ModelSpend {
  model: string;
  cost: number;
  input: number;
  output: number;
  cacheRead: number;
  calls: number;
  priced: boolean;
}
export interface ThreadCost {
  threadId: number;
  title: string | null;
  project: string | null;
  cost: number;
}
export interface Spend {
  totalCost: number;
  trackedCalls: number;
  untrackedCalls: number;
  byModel: ModelSpend[];
  topThreads: ThreadCost[];
}

/** What a "Shrink index" pass did (also the `compact:progress` payload, as running totals). */
export interface CompactSummary {
  rows: number; // tool-output rows trimmed
  bytesSaved: number;
  // The closing Reclaim space step, once the trim is done (absent in progress events and
  // from older backends). `ran: false` means the saved space stays inside the file.
  vacuum?: VacuumReport | null;
}

/** Overrides for relocated Claude Code / Codex / Copilot CLI data, plus what is in effect
 *  right now. */
export interface DataRoots {
  claudeConfigDir: string; // saved override ("" = none)
  codexHome: string;
  copilotHome: string;
  // Override, else CLAUDE_CONFIG_DIR / CODEX_HOME / COPILOT_HOME, else the default; null if
  // unresolvable.
  effectiveClaudeConfigDir: string | null;
  effectiveCodexHome: string | null;
  effectiveCopilotHome: string | null;
}

/** Which background jobs hold their run flags right now (`jobs_status`, answered from
 *  memory, so it is cheap to poll while one runs). `index` is a user-started full or
 *  single-source reindex (`index_source` refuses while it is set), `catchUp` the watcher's
 *  launch scan (a reindex started meanwhile waits for it; keep the search bar quiet for
 *  it), `rewrite` a Shrink index / Reclaim space pass. */
export interface JobsStatus {
  index: boolean;
  catchUp: boolean;
  embed: boolean;
  distill: boolean;
  rewrite: boolean;
}

/** Whether `cal watch` keeps the index fresh, so the app's own watcher is off until it stops. */
export interface LiveIndexing {
  cliWatch: boolean;
  cliWatchPid: number | null;
}

/** Background auto-distill allowance (UTC day). */
export interface AutoDistillBudget {
  eligibleThreads: number; // recently active and not yet distilled
  dailyCap: number; // threads per day; 0 pauses auto-distill
  remainingToday: number;
  recentDays: number; // eligibility window
}

/** An engine LLM distillation can run on, with whether this machine has it. `cli`: a
 *  logged-in claude / codex CLI (detail = its path or "not found"); `ollama`: a local server
 *  that answered; `key`: a cloud provider with a saved API key. */
export interface MemoryEngine {
  id: string; // provider id, e.g. "claude-cli", "ollama", "anthropic"
  label: string;
  kind: "cli" | "ollama" | "key";
  detected: boolean;
  detail: string;
}

/** What the "Turn on memory" card offers (`memory_setup`). */
export interface MemorySetup {
  enabled: boolean;
  provider: string | null;
  model: string | null;
  engines: MemoryEngine[];
  /** First detected of claude-cli, codex-cli, ollama, the first key; null if none. */
  recommended: string | null;
  eligibleThreads: number;
  dailyCap: number;
}

/** The SessionStart hook's recent-sessions digest (`hook_settings`). */
export interface HookSettings {
  digestEnabled: boolean;
  mutedProjects: string[];
}

/** Importing Claude Code auto memory into project memory (`harvest_settings`). */
export interface HarvestSettings {
  claudeMemory: boolean;
  importedFacts: number;
}

/** Events per kind in the value ledger (`ledger.rs`). */
export interface LedgerCounts {
  /** SessionStart injections of project memory. */
  hook: number;
  /** MCP tool calls answered. */
  mcp: number;
  /** Prompt-time recall hits (`recall-now`). */
  recall: number;
  /** SessionStart injections of the recent-sessions digest (repo with no memory yet). */
  digest: number;
}

/** What Callimachus did for the user's agents, and what it kept (`value_ledger`). */
export interface ValueLedger {
  /** The last 7 local days, today included. */
  week: LedgerCounts;
  allTime: number;
  preserved: {
    /** Indexed Claude Code sessions whose transcript Claude Code has since deleted. */
    threads: number;
    source: "claude_code";
    /** The user-level `cleanupPeriodDays`, null when unset. */
    cleanupPeriodDays: number | null;
    /** What Claude Code applies: the setting, else its 30-day default. */
    effectiveDays: number;
    /** The value Callimachus replaced (null: it was unset); meaningful when canRestore. */
    previousCleanupDays: number | null;
    canRestore: boolean;
  };
}

/** A saved snapshot as the app lists it (`list_snapshots`, `today`). */
export interface Snapshot {
  id: number;
  threadId: number | null;
  /** Null when the thread was deleted after the snapshot was taken. */
  threadTitle: string | null;
  /** Where the session ran (a worktree path for a worktree session). */
  projectPath: string | null;
  /** The canonical project key (a worktree's main repo), what the Project page and the
   *  search scope take; the stored path when the thread was pruned. */
  projectKey: string | null;
  sourceKind: string | null;
  label: string;
  /** The hook event of an auto snapshot ("PreCompact", ...), else "manual". */
  event: string;
  createdAt: number;
  /** Body length in chars (in a detail, of the redacted body it holds). */
  chars: number;
}

/** A snapshot plus its markdown body, redacted (`load_snapshot`). */
export interface SnapshotDetail extends Snapshot {
  body: string;
}

/** What a project's agents are given (`agent_view`). */
export interface AgentView {
  /** Exactly what the SessionStart hook prints for a new Claude Code session; "" when silent. */
  hook: string;
  hookChars: number;
  /** Why the hook would print nothing (managed block found, muted, no threads...). */
  hookSilentReason: string | null;
  /** Claude Code's cap on a hook's injected context (10,000 chars). */
  hookCap: number;
  /** The managed AGENTS.md / CLAUDE.md block as `cal agents` writes it, markers included. */
  agentsFile: string;
  agentsChars: number;
  /** The recent-sessions digest is muted for this repo (`cal hook --mute`). */
  muted: boolean;
  /** Claude Code's SessionStart hook is installed (by the app or the plugin): without it no
   *  session receives `hook`, however full the preview is. */
  hookInstalled: boolean;
  /** Whether the repo's AGENTS.md / CLAUDE.md holds this block: missing, as Update
   *  AGENTS.md would write it now, or older than the memory. */
  agentsFileState: "absent" | "current" | "stale";
}

export interface TodayCommit {
  /** Full sha (what a copy puts on the clipboard). */
  sha: string;
  shortSha: string;
  subject: string | null;
  committedAt: number;
}

export interface TodayThread {
  id: number;
  title: string | null;
  source: SourceKind;
  lastActive: number;
  durationSecs: number;
  messages: number;
  commits: TodayCommit[];
}

export interface TodayGroup {
  /** Canonical project key; "" for sessions with no project. */
  project: string;
  threads: TodayThread[];
}

export interface TodayTodo {
  id: number;
  text: string;
  project: string;
  threadId: number;
}

export interface TodayIssue {
  /** A representative error line, as the recurring errors list shows it. */
  signature: string;
  count: number;
  lastSeen: number;
  project: string;
}

/** Activity since the last open (`today`). Never involves an LLM. */
export interface Today {
  since: number;
  groups: TodayGroup[];
  newTodos: TodayTodo[];
  issuesSeenAgain: TodayIssue[];
  snapshots: Snapshot[];
  /** The 2 s git budget ran out before every repo's commits were linked. */
  commitsPartial: boolean;
  /** More sessions matched than the backend returns (its newest 200): `groups` holds only
   *  the newest. */
  truncated: boolean;
}

/** A decision or gotcha matching a search (`search_facts`). */
export interface FactHit {
  id: number;
  kind: "decision" | "gotcha";
  text: string;
  project: string;
  threadId: number;
  /** The message the fact came from, to open the thread there; null when unknown. */
  sourceMessageId: number | null;
  title: string | null;
  pinned: boolean;
  extractor: FactExtractor;
  /** When the fact was distilled (unix seconds), for Search's date range; absent from an
   *  older backend. */
  createdAt?: number | null;
}

export const api = {
  dbStats: () => invoke<DbStats>("db_stats"),
  indexStats: () => invoke<Stats>("index_stats"),
  coachOverview: () => invoke<CoachOverview>("coach_overview"),
  // Recurring errors mined across all sessions (last 180 days, most frequent first).
  recurringIssues: (project?: string) =>
    invoke<IssueCluster[]>("recurring_issues", { project: project ?? null }),
  // Estimated $ spend by model + priciest threads (needs a reindex to capture token usage).
  // `since` (epoch secs) narrows the window; omitted = all time.
  spend: (project?: string, since?: number) =>
    invoke<Spend>("spend", { project: project ?? null, since: since ?? null }),
  // Whether closing the window hides the app to the system tray instead of quitting.
  getCloseToTray: () => invoke<boolean>("get_close_to_tray"),
  setCloseToTray: (on: boolean) => invoke<void>("set_close_to_tray", { on }),
  findPriorWork: (query: string, opts?: { project?: string; limit?: number }) =>
    invoke<PriorWork[]>("find_prior_work", {
      query,
      project: opts?.project ?? null,
      limit: opts?.limit ?? null,
    }),
  // Git linkage: the commits a thread likely produced. linkThreadCommits recomputes (reads
  // the project's `git log`) then returns them; threadCommits just reads stored links.
  threadCommits: (threadId: number) => invoke<CommitLink[]>("thread_commits", { threadId }),
  linkThreadCommits: (threadId: number) =>
    invoke<CommitLink[]>("link_thread_commits", { threadId }),
  // Oldest-first threads with size, for cleanup. before = epoch secs upper bound.
  cleanupCandidates: (opts?: { before?: number; sources?: string[]; limit?: number }) =>
    invoke<CleanupRow[]>("cleanup_candidates", {
      before: opts?.before ?? null,
      sources: opts?.sources ?? null,
      limit: opts?.limit ?? null,
    }),
  deleteThreads: (ids: number[]) => invoke<number>("delete_threads", { ids }),
  vacuumDb: () => invoke<VacuumReport>("vacuum_db"),
  // "Shrink index": trim tool output indexed before the per-row cap existed, then VACUUM
  // when enough is free (the same rule as Reclaim space).
  // onProgress receives running totals while it works (the `compact:progress` event).
  compactIndex: async (onProgress?: (p: CompactSummary) => void) => {
    const unlisten = onProgress
      ? await listen<CompactSummary>("compact:progress", (e) => onProgress(e.payload))
      : null;
    try {
      return await invoke<CompactSummary>("compact_index");
    } finally {
      unlisten?.();
    }
  },
  // Relocated agent data: a Finder/Dock-launched app never sees the shell's
  // CLAUDE_CONFIG_DIR / CODEX_HOME / COPILOT_HOME. Pass "" to clear an override, omit to
  // leave it as is.
  getDataRoots: () => invoke<DataRoots>("get_data_roots"),
  setDataRoots: (roots: { claudeConfigDir?: string; codexHome?: string; copilotHome?: string }) =>
    invoke<DataRoots>("set_data_roots", {
      claudeConfigDir: roots.claudeConfigDir ?? null,
      codexHome: roots.codexHome ?? null,
      copilotHome: roots.copilotHome ?? null,
    }),
  // Write a thread as an Obsidian note (transcript + [[project]] link) into vaultDir.
  exportThread: (threadId: number, vaultDir: string) =>
    invoke<string>("export_thread", { threadId, vaultDir }),
  // Like exportThread, but prepend an LLM-synthesized summary / decisions / gotchas / TODOs.
  // provider/model empty => backend auto-picks the first provider with a stored key.
  synthesizeExport: (threadId: number, vaultDir: string, provider?: string, model?: string) =>
    invoke<string>("synthesize_export", {
      threadId,
      vaultDir,
      provider: provider || null,
      model: model || null,
    }),
  // Whether any cloud provider key is stored (gates the Synthesize action).
  canSynthesize: () => invoke<boolean>("can_synthesize"),
  // Vault folders Obsidian already knows about (from its own config) — recommendations.
  obsidianVaults: () => invoke<string[]>("obsidian_vaults"),
  // Background re-index: returns immediately; watch indexingStatus / the index:done event.
  indexAll: () => invoke<void>("index_all"),
  indexingStatus: () => invoke<boolean>("indexing_status"),
  indexSource: (kind: SourceKind) => invoke<IndexReport>("index_source", { kind }),
  jobsStatus: () => invoke<JobsStatus>("jobs_status"),
  liveIndexing: () => invoke<LiveIndexing>("live_indexing"),
  searchThreads: (query: string, filters?: SearchFilters) =>
    invoke<SearchResponse>("search_threads", { query, filters }),
  // Fire-and-forget when semantic search is switched on: loads the model (from the cache
  // only) and warms the vector index, so the first hybrid query isn't keyword-only.
  warmSemantic: () => invoke<void>("warm_semantic"),
  recentThreads: (filters?: SearchFilters) =>
    invoke<ThreadSummary[]>("recent_threads", { filters }),
  // Code-aware search: threads that mention a file path (substring).
  searchByFile: (path: string) => invoke<ThreadSummary[]>("search_by_file", { path }),
  // Project Memory: aggregated distilled knowledge per project.
  listProjects: () => invoke<ProjectInfo[]>("list_projects"),
  projectMemory: (project: string) => invoke<ProjectMemory>("project_memory", { project }),
  projectBrief: (project: string) => invoke<string>("project_brief", { project }),
  distillProject: (project: string) => invoke<void>("distill_project", { project }),
  distillingStatus: () => invoke<boolean>("distilling_status"),
  cancelDistill: () => invoke<void>("cancel_distill"),
  writeProjectMemoryFile: (project: string, withBrief: boolean) =>
    invoke<string>("write_project_memory_file", { project, withBrief }),
  // Write/refresh the managed memory block in a project's AGENTS.md / CLAUDE.md.
  writeAgentMemoryFile: (project: string, filename: string) =>
    invoke<string>("write_agent_memory_file", { project, filename }),
  // Fact curation: pin / edit / hide distilled facts; LLM conflict review.
  setFactPinned: (factId: number, pinned: boolean) =>
    invoke<void>("set_fact_pinned", { factId, pinned }),
  hideFact: (factId: number, hidden: boolean) => invoke<void>("hide_fact", { factId, hidden }),
  setTodoDone: (factId: number, done: boolean) => invoke<void>("set_todo_done", { factId, done }),
  editFact: (factId: number, text: string) => invoke<void>("edit_fact", { factId, text }),
  detectConflicts: (project: string) => invoke<Conflict[]>("detect_conflicts", { project }),
  // Write-back: record a decision or gotcha for a project. User-typed, so it is pinned.
  remember: (project: string, kind: "decision" | "gotcha", text: string) =>
    invoke<void>("remember", { project, kind, text }),
  getThread: (threadId: number) => invoke<ThreadDetail | null>("get_thread", { threadId }),
  // The full text of one message: a tool row's output, which get_thread cuts to a preview.
  messageText: (id: number) => invoke<string>("message_text", { id }),
  // Stars & tags ("collections").
  setStar: (threadId: number, starred: boolean) => invoke<void>("set_star", { threadId, starred }),
  setThreadTags: (threadId: number, tags: string[]) =>
    invoke<void>("set_thread_tags", { threadId, tags }),
  // [tag, threadCount][] sorted by count desc.
  listTags: () => invoke<[string, number][]>("list_tags"),
  // Knowledge layer: open TODOs across history (free heuristic tier).
  listOpenTodos: (query?: string, project?: string, source?: string) =>
    invoke<TodoFact[]>("list_open_todos", {
      query: query ?? null,
      project: project ?? null,
      source: source ?? null,
    }),
  // Distillation (opt-in LLM tier).
  knowledgeConfig: () => invoke<KnowledgeConfig>("knowledge_config"),
  setKnowledgeConfig: (enabled: boolean, provider?: string, model?: string) =>
    invoke<void>("set_knowledge_config", {
      enabled,
      provider: provider ?? null,
      model: model ?? null,
    }),
  // The engines the memory card can offer, with detection and the auto-distill allowance.
  memorySetup: () => invoke<MemorySetup>("memory_setup"),
  // One-click consent: turns on LLM distillation with this explicit engine (model null =
  // the engine's default) plus auto-distill, and starts the drain.
  enableMemory: (provider: string, model?: string | null) =>
    invoke<KnowledgeConfig>("enable_memory", { provider, model: model ?? null }),
  // The SessionStart hook's recent-sessions digest for repos with no memory yet.
  // The first-run setup checklist's saved step states (app_config `onboarding.steps`).
  onboardingState: () => invoke<OnboardingSteps>("onboarding_state"),
  setOnboardingStep: (step: OnboardingStep, state: OnboardingStepState) =>
    invoke<OnboardingSteps>("set_onboarding_step", { step, state }),
  hookSettings: () => invoke<HookSettings>("hook_settings"),
  setHookDigest: (on: boolean) => invoke<void>("set_hook_digest", { on }),
  // Local counts of hook injections, recall hits and MCP calls, plus the retention guard.
  valueLedger: () => invoke<ValueLedger>("value_ledger"),
  // Claude Code's transcript retention in days; null removes the key (its 30-day default).
  setClaudeCleanupDays: (days: number | null) => invoke<void>("set_claude_cleanup_days", { days }),
  // Harvest Claude Code's own auto-memory files into project memory.
  harvestSettings: () => invoke<HarvestSettings>("harvest_settings"),
  setHarvestClaudeMemory: (on: boolean) => invoke<void>("set_harvest_claude_memory", { on }),
  // Toggle background auto-distillation (turning on kicks an immediate drain).
  setAutoDistill: (on: boolean) => invoke<void>("set_auto_distill", { on }),
  autoDistillBudget: () => invoke<AutoDistillBudget>("auto_distill_budget"),
  // Threads auto-distill may send per UTC day (0 pauses it without turning it off).
  setAutoDistillDailyCap: (cap: number) =>
    invoke<AutoDistillBudget>("set_auto_distill_daily_cap", { cap }),
  threadKnowledge: (threadId: number) => invoke<ThreadKnowledge>("thread_knowledge", { threadId }),
  distillThread: (threadId: number) => invoke<ThreadKnowledge>("distill_thread", { threadId }),
  // Cross-thread semantic recall of distilled facts.
  recallDecisions: (query: string, project?: string) =>
    invoke<RecallHit[]>("recall_decisions", { query, project: project ?? null }),
  recallGotchas: (query: string, project?: string) =>
    invoke<RecallHit[]>("recall_gotchas", { query, project: project ?? null }),
  // Ask-your-history (RAG): synthesized answer + cited source threads. Saves nothing: the
  // chat store saves the turn with saveChat once the answer is shown.
  askHistory: (question: string) => invoke<AskAnswer>("ask_history", { question }),
  // Save a chat whose last reply did not come from send_chat (an Ask answer), as send_chat
  // saves one: `messages` is the conversation so far, `reply` its last assistant turn.
  saveChat: (threadId: string, messages: ChatMessage[], reply: string) =>
    invoke<void>("save_chat", { threadId, messages, reply }),
  embeddingStatus: () => invoke<EmbedStatus>("embedding_status"),
  buildEmbeddings: () => invoke<void>("build_embeddings"),
  // Stream a chat completion; onChunk fires per chunk (reasoning or answer text),
  // the promise resolves with the full reply (persisted as a searchable in_app thread).
  sendChat: (
    args: {
      threadId: string;
      provider: string;
      model: string;
      baseUrl?: string | null;
      messages: ChatMessage[];
    },
    onChunk: (c: ChatChunk) => void,
  ) => {
    const onTokenCh = new Channel<ChatChunk>();
    onTokenCh.onmessage = onChunk;
    return invoke<string>("send_chat", { onToken: onTokenCh, ...args });
  },
  setApiKey: (provider: string, key: string) => invoke<void>("set_api_key", { provider, key }),
  deleteApiKey: (provider: string) => invoke<void>("delete_api_key", { provider }),
  providerHasKey: (provider: string) => invoke<boolean>("provider_has_key", { provider }),
  // Saved custom base URL for a provider ("" = use the provider default). Used by local Ollama
  // (remote host) and Ollama Cloud / authed proxies.
  providerBaseUrl: (provider: string) => invoke<string>("provider_base_url", { provider }),
  setProviderBaseUrl: (provider: string, url: string) =>
    invoke<void>("set_provider_base_url", { provider, url }),
  resumeThread: (threadId: number) => invoke<void>("resume_thread", { threadId }),
  // Whether the backend can open a terminal here (macOS/Windows always; Linux only when a
  // terminal emulator is on PATH). Gates Resume / Open-in-CLI; fixed for the session.
  cliLaunchSupported: () => invoke<boolean>("cli_launch_supported"),
  // Packed context for a thread. `budgetChars` asks the backend for a smaller pack (its
  // budget ladder shortens tool calls/output, then thins them, then elides the middle of
  // the talk); omitted = the backend default (~48k chars). A backend without the
  // parameter ignores it.
  threadContext: (threadId: number, budgetChars?: number) =>
    invoke<string>("thread_context", { threadId, budgetChars: budgetChars ?? null }),
  openThreadInCli: (threadId: number, program?: string) =>
    invoke<string>("open_thread_in_cli", { threadId, program: program ?? null }),
  // Live model list from the provider's API (needs a key for anthropic/openai).
  listModels: (provider: string, baseUrl?: string) =>
    invoke<string[]>("list_models", { provider, baseUrl: baseUrl ?? null }),
  // Abort the in-flight chat stream; the partial reply is still saved.
  cancelChat: () => invoke<void>("cancel_chat"),
  // Approve or deny a shell command the agent asked to run.
  approveTool: (toolId: string, approved: boolean) =>
    invoke<void>("approve_tool", { toolId, approved }),
  // One-click Claude Code integration: /recall skill + self-registered MCP server.
  recallIntegrationStatus: () => invoke<IntegrationStatus>("recall_integration_status"),
  installRecallIntegration: () => invoke<IntegrationStatus>("install_recall_integration"),
  uninstallRecallIntegration: () => invoke<void>("uninstall_recall_integration"),
  // Opt-in proactive recall: inject prior work into Claude before each prompt (reads every prompt).
  setProactiveRecall: (enabled: boolean) =>
    invoke<IntegrationStatus>("set_proactive_recall", { enabled }),
  // CLI LLM backends (Claude Code / Codex) + whether each is installed — for keyless distillation.
  cliEngines: () => invoke<CliEngine[]>("cli_engines"),
  // MCP registration for the other detected agents (Codex / Cursor / Gemini).
  agentIntegrationsStatus: () => invoke<AgentIntegration[]>("agent_integrations_status"),
  installAgentIntegrations: () => invoke<AgentIntegration[]>("install_agent_integrations"),
  uninstallAgentIntegrations: () => invoke<void>("uninstall_agent_integrations"),
  // Memory hooks for Codex and Gemini CLI (SessionStart injection, SessionEnd queueing).
  agentHooksStatus: () => invoke<AgentHooksStatus[]>("agent_hooks_status"),
  installAgentHooks: (agent: HookAgentId) =>
    invoke<AgentHooksStatus>("install_agent_hooks", { agent }),
  uninstallAgentHooks: (agent: HookAgentId) =>
    invoke<AgentHooksStatus>("uninstall_agent_hooks", { agent }),
  // Snapshots for the Project page: one project's key, or null for all.
  listSnapshots: (project: string | null, limit: number) =>
    invoke<Snapshot[]>("list_snapshots", { project, limit }),
  loadSnapshot: (id: number) => invoke<SnapshotDetail>("load_snapshot", { id }),
  deleteSnapshot: (id: number) => invoke<boolean>("delete_snapshot", { id }),
  // Opens the agent CLI (claude, codex, ...) in a new terminal, seeded like `cal resume`.
  resumeSnapshot: (id: number, agent: string) => invoke<void>("resume_snapshot", { id, agent }),
  // The hook text and AGENTS.md block a project's agents get.
  agentView: (project: string) => invoke<AgentView>("agent_view", { project }),
  // Activity since `since` (null: since the last app open, else the last 24 h).
  today: (since: number | null) => invoke<Today>("today", { since }),
  // Call after the Today view has loaded, so the window shown is not the one just marked.
  markAppOpened: () => invoke<void>("mark_app_opened"),
  // The repo the Project page (and the Search scope chip) opens on.
  lastProject: () => invoke<string | null>("last_project"),
  setLastProject: (project: string | null) => invoke<void>("set_last_project", { project }),
  // Decisions and gotchas matching a search, for the facts group above the results.
  searchFacts: (query: string, project: string | null, limit: number) =>
    invoke<FactHit[]>("search_facts", { query, project, limit }),
  // Opt-in usage analytics: consent state + whether this build can send at all.
  telemetryState,
  setTelemetry,
};

/** The first-run checklist steps, in display order. */
export const ONBOARDING_STEPS = ["index", "agents", "memory", "semantic"] as const;
export type OnboardingStep = (typeof ONBOARDING_STEPS)[number];
/** A step's saved answer; null = not answered. "done" sticks once recorded, so a step the
 *  user finished stays finished when its live state later changes. Indexing can't be skipped. */
export type OnboardingStepState = "done" | "skipped" | null;
export type OnboardingSteps = Record<OnboardingStep, OnboardingStepState>;

// State of the Claude Code integration (the `/recall` skill + `callimachus` MCP server).
export interface IntegrationStatus {
  skillInstalled: boolean;
  skillOutdated: boolean;
  mcpRegistered: boolean;
  hookInstalled: boolean;
  proactiveRecallInstalled: boolean;
  calInstalled: boolean;
  /** The app runs from a permanent location, so agents can be pointed at it. */
  exeStable: boolean;
  /** Why it can't be registered from where it runs (e.g. "Move Callimachus to
   *  /Applications first"). Install refuses with this same message while it's set. */
  exeProblem: string | null;
  /** Why the `cal` CLI wasn't installed (another program owns `~/.local/bin/cal`, or a
   *  per-machine Windows install dir isn't writable). Everything else still works. */
  calProblem: string | null;
  /** Advice when `cal` is installed but a bare `cal` won't reach it from the user's
   *  shell. Ends with a copyable `export PATH=...` line. */
  pathHint: string | null;
  skillPath: string;
  configPath: string;
  /** The Callimachus Claude Code plugin is enabled (macOS and Linux), so it provides the
   *  skill, the MCP server and the hooks, and the app installs only `cal` for its hooks.
   *  Optional: an older backend doesn't send it. */
  pluginManaged?: boolean;
  /** Claude Code is set up on this machine (its config dir exists), like
   *  `AgentIntegration.present`. Optional: an older backend doesn't send it, and then the
   *  checklist offers Claude Code as before. */
  present?: boolean;
}

// A CLI LLM backend (Claude Code / Codex) usable for keyless distillation, with install state.
export interface CliEngine {
  id: string; // provider id, e.g. "claude-cli"
  label: string;
  bin: string; // binary name probed on PATH
  installed: boolean;
  path?: string | null; // where the binary resolved, when installed (optional for the mock)
}

// One non-Claude agent's MCP integration state.
export interface AgentIntegration {
  id: string;
  label: string;
  present: boolean; // the agent's config dir exists (the user uses it)
  registered: boolean;
  configPath: string;
}

/** An agent whose own hook system Callimachus can install memory hooks into. */
export type HookAgentId = "codex" | "gemini";

// One agent's memory-hook state (`agent_hooks_status`).
export interface AgentHooksStatus {
  agent: HookAgentId;
  /** The file the hooks live in: `~/.codex/hooks.json` or `~/.gemini/settings.json`. */
  configPath: string;
  installed: boolean;
  /** Why installing would do nothing (hooks turned off in the agent's config, or the agent
   *  is not set up here); the install command refuses with it. */
  blocked: string | null;
  /** Codex only: the hooks are installed but Codex will not run them until the user
   *  approves them again in /hooks (Codex re-asks when a hook's command changes, as it does
   *  after an app update or move). Optional: an older backend doesn't send it. */
  needsApproval?: boolean;
}

export interface ChatMessage {
  role: "system" | "user" | "assistant";
  content: string;
}

// Streamed chunk from send_chat. text/reasoning are answer/thinking; the tool_*
// kinds carry a tool step (toolId links a request to its result).
export interface ChatChunk {
  kind: "reasoning" | "text" | "tool_call" | "tool_request" | "tool_result";
  text: string;
  toolId?: string;
  toolName?: string;
}

// `models` are type-ahead suggestions only — the model field is free-text, so any
// model string the provider accepts works (these just save typing).
export const PROVIDERS = [
  {
    id: "anthropic",
    label: "Anthropic",
    defaultModel: "claude-opus-4-8",
    models: ["claude-opus-4-8", "claude-sonnet-4-6", "claude-haiku-4-5"],
  },
  {
    id: "openai",
    label: "OpenAI",
    defaultModel: "gpt-4o",
    models: ["gpt-4o", "gpt-4o-mini", "gpt-4.1", "o3", "o4-mini"],
  },
  {
    id: "gemini",
    label: "Gemini",
    defaultModel: "gemini-2.5-pro",
    models: ["gemini-2.5-pro", "gemini-2.5-flash", "gemini-2.0-flash"],
  },
  {
    id: "openrouter",
    label: "OpenRouter",
    defaultModel: "anthropic/claude-sonnet-4.6",
    models: [
      "anthropic/claude-sonnet-4.6",
      "anthropic/claude-opus-4.8",
      "openai/gpt-4o",
      "google/gemini-2.5-pro",
      "meta-llama/llama-3.1-70b-instruct",
    ],
  },
  {
    id: "ollama",
    label: "Ollama (local)",
    defaultModel: "llama3.1",
    models: ["llama3.1", "qwen2.5-coder", "deepseek-r1", "mistral", "gemma2"],
  },
  {
    id: "ollama_cloud",
    label: "Ollama Cloud",
    defaultModel: "gemma3:4b",
    models: ["gemma3:4b", "gpt-oss:120b", "deepseek-v3.2", "qwen3-coder:480b"],
  },
] as const;

/**
 * CLI agents a thread can be opened in, regardless of which tool created it.
 * `program` must be on PATH and accept a positional prompt arg. Edit freely.
 */
export const OPEN_TARGETS = [
  { program: "claude", label: "Claude Code" },
  { program: "codex", label: "Codex" },
  { program: "cursor-agent", label: "Cursor" },
  { program: "gemini", label: "Gemini" },
] as const;

export const SOURCE_LABELS: Record<SourceKind, string> = {
  claude_code: "Claude Code",
  codex: "Codex",
  cursor: "Cursor",
  gemini: "Gemini CLI",
  qwen: "Qwen Code",
  goose: "Goose",
  opencode: "OpenCode",
  continue: "Continue",
  cline: "Cline",
  roo: "Roo Code",
  kilo: "Kilo Code",
  copilot: "Copilot Chat",
  copilot_cli: "Copilot CLI",
  in_app: "Chat",
};

/**
 * Sources that have a filesystem indexer (everything except in-app chat, which is
 * written directly). Single source of truth for the source-filter chips and the
 * per-source reindex buttons: add a new indexer's kind here once and both update.
 */
export const INDEXABLE_SOURCES: SourceKind[] = [
  "claude_code",
  "codex",
  "cursor",
  "gemini",
  "qwen",
  "goose",
  "opencode",
  "continue",
  "cline",
  "roo",
  "kilo",
  "copilot",
  "copilot_cli",
];
