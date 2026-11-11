// The Memory view's model, with no `vscode` import (apps/vscode/test drives it with
// `node --test`): the only two `cal` calls the view makes, their parsing, what a failure
// means for the view, and the rows it shows. memoryView.ts turns the rows into TreeItems.
//
// Shapes follow the checked-in schemas (schemas/cal/memory.v1.json and issues.v1.json, v1)
// and the bare shapes an older `cal` prints without CAL_JSON_VERSION support (before 0.12):
// a fact with only id, threadId, text, title, createdAt, pinned, extractor and originRef,
// and an issue without threadIds, fix or playbook.
//
// The view is read-only. `cal memory`'s FIRST argument routes to `publish`, `import` and
// `review` (cli_core.rs cmd_memory), and the first two write, so `memory` gets no positional
// at all: it scopes to its working directory instead (`knowledge::cwd_project_key`, which
// resolves a git worktree to its main repo). Every call goes through assertReadOnlyCall.

import {
  CalSetupError,
  CalStateError,
  CalVersionError,
  parseList,
  parseObject,
  type RunCalOptions,
} from "./calCore";
import { sourceLabel } from "./protocol";

/** One fact of `cal memory --json` (memory.v1.json `MemoryFact`). The fields after
 *  `originRef` came with 0035 and are absent from an older `cal`'s bare output. */
export interface MemoryFact {
  id: number;
  threadId: number;
  text: string;
  /** The title of the session it came from. */
  title: string | null;
  createdAt: number;
  pinned: boolean;
  extractor: string;
  originRef: string | null;
  kind?: string;
  decidedAt?: number;
  source?: string;
  status?: "current" | "disputed" | "flagged" | "superseded";
  support?: number;
  disputedBy?: { id: number; text: string; decidedAt: number; threadId?: number | null }[];
}

/** `cal memory --json` (memory.v1.json). `superseded` / `flagged` come only with
 *  `--include-superseded`, which the view never passes. */
export interface ProjectMemory {
  project: string;
  decisions: MemoryFact[];
  gotchas: MemoryFact[];
  openTodos: MemoryFact[];
  threadCount?: number;
  distilledCount?: number;
  pendingCount?: number;
}

/** issues.v1.json `IssueFix`. */
export interface IssueFix {
  sha: string;
  shortSha: string;
  subject: string | null;
  committedAt: number;
  threadId: number;
}

/** One recurring error of `cal issues --json` (issues.v1.json `Issue`). */
export interface Issue {
  example: string;
  count: number;
  threads: number;
  firstSeen: number;
  lastSeen: number;
  threadIds?: number[];
  fix?: IssueFix | null;
  playbook?: string | null;
}

/** Facts per kind asked of `cal memory -n` (cal's own default, and the extension's default
 *  result limit). */
export const MEMORY_PER_KIND = 40;
/** Recurring errors asked of `cal issues -n` (cal's default). */
export const ISSUES_LIMIT = 20;
/** `cal memory` answered in 23 to 240 ms for three repos of a 5.4 GB index (Batch L debug
 *  build, 2026-11-10); past this something is stuck. */
export const MEMORY_TIMEOUT_MS = 30_000;
/** `cal issues <project>` mines that project's sessions live instead of reading the
 *  issue_clusters cache: 3.6 s to 20.8 s for the same three repos with that debug build
 *  (SQLite unoptimized; a release build is faster). It runs only when the group is
 *  expanded, and gives up after this, as `cal why` does. */
export const ISSUES_TIMEOUT_MS = 60_000;

/** `cal memory --json -n <perKind>`, with no positional (see the module note). */
export function memoryArgs(perKind: number = MEMORY_PER_KIND): string[] {
  const n = Number.isFinite(perKind) ? Math.min(Math.max(Math.floor(perKind), 1), 500) : 40;
  return ["memory", "--json", "-n", String(n)];
}

