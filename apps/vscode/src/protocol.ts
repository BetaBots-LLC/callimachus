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
  | { kind: "action"; action: ActionName; id: number; title?: string | null };

/** host -> webview envelope. */
export type ToWebview =
  | { kind: "init"; init: InitPayload }
  | { kind: "rpc-result"; id: number; ok: true; result: unknown }
  | { kind: "rpc-result"; id: number; ok: false; error: string }
  | { kind: "refresh" };

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
  in_app: "Chat",
};

export const sourceLabel = (kind: string): string => SOURCE_LABELS[kind] ?? kind;
