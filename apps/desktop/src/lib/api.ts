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

/** One snapshot of the index (`backup::Snapshot`): a copy kept in the snapshots folder next to
 *  it. `privateBytes` is what it costs on disk now (an APFS clone starts at 0 and grows as
 *  the index changes); null where the system does not report it. `otherIndex`: an automatic
 *  snapshot of an earlier index (before a fresh start), never removed automatically. */
export interface IndexSnapshot {
  name: string;
  takenAt: number; // unix seconds
  reason: "pre-migration" | "weekly" | "manual";
  bytes: number;
  privateBytes: number | null;
  otherIndex: boolean;
}

/** The index's snapshots for Settings (`backup::Status`). `automatic`: they are taken on their
 *  own here (macOS, with the index on a disk that clones); elsewhere only `cal backup` makes
 *  one. `lastCheck.at` is 0 for a failed check carried across Shrink index / Reclaim space. */
export interface SnapshotStatus {
  automatic: boolean;
  folder: string;
  snapshots: IndexSnapshot[];
  bytesOnDisk: number | null;
  lastCheck: { at: number; ok: boolean; ms: number } | null;
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
 *  file), "claude-compact" (a PreCompact compaction summary) or "playbook" (the fix of a
 *  recurring error, recorded when a linked commit fixed it). Older backends omit it. */
export type FactExtractor =
  | "llm"
  | "heuristic"
  | "user"
  | "agent"
  | "claude-memory"
  | "claude-compact"
  /** Imported from the repo's committed `.callimachus/memory/` files. */
  | "repo"
  | "playbook"
  /** A TODO from a list an agent titled "Follow-ups", "Not done", "Deferred" or "Remaining". */
  | "followup"
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

/** Two distilled decisions the LLM flagged as conflicting / superseding, with when each was
 *  decided. Nothing is stored until the user acts on it. */
export interface Conflict {
  aId: number;
  aText: string;
  aDecidedAt: number;
  bId: number;
  bText: string;
  bDecidedAt: number;
  /** The model's reading: one replaced the other, or they disagree. */
  kind: "supersedes" | "conflict";
  /** The newer side, by date whenever the two dates differ. */
  newer: "a" | "b";
  reason: string;
}

/** Where a fact stands: replaced by a newer one, flagged stale / wrong, disputed by another,
 *  or none of those. */
export type FactStatus = "current" | "disputed" | "flagged" | "superseded";

/** The other side of a link: what replaced a fact, or what disputes it. */
export interface LinkedFact {
  id: number;
  text: string;
  /** Epoch seconds: its session's start, or when it was recorded. */
  decidedAt: number;
  /** Its session thread; absent for a recorded or imported fact. */
  threadId?: number;
}

/** An unresolved stale / wrong flag on a fact. */
export interface FactFlag {
  id: number;
  kind: "stale" | "wrong";
  /** An agent (`flag_fact`), a memory pass (the distiller retired it), or the user. */
  source: "agent" | "distill" | "user";
  reason?: string;
  createdAt: number;
  threadId?: number;
}

/** A distilled fact in a project's aggregated memory, with its source thread, when it was
 *  decided and where it stands. */
export interface MemoryFact {
  id: number;
  threadId: number;
  text: string;
  title: string | null;
  createdAt: number;
  pinned: boolean;
  /** The user rewrote its text: curated like a pinned fact, so a flag leaves it in memory.
   *  Absent when false. */
  edited?: boolean;
  extractor: FactExtractor;
  /** e.g. "claude-memory:<slug>/<file>" for a harvested fact; null for distilled ones. */
  originRef: string | null;
  kind: "decision" | "gotcha" | "todo";
  /** Epoch seconds: a distilled fact's session start, else its own time. */
  decidedAt: number;
  /** Its thread's source (`in_app` for recorded and imported facts). */
  source: SourceKind;
  status: FactStatus;
  /** The ids of the facts it actively supersedes, newest first (absent when none): the same
   *  key as on a recall or check hit. */
  replaces?: number[];
  /** `decidedAt` of the newest of those, the `replaces <date>` in its memory line. */
  replacesDecidedAt?: number;
  /** The facts it is disputed with, newest first. */
  disputedBy?: LinkedFact[];
  /** 1 + the other sessions that confirmed it. */
  support: number;
  /** The newest start among those sessions. */
  lastConfirmed?: number;
  /** What replaced it (superseded facts only). */
  supersededBy?: LinkedFact;
  /** Its unresolved flags (flagged facts only). */
  flags?: FactFlag[];
}