/** `cal issues <project> --json -n <limit>`, or null when there is no project to scope to.
 *  `project` is the memory answer's canonical `project` (so a worktree asks for its main
 *  repo's errors, whose `project_key` is the main repo path). Without a positional `cal
 *  issues` lists every project's errors, so an empty project is skipped, not sent; one that
 *  starts with `-` would be read as a flag, and is skipped too. `cal issues` matches the
 *  positional as a substring, so `/r/app` also lists `/r/app-web`'s errors (cmd_issues), and
 *  a project that holds every project is skipped as well (holdsEveryProject). */
export function issuesArgs(
  project: string,
  home?: string,
  limit: number = ISSUES_LIMIT,
): string[] | null {
  const p = project.trim();
  if (!p || p.startsWith("-") || holdsEveryProject(p, home)) return null;
  const n = Number.isFinite(limit) ? Math.min(Math.max(Math.floor(limit), 1), 200) : 20;
  return ["issues", p, "--json", "-n", String(n)];
}

/** Whether `project` is a filesystem root (`/`, a bare drive), the home folder or a folder
 *  above it. A folder that is no repo keys as itself (`indexer::canonical_project`), and
 *  `cal issues` turns its positional into `LIKE '%<project>%'`, so a window opened at home
 *  listed every project's errors as this one's, after mining all of them (the slowest case).
 *  `home` comes from the caller, canonical as cal keys it (symlinks resolved). */
export function holdsEveryProject(project: string, home?: string): boolean {
  const strip = (s: string) => s.trim().replace(/[\\/]+$/, "");
  if (!project.trim()) return false; // no project at all, which issuesArgs skips anyway
  const p = strip(project);
  if (!p || /^[A-Za-z]:$/.test(p)) return true;
  const h = strip(home ?? "");
  if (!h) return false;
  // Drive paths compare without case, as Windows does.
  const [a, b] = /^[A-Za-z]:/.test(p) ? [p.toLowerCase(), h.toLowerCase()] : [p, h];
  return b === a || b.startsWith(`${a}/`) || b.startsWith(`${a}\\`);
}

/** `cal` subcommands that write the index or the checkout (repo CLAUDE.md, Data safety;
 *  cli_core.rs USAGE): the view must never be able to send one. */
export const WRITE_COMMANDS: readonly string[] = [
  "star",
  "tag",
  "distill",
  "done",
  "remember",
  "snapshot",
  "commits",
  "audit-pr",
  "index",
  "watch",
  "embed",
  "backup",
  "trace",
  "hook",
  "snapshot-hook",
  "session-end",
  "recall-now",
  "recall-error",
  "publish",
  "import",
  "review",
];

/** Refuse anything but the two calls above: `memory` with flags only, or `issues` with one
 *  project and flags. Thrown before a process starts. */
export function assertReadOnlyCall(args: readonly string[]): void {
  const [command, ...rest] = args;
  const refuse = (why: string): never => {
    throw new Error(`The Memory view refused to run \`cal ${args.join(" ")}\`: ${why}`);
  };
  if (args.some((a) => WRITE_COMMANDS.includes(a))) refuse("it names a write command");
  if (command === "memory") {
    // Flags and their values only: `--json`, `-n <number>`.
    for (let i = 0; i < rest.length; i++) {
      if (rest[i] === "--json") continue;
      if (rest[i] === "-n" && /^\d+$/.test(rest[i + 1] ?? "")) {
        i++;
        continue;
      }
      refuse("`cal memory` takes no positional here");
    }
    return;
  }
  if (command === "issues") {
    const [project, ...flags] = rest;
    if (!project || project.startsWith("-")) refuse("`cal issues` needs a project");
    if (flags.length !== 3 || flags[0] !== "--json" || flags[1] !== "-n") {
      refuse("unexpected flags");
    }
    if (!/^\d+$/.test(flags[2] ?? "")) refuse("unexpected flags");
    return;
  }
  refuse("only `cal memory` and `cal issues` are read here");
}

const isNum = (v: unknown): v is number => typeof v === "number" && Number.isFinite(v);

