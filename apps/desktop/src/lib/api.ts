// Typed wrappers around Tauri commands. Keep all `invoke` calls here so the rest
// of the app talks to a typed surface, not stringly-named commands.
import { invoke, Channel } from "@tauri-apps/api/core";

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
  | "in_app";

export interface DbStats {
  threads: number;
  messages: number;
  sources: number;
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

export interface ThreadSummary {
  id: number;
  source: SourceKind;
  title: string | null;
  projectPath: string | null;
  messageCount: number;
  updatedAt: number | null;
  starred: boolean;
}

export interface MessageRow {
  id: number;
  role: string;
  text: string;
  toolName: string | null;
  ts: number | null;
  model: string | null;
}

export interface ThreadDetail {
  id: number;
  source: SourceKind;
  externalId: string;
  title: string | null;
  projectPath: string | null;
  gitBranch: string | null;
  createdAt: number | null;
  updatedAt: number | null;
  starred: boolean;
  tags: string[];
  messages: MessageRow[];
}

/** An open TODO extracted from history (knowledge layer). */
export interface TodoFact {
  id: number;
  threadId: number;
  text: string;
  source: SourceKind;
  title: string | null;
  projectPath: string | null;
  createdAt: number;
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
}

export interface AskAnswer {
  answer: string;
  sources: AskSource[];
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

// One day's message activity, for the Coach coding heatmap.
export interface DayActivity {
  day: number; // unix seconds at UTC midnight
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

export const api = {
  dbStats: () => invoke<DbStats>("db_stats"),
  indexStats: () => invoke<Stats>("index_stats"),
  coachOverview: () => invoke<CoachOverview>("coach_overview"),
  // Recurring errors mined across all sessions (last 180 days, most frequent first).
  recurringIssues: (project?: string) =>
    invoke<IssueCluster[]>("recurring_issues", { project: project ?? null }),
  // Estimated $ spend by model + priciest threads (needs a reindex to capture token usage).
  spend: (project?: string) => invoke<Spend>("spend", { project: project ?? null }),
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
  vacuumDb: () => invoke<void>("vacuum_db"),
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
  searchThreads: (query: string, filters?: SearchFilters) =>
    invoke<SearchHit[]>("search_threads", { query, filters }),
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
  getThread: (threadId: number) => invoke<ThreadDetail | null>("get_thread", { threadId }),
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
  // Toggle background auto-distillation (turning on kicks an immediate drain).
  setAutoDistill: (on: boolean) => invoke<void>("set_auto_distill", { on }),
  threadKnowledge: (threadId: number) => invoke<ThreadKnowledge>("thread_knowledge", { threadId }),
  distillThread: (threadId: number) => invoke<ThreadKnowledge>("distill_thread", { threadId }),
  // Cross-thread semantic recall of distilled facts.
  recallDecisions: (query: string, project?: string) =>
    invoke<RecallHit[]>("recall_decisions", { query, project: project ?? null }),
  recallGotchas: (query: string, project?: string) =>
    invoke<RecallHit[]>("recall_gotchas", { query, project: project ?? null }),
  // Ask-your-history (RAG): synthesized answer + cited source threads.
  askHistory: (question: string) => invoke<AskAnswer>("ask_history", { question }),
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
  resumeThread: (threadId: number) => invoke<void>("resume_thread", { threadId }),
  threadContext: (threadId: number) => invoke<string>("thread_context", { threadId }),
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
};

// State of the Claude Code integration (the `/recall` skill + `callimachus` MCP server).
export interface IntegrationStatus {
  skillInstalled: boolean;
  skillOutdated: boolean;
  mcpRegistered: boolean;
  hookInstalled: boolean;
  proactiveRecallInstalled: boolean;
  calInstalled: boolean;
  skillPath: string;
  configPath: string;
}

// A CLI LLM backend (Claude Code / Codex) usable for keyless distillation, with install state.
export interface CliEngine {
  id: string; // provider id, e.g. "claude-cli"
  label: string;
  bin: string; // binary name probed on PATH
  installed: boolean;
}

// One non-Claude agent's MCP integration state.
export interface AgentIntegration {
  id: string;
  label: string;
  present: boolean; // the agent's config dir exists (the user uses it)
  registered: boolean;
  configPath: string;
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
  in_app: "Chat",
};

/**
 * Sources that have a filesystem indexer (everything except in-app chat, which is
 * written directly). Single source of truth for the source-filter chips and the
 * per-source reindex buttons — add a new indexer's kind here once and both update.
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
];