/** Durable, aggregated knowledge for one project + distillation coverage. The three lists
 *  hold live facts (a flagged one only when pinned or edited); `superseded` and `flagged`
 *  are the history the Projects page lists apart, absent when empty. */
export interface ProjectMemory {
  project: string;
  decisions: MemoryFact[];
  gotchas: MemoryFact[];
  openTodos: MemoryFact[];
  superseded?: MemoryFact[];
  flagged?: MemoryFact[];
  threadCount: number;
  distilledCount: number;
  pendingCount: number;
}

/** `supersedes`: the first fact replaces the second. `contradicts`: they disagree. */
export type LinkKind = "supersedes" | "contradicts";

/** Why `link_facts` stored nothing. */
export type LinkRejection =
  | "notFound"
  | "notLinkable"
  | "kindMismatch"
  | "crossProject"
  | "selfLink"
  | "cycle"
  | "wrongDirection"
  | "alreadySettled"
  | "dismissed";

/** What `link_facts` did. A user link is never downgraded; the variant exists for agents. */
export type LinkOutcome =
  | { outcome: "linked"; id: number; kind: LinkKind }
  | { outcome: "downgraded"; id: number }
  | { outcome: "rejected"; reason: LinkRejection };

/** A project's repo memory (`.callimachus/memory/` in its checkout). */
export interface RepoMemoryStatus {
  /** The git checkout; null when the project has none on this machine. */
  root: string | null;
  /** Whether `.callimachus/memory/` exists there. */
  published: boolean;
  /** Ids of this project's own facts that the files also hold. */
  sharedIds: number[];
  /** Visible facts imported from the files. */
  imported: number;
}

/** One fact a publish adds, changes or removes (text redacted). */
export interface RepoFactChange {
  uid: string;
  kind: "decision" | "gotcha";
  text: string;
}

/** Why publish holds a fact back: `flagged` stale or wrong and not reviewed yet (and neither
 *  pinned nor edited; wins when both apply), or `replacementUnpublished`: replaced by facts
 *  none of which this publish writes. */
export type RepoHeldReason = "flagged" | "replacementUnpublished";

/** A fact publish holds back, and why. */
export interface RepoHeldFact extends RepoFactChange {
  reason: RepoHeldReason;
}

