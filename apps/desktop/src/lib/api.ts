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

export interface SearchFilters {
  sources?: string[];
  project?: string | null;
  after?: number | null;
  before?: number | null;
  limit?: number | null;
  includeSubagents?: boolean;
  hybrid?: boolean;
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
}

export interface MessageRow {
  id: number;
  role: string;
  text: string;
  toolName: string | null;
  ts: number | null;
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
  messages: MessageRow[];
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

export const api = {
  dbStats: () => invoke<DbStats>("db_stats"),
  indexStats: () => invoke<Stats>("index_stats"),
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
  indexAll: () => invoke<IndexReport>("index_all"),
  indexSource: (kind: SourceKind) => invoke<IndexReport>("index_source", { kind }),
  searchThreads: (query: string, filters?: SearchFilters) =>
    invoke<SearchHit[]>("search_threads", { query, filters }),
  recentThreads: (filters?: SearchFilters) =>
    invoke<ThreadSummary[]>("recent_threads", { filters }),
  getThread: (threadId: number) => invoke<ThreadDetail | null>("get_thread", { threadId }),
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
};

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
];