function factsOf(v: unknown): MemoryFact[] {
  if (!Array.isArray(v)) return [];
  return v.filter(
    (f): f is MemoryFact =>
      typeof f === "object" && f !== null && isNum(f.id) && typeof f.text === "string",
  );
}

/** Read `cal memory --json` stdout: the versioned object, or the bare one an older `cal`
 *  prints. A `schema_version` above 1 throws CalVersionError. */
export function parseMemory(raw: string): ProjectMemory {
  const m = parseObject<Record<string, unknown>>(raw, "memory");
  if (typeof m.project !== "string" || !Array.isArray(m.decisions)) {
    throw new Error("`cal memory` returned JSON without a project's memory. Update the cal CLI?");
  }
  return {
    project: m.project,
    decisions: factsOf(m.decisions),
    gotchas: factsOf(m.gotchas),
    openTodos: factsOf(m.openTodos),
    threadCount: isNum(m.threadCount) ? m.threadCount : undefined,
    distilledCount: isNum(m.distilledCount) ? m.distilledCount : undefined,
    pendingCount: isNum(m.pendingCount) ? m.pendingCount : undefined,
  };
}

/** Read `cal issues --json` stdout: the `{schema_version, results}` envelope, or the bare
 *  array of clusters an older `cal` prints. A `schema_version` above 1 throws. */
export function parseIssues(raw: string): Issue[] {
  return parseList<Issue>(raw, "issues").filter(
    (i) => typeof i === "object" && i !== null && typeof i.example === "string" && isNum(i.count),
  );
}

/** Runs `cal` with the resolved binary (cal.ts runCal in the editor, calCore runCalAt on a
 *  fake binary in the tests). */
export type CalRunner = (args: string[], opts: RunCalOptions) => Promise<string>;

/** How a load is run. Aborting `signal` kills the `cal` process (calCore's execCal): the view
 *  aborts on Refresh and on dispose, so a superseded `cal issues`, which mines for seconds,
 *  does not keep running next to the one that replaced it. */
export interface LoadOptions {
  signal?: AbortSignal;
}

/** The project memory of `folder` (the process's cwd, never an argument). */
export async function loadMemory(
  run: CalRunner,
  folder: string,
  { signal, perKind = MEMORY_PER_KIND }: LoadOptions & { perKind?: number } = {},
): Promise<ProjectMemory> {
  const args = memoryArgs(perKind);
  assertReadOnlyCall(args);
  return parseMemory(await run(args, { cwd: folder, timeoutMs: MEMORY_TIMEOUT_MS, signal }));
}

/** The recurring errors of `project` (the memory answer's), or null when it is empty or
 *  holds every project (`home` as for holdsEveryProject). */
export async function loadIssues(
  run: CalRunner,
  folder: string,
  project: string,
  { signal, home }: LoadOptions & { home?: string } = {},
): Promise<Issue[] | null> {
  const args = issuesArgs(project, home);
  if (!args) return null;
  assertReadOnlyCall(args);
  return parseIssues(await run(args, { cwd: folder, timeoutMs: ISSUES_TIMEOUT_MS, signal }));
}

/** What the view shows instead of a tree: each value but `ready`, `loading` and `error`
 *  has a `viewsWelcome` entry in package.json (`callimachus.memoryState == <value>`). */
export type ViewState =
  | "loading"
  | "ready"
  | "error"
  | "noFolder"
  | "missing"
  | "noIndex"
  | "busy"
  | "schemaBehind"
  | "restoreInterrupted"
  | "empty";

/** The states package.json must give a welcome text (checked by the tests). */
export const WELCOME_STATES: readonly ViewState[] = [
  "noFolder",
  "missing",
  "noIndex",
  "busy",
  "schemaBehind",
  "restoreInterrupted",
  "empty",
];