/** What "Publish to repo" would write (the preview) or wrote. */
export interface RepoPublishPlan {
  root: string;
  dir: string;
  /** Portable project id written on each line, e.g. "github.com/acme/app". */
  project: string | null;
  all: boolean;
  decisions: number;
  gotchas: number;
  added: RepoFactChange[];
  updated: RepoFactChange[];
  removed: RepoFactChange[];
  /** Facts publish would otherwise write but holds back, since they are out of memory here
   *  and would read as current to a teammate: flagged stale or wrong and not reviewed yet
   *  (and neither pinned nor edited), or replaced by a fact that is not published. An
   *  earlier line of one is in `removed`. */
  held: RepoHeldFact[];
  /** False when the files already hold exactly this. */
  changed: boolean;
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

/** A past session offered when recall has no fact to return: by keyword without the
 *  embedding model, or the closest by meaning in a scope that was never distilled. */
export interface ClosestSession {
  id: number;
  title: string | null;
  /** The best-matching message, `role: text`, redacted and clipped. */
  snippet: string;
}

/** `recall_decisions` / `recall_gotchas`: the hits, or why there are none and what to open
 *  instead (`knowledge::RecallOutcome`). `noModel`: show the sessions with a hint to
 *  download the model (Settings, semantic search). */
export type RecallOutcome =
  | { kind: "hits"; hits: RecallHit[] }
  | {
      kind: "thin";
      coverage: { distilledThreads: number; threads: number };
      sessions: ClosestSession[];
      distillOn: boolean;
    }
  | { kind: "noModel"; sessions: ClosestSession[] };

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

/** The commit that fixed a recurring error: the latest session that hit it, then edited the
 *  file the error named and committed. */
export interface IssueFix {
  sha: string;
  shortSha: string;
  subject: string | null;
  committedAt: number;
  /** The session that hit the error and made the fix. */
  threadId: number;
}

/** A recurring error with its sessions and, once a commit fixed it, the fix and the playbook
 *  line recorded as a gotcha. Older backends return the bare cluster (no `fix`). */
export interface Issue extends IssueCluster {
  /** Sessions that hit it, newest sighting first (at most 10). */
  threadIds?: number[];
  /** Null while no fix is known, or when the fix landed in another project. */
  fix?: IssueFix | null;
  playbook?: string | null;
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
/** One local calendar day's estimated spend. */
export interface DayCost {
  day: string; // YYYY-MM-DD, local time
  cost: number;
  input: number;
  output: number;
  cacheWrite: number;
  cacheRead: number;
}
export interface CommitSession {
  id: number;
  title: string | null;
  cost: number; // the session's whole cost, subagents included
  overlap: number;
  commits: number; // listed commits this session also produced
}
/** What the sessions behind one commit cost. */
export interface CommitCost {
  sha: string;
  shortSha: string;
  subject: string | null;
  committedAt: number;
  project: string | null;
  cost: number;
  shared: boolean; // a session behind it produced another commit too
  sessions: CommitSession[];
}
export interface SpendBreakdown {
  days: DayCost[]; // the last 30 local days, oldest first, zero days included
  commits: CommitCost[]; // the priciest commits, at most 5
}

/** One model's spend in a report window (`cost::ModelCost`). */
export interface ModelCost {
  model: string;
  family: string | null;
  priced: boolean;
  unpricedReason: string | null;
  cost: number;
  calls: number;
  input: number;
  output: number;
  cacheWrite: number;
  cacheRead: number;
}

/** "Your agents, so far" (`report.rs`, also `cal report --json`). A section with no data is
 *  absent, never empty. */
export interface Report {
  generatedAt: number;
  since: number | null; // epoch secs; null = all time
  totals?: ReportTotals;
  spend?: ReportSpend;
  recurringErrors?: ReportIssue[];
  solvedAgain?: PromptGroup[];
  memory?: ReportMemory;
  files?: ReportFile[];
  filesOmitted?: number; // files outside a project folder, left out of `files`
  ledger?: ReportLedger;
}
export interface ReportTotals {
  sessions: number; // top-level sessions; subagents and in-app chats left out
  messages: number;
  firstSession: number;
  lastSession: number;
  activeDays: number;
  estimatedHours: number; // each session's span capped at 8 hours, over hoursSessions
  hoursSessions: number; // sessions with a usable span
  hoursSources: SourceKind[]; // the agents those sessions ran in, most sessions first
  byTool: { source: SourceKind; label: string; sessions: number; share: number }[];
}
export interface ReportSpendWindow {
  since: number;
  totalCost: number;
  trackedCalls: number;
  untrackedCalls: number;
  byTool: { source: SourceKind; label: string; cost: number; calls: number; share: number }[];
  byModel: ModelCost[];
}
export interface ReportSpend {
  basis: string; // list-price equivalents, not a bill
  total: ReportSpendWindow;
  last30Days: ReportSpendWindow;
  priciestCommits: CommitCost[];
}
export interface ReportIssue {
  example: string;
  count: number;
  sessions: number;
  firstSeen: number;
  lastSeen: number;
  project: string | null;
  fix: IssueFix | null;
}
export interface PromptGroup {
  prompt: string;
  sessions: number;
  firstSeen: number;
  lastSeen: number;
  project: string | null;
  threadIds: number[];
}
export interface ReportMemory {
  decisions: number;
  gotchas: number;
  openTodos: number;
  projectsWithMemory: number;
  topDecisionProjects: { project: string; decisions: number }[];
  injected: {
    injections: number;
    sessions: number;
    share: number;
    sinceDay: string;
  } | null;
}
export interface ReportFile {
  path: string;
  project: string | null;
  edits: number;
  sessions: number;
}
export interface ReportLedger {
  counts: LedgerCounts;
  sinceDay: string | null;
  allTime: number;
}
/** The `report` command's answer: the report and the markdown `cal report` prints. */
export interface ReportView {
  report: Report;
  markdown: string;
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
  /** Error-playbook injections after a failed shell command (`recall-error`). */
  playbook: number;
}

/** What Settings > General's Copy diagnostics copies (`diagnostics`): `cal doctor`'s report
 *  as text, plus the structure (`schemas/cal/doctor.v1.json` without `schema_version`). Paths
 *  show the home folder as `~`; no project text. */
export interface Diagnostics {
  text: string;
  report: {
    version: string;
    platform: string;
    /** What looks wrong, in plain words. */
    problems: string[];
    [key: string]: unknown;
  };
}

/** A source whose last indexing pass read like a transcript format change (`parse_drifts`,
 *  `indexer::parse_health`). Counts and type names only. */
export interface ParseDrift {
  source: SourceKind;
  /** When the pass ran (unix seconds). */
  at: number;
  /** Unknown plus malformed records in that pass, 0 to 1. */
  share: number;
  /** The same share over the source's earlier passes. */
  baseline: number;
  malformed: number;
  /** Its unknown record types, most frequent first. */
  kinds: string[];
  files: number;
  zeroFiles: number;
  byShare: boolean;
  byZeroFiles: boolean;
  dismissed: boolean;
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

/** Why a fact is on Today's "Memory to review" list (`today::ReviewReason`). */
export type ReviewReason = "flagged" | "disputed" | "replaced" | "recorded";

/** Something an agent or a memory pass did to memory that nobody reviewed. */
export interface ReviewItem {
  reason: ReviewReason;
  /** The fact the item is about (for `replaced`, the one that was replaced). */
  factId: number;
  kind: "decision" | "gotcha";
  text: string;
  project: string;
  /** When the flag, the link or the recording happened (unix secs). */
  at: number;
  /** `agent`, `distill` (a memory pass), or a dispute's source (`repo`, `review`, `user`). */
  source: string;
  /** What replaced it, or what disputes it. */
  other?: { id: number; text: string };
  /** A flag's kind. */
  flag?: "stale" | "wrong";
  /** A flag's reason. */
  note?: string;
  /** Pinned or edited by the user: a flag leaves it in memory (when `injectable`). */
  trusted: boolean;
  /** Injected memory can carry it: false for an unpinned playbook or subagent fact, where a
   *  flag (or keeping it) changes nothing an agent is shown. */
  injectable: boolean;
}

/** Activity since the last open (`today`). Never involves an LLM. */
export interface Today {
  since: number;
  groups: TodayGroup[];
  newTodos: TodayTodo[];
  issuesSeenAgain: TodayIssue[];
  snapshots: Snapshot[];
  /** Memory to review; absent from an older backend. */
  memoryReview?: ReviewItem[];
  /** How many more items there are to review than `memoryReview` lists (the backend stops
   *  at 50). */
  memoryReviewMore?: number;
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
    invoke<Issue[]>("recurring_issues", { project: project ?? null }),
  // Estimated $ spend by model + priciest threads (needs a reindex to capture token usage).
  // `since` (epoch secs) narrows the window; omitted = all time.
  spend: (project?: string, since?: number) =>
    invoke<Spend>("spend", { project: project ?? null, since: since ?? null }),
  // Spend per local day (last 30) and the priciest commits the sessions produced.
  spendBreakdown: (project?: string) =>
    invoke<SpendBreakdown>("spend_breakdown", { project: project ?? null }),
  // "Your agents, so far": the whole index, or sessions since `since` (epoch secs).
  report: (since?: number) => invoke<ReportView>("report", { since: since ?? null }),
  // Save the report card (PNG bytes) to Downloads; returns the written path.
  saveReportCard: (png: Uint8Array) => invoke<string>("save_report_card", { png: Array.from(png) }),
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
  // The index's snapshots (before upgrades, weekly, `cal backup`) and what they cost on disk.
  indexSnapshots: () => invoke<SnapshotStatus>("index_snapshots"),
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
  // Repo memory: `.callimachus/memory/*.jsonl` in the project's checkout.
  repoMemoryStatus: (project: string) =>
    invoke<RepoMemoryStatus>("repo_memory_status", { project }),
  previewRepoMemory: (project: string, all: boolean) =>
    invoke<RepoPublishPlan>("preview_repo_memory", { project, all }),
  publishRepoMemory: (project: string, all: boolean) =>
    invoke<RepoPublishPlan>("publish_repo_memory", { project, all }),
  // Fact curation: pin / edit / hide distilled facts; LLM conflict review.
  setFactPinned: (factId: number, pinned: boolean) =>
    invoke<void>("set_fact_pinned", { factId, pinned }),
  hideFact: (factId: number, hidden: boolean) => invoke<void>("hide_fact", { factId, hidden }),
  setTodoDone: (factId: number, done: boolean) => invoke<void>("set_todo_done", { factId, done }),
  editFact: (factId: number, text: string) => invoke<void>("edit_fact", { factId, text }),
  detectConflicts: (project: string) => invoke<Conflict[]>("detect_conflicts", { project }),
  // Fact truth: the user links two facts (the first replaces or disputes the second), undoes
  // a link (it stays undone: a re-distill cannot declare it again), or keeps a flagged fact.
  linkFacts: (fromId: number, toId: number, kind: LinkKind) =>
    invoke<LinkOutcome>("link_facts", { fromId, toId, kind }),
  unlinkFacts: (fromId: number, toId: number, kind?: LinkKind) =>
    invoke<number>("unlink_facts", { fromId, toId, kind: kind ?? null }),
  // Undo every replacement of a fact at once: it is current again, however many replaced it.
  restoreFact: (factId: number) => invoke<number>("restore_fact", { factId }),
  // Keep a recorded or replaced fact on Today's review list: it stays off that list and
  // `cal memory review`. A flag or a dispute is kept by settling it (resolveFactFlags,
  // unlinkFacts). `otherId`: the fact that replaced a replaced one.
  keepReviewItem: (reason: "recorded" | "replaced", factId: number, otherId?: number) =>
    invoke<void>("keep_review_item", { reason, factId, otherId: otherId ?? null }),
  resolveFactFlags: (factId: number) => invoke<number>("resolve_fact_flags", { factId }),
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
  // `cal doctor`'s report for Settings > General's Copy diagnostics.
  diagnostics: () => invoke<Diagnostics>("diagnostics"),
  // Sources whose transcripts may have changed format (Settings > Index), and dismissing one.
  parseDrifts: () => invoke<ParseDrift[]>("parse_drifts"),
  dismissParseDrift: (source: string) => invoke<void>("dismiss_parse_drift", { source }),
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
  // Cross-thread semantic recall of distilled facts, or the closest sessions when there is
  // no model or no distilled memory in scope.
  recallDecisions: (query: string, project?: string) =>
    invoke<RecallOutcome>("recall_decisions", { query, project: project ?? null }),
  recallGotchas: (query: string, project?: string) =>
    invoke<RecallOutcome>("recall_gotchas", { query, project: project ?? null }),
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
  // Opt-in error playbooks: after each shell command Claude runs, add the fix of an error a
  // commit fixed before (runs after every shell command).
  setErrorRecall: (enabled: boolean) => invoke<IntegrationStatus>("set_error_recall", { enabled }),
  // CLI LLM backends (Claude Code / Codex) + whether each is installed — for keyless distillation.
  cliEngines: () => invoke<CliEngine[]>("cli_engines"),
  // MCP registration for the other detected agents (Codex / Cursor / Gemini).
  agentIntegrationsStatus: () => invoke<AgentIntegration[]>("agent_integrations_status"),
  installAgentIntegrations: () => invoke<AgentIntegration[]>("install_agent_integrations"),
  uninstallAgentIntegrations: () => invoke<void>("uninstall_agent_integrations"),
  // Memory hooks for Codex, Gemini CLI, GitHub Copilot and Cursor (session-start injection,
  // session-end queueing; Codex's also snapshot before compaction).
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
export const ONBOARDING_STEPS = ["index", "agents", "memory", "semantic", "report"] as const;
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
  /** The opt-in error-playbook hooks (PostToolUse / PostToolUseFailure on Bash). Older
   *  backends omit it. */
  errorRecallInstalled?: boolean;
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
export type HookAgentId = "codex" | "gemini" | "copilot" | "cursor";

// One agent's memory-hook state (`agent_hooks_status`).
export interface AgentHooksStatus {
  agent: HookAgentId;
  /** The agent's config dir exists here. Optional: an older backend doesn't send it (then
   *  the MCP rows' `present` stands in, which has no Copilot row). */
  present?: boolean;
  /** The file the hooks live in: `~/.codex/hooks.json`, `~/.gemini/settings.json`,
   *  `~/.copilot/hooks/callimachus.json` or `~/.cursor/hooks.json`. */
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
