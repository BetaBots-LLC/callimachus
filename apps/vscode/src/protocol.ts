// Shared message contract between the extension host (tsc) and the webview UI
// (vite). Imported by both, so keep it free of any `vscode`, node, or DOM imports
// — pure types plus a little static data.

export type ViewId = "sidebar" | "thread";

/** Message-level search hit (mirrors the Rust `SearchHit`, camelCase JSON). */
export interface SearchHit {
  threadId: number;
  source: string;
  title: string | null;
  snippet: string;
  projectPath: string | null;
}

/** Thread summary row (mirrors the Rust `ThreadSummary`). */
export interface ThreadSummary {
  id: number;
  source: string;
  title: string | null;
  projectPath: string | null;
  messageCount: number;
  updatedAt: number | null;
}

export interface SourceStat {
  kind: string;
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

/** Corpus overview (mirrors the Rust `Stats` / `cal stats --json`). */
export interface Stats {
  threads: number;
  messages: number;
  embedded: number;
  embeddable: number;
  earliest: number | null;
  latest: number | null;
  perSource: SourceStat[];
  perRole: RoleStat[];
  topProjects: ProjectStat[];
}

/** A decision or gotcha from a session behind a commit (`cal why --json`). */
export interface WhyFact {
  id: number;
  threadId: number;
  kind: string;
  text: string;
  rationale: string | null;
  extractor: string;
  originRef: string | null;
}

/** A session inferred to have produced a blamed commit, with what it did to the file. */
export interface WhyThread {
  id: number;
  title: string | null;
  source: string;
  lastActive: number | null;
  /** Edit and write tool calls on the file inside the session. */
  edits: number;
  /** The strongest thing the session did to the file: write, edit, read or mention. */
  touch: string | null;
  overlap: number;
  facts: WhyFact[];
}

/** What a repo's Agent Trace record (agent-trace.dev, in `.agent-trace/`) says about a
 *  commit's lines: read from the repo, not inferred from a transcript in the index. */
export interface WhyTraceAttribution {
  from: "agent-trace";
  agent: string | null;
  contributor: string;
  model: string | null;
  conversationId: string | null;
  conversationUrl: string | null;
  /** The indexed session with that conversation id, when there is one. */
  threadId: number | null;
  /** Inclusive [start, end] line runs the record names; empty for the whole file. */
  ranges: [number, number][];
  revision: string;
  /** Written while the change was being made, before the commit. */
  recordedBeforeCommit: boolean;
  recordedAt: string | null;
  traceFile: string;
}

export interface WhyCommit {
  sha: string;
  shortSha: string;
  subject: string;
  committedAt: number;
  lines: number;
  /** The lines the commit owns, inclusive [start, end] runs (absent from an older `cal`). */
  ranges?: [number, number][];
  threads: WhyThread[];
  /** Agent Trace attributions (absent when there are none, or from an older `cal`). */
  trace?: WhyTraceAttribution[];
}

/** `cal why <file>:<line> --json` (schema: callimachus.app/schemas/cal/why.v1.json). */
export interface WhyResult {
  file: string;
  line: number | null;
  repo: string;
  commits: WhyCommit[];
  /** No commit here links to a session yet (`cal commits` links the repo's history). */
  unlinked: boolean;
  /** The line has changes no commit owns yet. */
  uncommitted: boolean;
}

/** The sidebar's "Why this line" section: asking, the answer, or why there is none. */
export type WhyState =
  | { status: "loading"; file: string; line: number }
  | { status: "done"; file: string; line: number; result: WhyResult }
  | { status: "error"; file: string; line: number; message: string };

/** RPC methods (webview -> host, awaiting a reply). */
export interface RpcMap {
  search: { params: { query: string; project?: string | null }; result: SearchHit[] };
  recent: { params: Record<string, never>; result: ThreadSummary[] };
  stats: { params: Record<string, never>; result: Stats };
  cat: { params: { id: number }; result: string };
}
export type RpcMethod = keyof RpcMap;

/** Fire-and-forget actions (webview -> host). */
export type ActionName =
  | "openThread"
  | "insertThread"
  | "copyThread"
  | "exportThread"
  | "openInCli";

/** Init payload the host pushes once the webview signals readiness. */
export interface InitPayload {
  view: ViewId;
  // sidebar
  query?: string;
  projectPath?: string | null;
  // thread
  threadId?: number;
  title?: string | null;
}

/** webview -> host envelope. */
export type FromWebview =
  | { kind: "ready" }
  | { kind: "rpc"; id: number; method: RpcMethod; params: unknown }
  | { kind: "action"; action: ActionName; id: number; title?: string | null }
  // The user closed the "Why this line" section.
  | { kind: "closeWhy" };

/** host -> webview envelope. */
export type ToWebview =
  | { kind: "init"; init: InitPayload }
  | { kind: "rpc-result"; id: number; ok: true; result: unknown }
  | { kind: "rpc-result"; id: number; ok: false; error: string }
  | { kind: "refresh" }
  // Ambient recall: the host computes related threads from the editor context and
  // pushes them; `label` is a short hint of what matched (a symbol, "selection"…).
  | { kind: "related"; label: string; results: ThreadSummary[] }
  // "Why this line": the host runs `cal why` for the editor line and pushes each state;
  // null clears the section.
  | { kind: "why"; state: WhyState | null };

/** Human labels for the source kinds (kept in sync with the desktop app). */
export const SOURCE_LABELS: Record<string, string> = {
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

export const sourceLabel = (kind: string): string => SOURCE_LABELS[kind] ?? kind;