/** The state a failed `cal` call puts the view in, and for `error` the message to show. */
export function stateOfError(err: unknown): { state: ViewState; message?: string } {
  if (err instanceof CalSetupError) {
    return { state: err.reason === "missing" ? "missing" : "noIndex" };
  }
  if (err instanceof CalStateError) {
    switch (err.problem) {
      case "busy":
        return { state: "busy" };
      case "schema-behind":
        return { state: "schemaBehind" };
      case "restore-interrupted":
        return { state: "restoreInterrupted" };
    }
  }
  if (err instanceof CalVersionError) return { state: "error", message: err.message };
  return { state: "error", message: oneLine((err as Error)?.message ?? String(err), 300) };
}

/** The state of a memory answer: `empty` when it has no fact of any kind. */
export function stateOfMemory(mem: ProjectMemory): ViewState {
  return mem.decisions.length + mem.gotchas.length + mem.openTodos.length === 0 ? "empty" : "ready";
}

/** One line of a row under the Recurring errors group when it has no errors to list. */
export function issuesProblemText(err: unknown): string {
  const { state, message } = stateOfError(err);
  switch (state) {
    case "busy":
      return "The index is busy. Refresh to try again.";
    case "schemaBehind":
      return "The index is older than this cal. Open the Callimachus app once to upgrade it.";
    case "restoreInterrupted":
      return "A restore of the index was interrupted. Open the Callimachus app to finish it.";
    case "missing":
    case "noIndex":
      return "cal could not read the index. Refresh to try again.";
    default:
      return `Could not load recurring errors: ${message ?? "unknown error"}`;
  }
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

export type FactGroup = "decisions" | "gotchas" | "openTodos";

export type MemoryNode =
  | { type: "group"; group: FactGroup; facts: MemoryFact[] }
  | { type: "empty" }
  | { type: "errors"; project: string }
  | { type: "fact"; fact: MemoryFact }
  | { type: "issue"; issue: Issue; index: number }
  | { type: "fix"; fix: IssueFix; index: number }
  | { type: "playbook"; text: string; index: number }
  | { type: "info"; text: string };

/** A row as plain data; memoryView.ts maps it onto a vscode.TreeItem one to one. */
export interface RowView {
  id: string;
  label: string;
  description?: string;
  tooltip?: string;
  /** A codicon id, present in VS Code 1.130.0's and Cursor 3.9.16's bundles. */
  icon: string;
  collapsible: "none" | "collapsed" | "expanded";
  /** Only ever `callimachus.openThread` (thread id, session title): the view runs nothing
   *  else, and nothing that writes. */
  command?: {
    command: "callimachus.openThread";
    title: string;
    arguments: [number, string | null];
  };
}

const GROUPS: { group: FactGroup; label: string; icon: string; open: boolean }[] = [
  { group: "decisions", label: "Decisions", icon: "law", open: true },
  { group: "gotchas", label: "Gotchas", icon: "lightbulb", open: false },
  { group: "openTodos", label: "Open TODOs", icon: "tasklist", open: false },
];

/** What the view says above Recurring errors when the project has no fact yet. */
export const EMPTY_MEMORY_TEXT =
  "Nothing remembered yet: decisions, gotchas and open TODOs appear once the Callimachus app has distilled this project's sessions.";

/** Said in place of Recurring errors for a project that holds every project. */
export const EVERY_PROJECT_TEXT =
  "No recurring errors for your home folder, a folder above it or a filesystem root: every project is inside it. Open a project's folder to see its errors.";

/** The top rows: the three fact groups, then Recurring errors unless the answer names no
 *  project (an empty `project` would make `cal issues` list every project's errors). An
 *  empty memory shows one notice row instead of the groups, but keeps Recurring errors:
 *  `cal issues` mines raw sessions, so a project never distilled can still have them. With
 *  neither, there are no rows, and the view's `empty` welcome text shows. A project that
 *  holds every project (holdsEveryProject, with `home`) gets a row saying why instead. */
export function rootNodes(mem: ProjectMemory, home?: string): MemoryNode[] {
  const errors: MemoryNode[] = holdsEveryProject(mem.project, home)
    ? [{ type: "info", text: EVERY_PROJECT_TEXT }]
    : issuesArgs(mem.project, home)
      ? [{ type: "errors", project: mem.project.trim() }]
      : [];
  if (stateOfMemory(mem) === "empty") return errors.length ? [{ type: "empty" }, ...errors] : [];
  const groups: MemoryNode[] = GROUPS.map(({ group }) => ({
    type: "group",
    group,
    facts: mem[group],
  }));
  return [...groups, ...errors];
}

export function childNodes(node: MemoryNode): MemoryNode[] {
  switch (node.type) {
    case "group":
      return node.facts.map((fact) => ({ type: "fact", fact }));
    case "issue": {
      const out: MemoryNode[] = [];
      if (node.issue.fix) out.push({ type: "fix", fix: node.issue.fix, index: node.index });
      if (node.issue.playbook?.trim()) {
        out.push({ type: "playbook", text: node.issue.playbook, index: node.index });
      }
      return out;
    }
    default:
      return [];
  }
}

/** The rows of a loaded Recurring errors group. */
export function issueNodes(issues: Issue[]): MemoryNode[] {
  if (issues.length === 0) {
    return [{ type: "info", text: "No recurring errors in this project in the last 180 days." }];
  }
  return issues.map((issue, index) => ({ type: "issue", issue, index }));
}

/** Collapse whitespace to one line and cap it, for a label. */
export function oneLine(text: string, max = 160): string {
  const s = text.replace(/\s+/g, " ").trim();
  return s.length > max ? `${s.slice(0, max - 1).trimEnd()}…` : s;
}

/** Epoch seconds as the local YYYY-MM-DD. */
export function formatDate(epochSeconds: number | undefined): string | undefined {
  if (!isNum(epochSeconds) || epochSeconds <= 0) return undefined;
  const d = new Date(epochSeconds * 1000);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

/** Where a fact came from. `in_app` is the project's "Recorded memory" thread, which holds
 *  facts you or an agent recorded and facts imported from the repo (memory.v1.json). */
export function factSourceLabel(fact: MemoryFact): string | undefined {
  if (!fact.source) return undefined;
  if (fact.source === "in_app") return fact.extractor === "repo" ? "from the repo" : "recorded";
  return sourceLabel(fact.source);
}

/** The words for a fact that is not current (the same words `cal why` prints). */
export function factStatusText(fact: MemoryFact): string | undefined {
  switch (fact.status) {
    case "flagged":
      return "flagged stale or wrong";
    case "disputed":
      return "disputed";
    case "superseded":
      return "superseded";
    default:
      return undefined;
  }
}

/** Flagged and disputed say more than pinned, so they take the icon; the tooltip names both. */
function factIcon(fact: MemoryFact): string {
  switch (fact.status) {
    case "flagged":
      return "warning";
    case "disputed":
      return "question";
    case "superseded":
      return "circle-outline";
    default:
      return fact.pinned ? "pinned" : "circle-small-filled";
  }
}

const openThread = (
  threadId: number | undefined,
  title: string | null,
): RowView["command"] | undefined =>
  isNum(threadId) && Number.isInteger(threadId) && threadId > 0
    ? { command: "callimachus.openThread", title: "Open session", arguments: [threadId, title] }
    : undefined;

const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;

export function rowView(node: MemoryNode, issuesLoaded?: number): RowView {
  switch (node.type) {
    case "group": {
      const g = GROUPS.find((x) => x.group === node.group) ?? GROUPS[0];
      // `cal memory -n` caps each kind (knowledge::project_facts), so a full list is not the
      // project's count, and memory.v1.json carries no per-kind total to show instead.
      const full = node.facts.length >= MEMORY_PER_KIND;
      return {
        id: `group:${node.group}`,
        label: g.label,
        description: full ? `${node.facts.length}+` : String(node.facts.length),
        tooltip: full
          ? `Showing the first ${MEMORY_PER_KIND} (pinned ones, then the newest); there may be more. See the project's page in the Callimachus app.`
          : undefined,
        icon: g.icon,
        collapsible: node.facts.length === 0 ? "none" : g.open ? "expanded" : "collapsed",
      };
    }
    case "empty":
      return {
        id: "memory:empty",
        label: EMPTY_MEMORY_TEXT,
        tooltip: EMPTY_MEMORY_TEXT,
        icon: "info",
        collapsible: "none",
      };
    case "errors":
      // `cal issues <project>` matches `project` as a substring of each session's project
      // (cmd_issues: ProjectMatch::Substring), unlike `cal memory`'s exact key, and its JSON
      // names no project per error, so the view cannot narrow the list: it says so instead.
      return {
        id: "group:errors",
        label: "Recurring errors",
        description: issuesLoaded === undefined ? undefined : String(issuesLoaded),
        tooltip: `Errors sessions hit again and again (last 180 days), from cal issues, loaded when you expand this. cal issues matches projects by path, so this also lists the errors of every project whose path contains ${node.project} (such as ${node.project}-web, or a repository inside this folder).`,
        icon: "bug",
        collapsible: "collapsed",
      };
    case "fact": {
      const f = node.fact;
      const date = formatDate(f.decidedAt ?? f.createdAt);
      const source = factSourceLabel(f);
      const status = factStatusText(f);
      const lines = [f.text.trim(), ""];
      const meta = [date ? `Decided ${date}` : undefined, source].filter(Boolean).join(", ");
      if (meta) lines.push(meta);
      if (status) lines.push(`Status: ${status}`);
      if (f.pinned) lines.push("Pinned");
      const other = f.disputedBy?.[0];
      if (other?.text) lines.push(`Disputed by: ${oneLine(other.text, 200)}`);
      if (isNum(f.support) && f.support > 1) {
        lines.push(`Reaffirmed by ${plural(f.support - 1, "other session")}`);
      }
      if (f.title?.trim()) lines.push(`Session: ${oneLine(f.title, 120)}`);
      return {
        id: `fact:${f.id}`,
        label: oneLine(f.text),
        description: [date, source].filter(Boolean).join(" · ") || undefined,
        tooltip: lines.join("\n").trimEnd(),
        icon: factIcon(f),
        collapsible: "none",
        command: openThread(f.threadId, f.title),
      };
    }
    case "issue": {
      const i = node.issue;
      const last = formatDate(i.lastSeen);
      const first = formatDate(i.firstSeen);
      const lines = [
        i.example.trim(),
        "",
        `Seen ${plural(i.count, "time")} in ${plural(i.threads, "session")}${
          first && last ? `, ${first} to ${last}` : ""
        }`,
      ];
      if (i.threadIds?.length) lines.push("Click to open the latest session that hit it.");
      return {
        id: `issue:${node.index}`,
        label: oneLine(i.example),
        description: `${i.count}×${last ? ` · last ${last}` : ""}`,
        tooltip: lines.join("\n"),
        icon: "error",
        collapsible: i.fix || i.playbook?.trim() ? "collapsed" : "none",
        command: openThread(i.threadIds?.[0], null),
      };
    }
    case "fix": {
      const x = node.fix;
      const subject = x.subject?.trim();
      return {
        id: `issue:${node.index}:fix`,
        label: subject ? `${x.shortSha} ${oneLine(subject)}` : x.shortSha,
        description: formatDate(x.committedAt),
        tooltip:
          "The linked fix: the latest session here that hit this error, then edited the file it named and committed a change to that file. Click to open that session.",
        icon: "git-commit",
        collapsible: "none",
        command: openThread(x.threadId, null),
      };
    }
    case "playbook":
      return {
        id: `issue:${node.index}:playbook`,
        label: oneLine(node.text),
        description: "playbook",
        tooltip: node.text.trim(),
        icon: "book",
        collapsible: "none",
      };
    case "info":
      return {
        id: "errors:info",
        label: node.text,
        tooltip: node.text,
        icon: "info",
        collapsible: "none",
      };
  }
}
