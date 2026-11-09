// Provenance in the editor: a gutter mark on each line whose commit a session produced (or
// an Agent Trace record attributes), and a hover that says which session, with its
// decisions. Both come from one `cal why <file>` per document version (the whole-file form
// returns each commit's line ranges), so neither runs git itself.
//
// Each run is a `cal` process that blames the file, so the editor stays responsive by
// running at most one per document and MAX_CONCURRENT_RUNS in all, debounced after edits,
// cancelled when a newer version supersedes it or its editor is closed or hidden, and killed
// (with the git it runs) after a hard timeout. An unsaved UTF-8 buffer goes to
// `cal why --stdin` so the line numbers match what the user sees.

import { open } from "node:fs/promises";
import * as path from "node:path";
import * as vscode from "vscode";
import { CalSetupError, calHasWhy, config, whyFile } from "./cal";
import { sourceLabel, type WhyCommit, type WhyResult, type WhyThread } from "./protocol";

/** Settle time after an edit before re-asking. */
const DEBOUNCE_MS = 500;
/** A run past this is killed: a blame that slow would hold a process per keystroke burst. */
const RUN_TIMEOUT_MS = 5_000;
/** Files longer than this are skipped: blame cost grows with the file, and so does the
 *  JSON the hover would never need most of. */
const MAX_LINES = 20_000;
/** A commit, checkout or `cal commits` changes the answer without changing the document:
 *  an answer older than this is refreshed when its editor is shown or the window regains
 *  focus. */
const STALE_MS = 60_000;
/** After `cal` turned out missing or without an index, how long before trying again. */
const SETUP_RETRY_MS = 5 * 60_000;
/** Facts a hover shows. */
const HOVER_FACTS = 3;
/** `cal why` processes at once across all documents. Cycling through tabs, or regaining
 *  focus with many split editors, would otherwise start one blame (and one open of the
 *  index) per editor at the same moment. */
const MAX_CONCURRENT_RUNS = 2;

interface Entry {
  version: number;
  at: number;
  result: WhyResult | null;
}

interface Run {
  version: number;
  controller: AbortController;
  done: Promise<void>;
}

interface Waiter {
  grant: () => void;
  signal: AbortSignal;
}

export class WhyGutter implements vscode.Disposable {
  private readonly decoration: vscode.TextEditorDecorationType;
  private readonly cache = new Map<string, Entry>();
  private readonly runs = new Map<string, Run>();
  private readonly timers = new Map<string, ReturnType<typeof setTimeout>>();
  /** Folders `cal` said are outside any git repository. */
  private readonly notRepo = new Set<string>();
  private readonly subs: vscode.Disposable[] = [];
  /** `cal` lacks `why`, or `--stdin` (older releases): skip, or blame saved files only. */
  private noWhy = false;
  private noStdin = false;
  private setupFailedAt = 0;
  private active = 0;
  /** Runs waiting for a slot, the newest last: it is served first, since the editor the
   *  user just turned to matters more than one they moved past. */
  private readonly waiting: Waiter[] = [];

  constructor(extensionUri: vscode.Uri) {
    const icon = (name: string) => vscode.Uri.joinPath(extensionUri, "media", name);
    this.decoration = vscode.window.createTextEditorDecorationType({
      light: { gutterIconPath: icon("why-gutter-light.svg") },
      dark: { gutterIconPath: icon("why-gutter-dark.svg") },
      gutterIconSize: "contain",
      isWholeLine: true,
    });
    this.subs.push(
      this.decoration,
      vscode.languages.registerHoverProvider({ scheme: "file" }, this),
      vscode.window.onDidChangeVisibleTextEditors(() => {
        this.cancelHidden();
        this.refreshVisible();
      }),
      vscode.workspace.onDidChangeTextDocument((e) => {
        if (e.contentChanges.length > 0) this.schedule(e.document, DEBOUNCE_MS);
      }),
      vscode.workspace.onDidCloseTextDocument((doc) => this.forget(doc)),
      vscode.window.onDidChangeWindowState((s) => {
        if (s.focused) this.refreshVisible();
      }),
      vscode.workspace.onDidChangeConfiguration((e) => {
        if (
          e.affectsConfiguration("callimachus.whyGutter") ||
          e.affectsConfiguration("callimachus.whyHover") ||
          e.affectsConfiguration("callimachus.calPath")
        ) {
          this.noWhy = false;
          this.noStdin = false;
          this.setupFailedAt = 0;
          this.refreshVisible();
        }
      }),
    );
    this.refreshVisible();
  }

  private gutterOn(): boolean {
    return config<boolean>("whyGutter", true);
  }

  private hoverOn(): boolean {
    return config<boolean>("whyHover", true);
  }

  /** Whether `doc` gets a `cal why` at all. */
  private eligible(doc: vscode.TextDocument): boolean {
    if (!this.gutterOn() && !this.hoverOn()) return false;
    if (this.noWhy || Date.now() - this.setupFailedAt < SETUP_RETRY_MS) return false;
    if (doc.uri.scheme !== "file" || doc.isUntitled || doc.lineCount > MAX_LINES) return false;
    return !this.notRepo.has(path.dirname(doc.uri.fsPath));
  }

  /** Render every visible editor, asking for documents with no fresh answer. */
  private refreshVisible(): void {
    for (const editor of vscode.window.visibleTextEditors) {
      const doc = editor.document;
      const entry = this.cache.get(key(doc));
      const fresh = entry?.version === doc.version && Date.now() - entry.at < STALE_MS;
      if (!fresh) this.schedule(doc, 0);
      this.render(editor);
    }
  }

  private schedule(doc: vscode.TextDocument, delay: number): void {
    const k = key(doc);
    const pending = this.timers.get(k);
    if (pending) clearTimeout(pending);
    this.timers.delete(k);
    if (!this.eligible(doc)) {
      this.renderDoc(doc);
      return;
    }
    // Only documents on screen: a background tab is asked when it is shown.
    if (!vscode.window.visibleTextEditors.some((e) => e.document === doc)) return;
    this.timers.set(
      k,
      setTimeout(() => {
        this.timers.delete(k);
        // A hover may have asked for this version while the timer waited.
        const entry = this.cache.get(k);
        if (entry?.version === doc.version && Date.now() - entry.at < STALE_MS) {
          this.renderDoc(doc);
          return;
        }
        void this.run(doc);
      }, delay),
    );
  }

  /** One `cal why` for the document's current version, replacing any older run. */
  private run(doc: vscode.TextDocument): Promise<void> {
    const k = key(doc);
    const version = doc.version;
    const current = this.runs.get(k);
    if (current?.version === version) return current.done;
    current?.controller.abort();
    const controller = new AbortController();
    const done = this.ask(doc, version, controller).finally(() => {
      if (this.runs.get(k)?.controller === controller) this.runs.delete(k);
    });
    this.runs.set(k, { version, controller, done });
    return done;
  }

  /** Stop the runs and pending asks of documents no longer on screen; they are asked again
   *  when shown (refreshVisible finds no fresh answer for them). */
  private cancelHidden(): void {
    const shown = new Set(vscode.window.visibleTextEditors.map((e) => key(e.document)));
    for (const [k, run] of this.runs) {
      if (shown.has(k)) continue;
      run.controller.abort();
      this.runs.delete(k);
    }
    for (const [k, timer] of this.timers) {
      if (shown.has(k)) continue;
      clearTimeout(timer);
      this.timers.delete(k);
    }
  }

  /** Wait for one of the MAX_CONCURRENT_RUNS slots; false when aborted while waiting. */
  private acquire(signal: AbortSignal): Promise<boolean> {
    if (signal.aborted) return Promise.resolve(false);
    if (this.active < MAX_CONCURRENT_RUNS) {
      this.active++;
      return Promise.resolve(true);
    }
    return new Promise((resolve) => {
      const waiter: Waiter = { grant: () => resolve(true), signal };
      this.waiting.push(waiter);
      signal.addEventListener(
        "abort",
        () => {
          const i = this.waiting.indexOf(waiter);
          if (i >= 0) this.waiting.splice(i, 1);
          resolve(false);
        },
        { once: true },
      );
    });
  }

  private release(): void {
    this.active--;
    while (this.waiting.length > 0) {
      const next = this.waiting.pop();
      if (!next || next.signal.aborted) continue;
      this.active++;
      next.grant();
      return;
    }
  }

  private async ask(
    doc: vscode.TextDocument,
    version: number,
    controller: AbortController,
  ): Promise<void> {
    const k = key(doc);
    let result: WhyResult | null = null;
    try {
      if (!(await calHasWhy())) {
        this.noWhy = true;
        this.renderDoc(doc);
        return;
      }
      let input: string | undefined;
      if (doc.isDirty) {
        // No buffer `cal` can compare byte for byte: the saved file's answer stays (its
        // marks move with the edits) until the file is saved.
        if (this.noStdin) return;
        const bytes = await bufferInput(doc);
        if (bytes === null) return;
        input = bytes;
      }
      if (!(await this.acquire(controller.signal))) return;
      try {
        if (controller.signal.aborted) return;
        result = await whyFile(doc.uri.fsPath, {
          input,
          signal: controller.signal,
          timeoutMs: RUN_TIMEOUT_MS,
        });
      } finally {
        this.release();
      }
    } catch (err) {
      if (controller.signal.aborted) return;
      const message = (err as Error).message ?? "";
      if (err instanceof CalSetupError) {
        // The sidebar shows the setup prompt; the gutter stays quiet and retries later.
        this.setupFailedAt = Date.now();
      } else if (/not in a git repository/i.test(message)) {
        this.notRepo.add(path.dirname(doc.uri.fsPath));
      } else if (/unknown flag '--stdin'/.test(message)) {
        this.noStdin = true;
        return;
      } else if (/unknown command 'why'/.test(message)) {
        this.noWhy = true;
      }
      // Anything else (a timeout, a file git does not track): no marks for this version.
    }
    if (controller.signal.aborted) return;
    this.cache.set(k, { version, at: Date.now(), result });
    this.renderDoc(doc);
  }

  private renderDoc(doc: vscode.TextDocument): void {
    for (const editor of vscode.window.visibleTextEditors) {
      if (editor.document === doc) this.render(editor);
    }
  }

  private render(editor: vscode.TextEditor): void {
    const doc = editor.document;
    const entry = this.cache.get(key(doc));
    // A stale answer is left in place while a newer one is asked for: VS Code moves
    // decorations with the edits, which is closer than clearing them on every keystroke.
    if (!this.gutterOn() || !this.eligible(doc) || !entry?.result) {
      editor.setDecorations(this.decoration, []);
      return;
    }
    if (entry.version !== doc.version) return;
    const ranges: vscode.Range[] = [];
    for (const c of entry.result.commits) {
      if (!explained(c)) continue;
      for (const [start, end] of c.ranges ?? []) {
        const last = Math.min(end, doc.lineCount);
        if (start > last) continue;
        ranges.push(new vscode.Range(start - 1, 0, last - 1, 0));
      }
    }
    editor.setDecorations(this.decoration, ranges);
  }

  async provideHover(
    doc: vscode.TextDocument,
    pos: vscode.Position,
    token: vscode.CancellationToken,
  ): Promise<vscode.Hover | undefined> {
    if (!this.hoverOn() || !this.eligible(doc)) return undefined;
    let entry = this.cache.get(key(doc));
    if (entry?.version !== doc.version) {
      // Asked before the debounce fired: wait for this version's answer (bounded by the
      // run's own timeout) rather than describe a line that moved.
      await this.run(doc);
      if (token.isCancellationRequested) return undefined;
      entry = this.cache.get(key(doc));
      if (entry?.version !== doc.version) return undefined;
    }
    const line = pos.line + 1;
    const commit = entry.result?.commits.find(
      (c) => explained(c) && (c.ranges ?? []).some(([a, b]) => a <= line && line <= b),
    );
    if (!commit) return undefined;
    return new vscode.Hover(hoverText(commit, doc.uri.fsPath, line), doc.lineAt(pos.line).range);
  }

  private forget(doc: vscode.TextDocument): void {
    const k = key(doc);
    this.runs.get(k)?.controller.abort();
    this.runs.delete(k);
    const pending = this.timers.get(k);
    if (pending) clearTimeout(pending);
    this.timers.delete(k);
    this.cache.delete(k);
  }

  dispose(): void {
    // Aborting also drops each run still waiting for a slot.
    for (const run of this.runs.values()) run.controller.abort();
    for (const t of this.timers.values()) clearTimeout(t);
    for (const s of this.subs) s.dispose();
  }
}

const key = (doc: vscode.TextDocument): string => doc.uri.toString();

/** An unsaved buffer as the bytes `git blame --contents -` compares with the committed
 *  file. `cal` gets the text as UTF-8, and getText() drops the byte order mark, so the
 *  buffer is sent only for a UTF-8 file, with the mark put back when the file has one;
 *  otherwise every line would differ from the blob and show as not committed yet. The
 *  encoding comes from `TextDocument.encoding` (VS Code 1.100 and later) or, before it, from
 *  the saved file's byte order mark: a UTF-16 file is skipped, and a file in a legacy
 *  8-bit encoding cannot be told from UTF-8 there, so its non-ASCII lines lose their marks
 *  while it is unsaved. Null when the buffer cannot be sent. */
async function bufferInput(doc: vscode.TextDocument): Promise<string | null> {
  const encoding = (doc as { encoding?: string }).encoding;
  let bom = encoding === "utf8bom";
  if (encoding !== undefined) {
    if (encoding !== "utf8" && encoding !== "utf8bom") return null;
  } else {
    const head = await fileHead(doc.uri.fsPath);
    if (
      head.length >= 2 &&
      ((head[0] === 0xff && head[1] === 0xfe) || (head[0] === 0xfe && head[1] === 0xff))
    ) {
      return null;
    }
    bom = head.length >= 3 && head[0] === 0xef && head[1] === 0xbb && head[2] === 0xbf;
  }
  const text = doc.getText();
  return bom ? `\uFEFF${text}` : text;
}

/** The first bytes of the saved file; empty when it cannot be read. */
async function fileHead(file: string): Promise<Uint8Array> {
  try {
    const handle = await open(file, "r");
    try {
      const buf = new Uint8Array(3);
      const { bytesRead } = await handle.read(buf, 0, 3, 0);
      return buf.subarray(0, bytesRead);
    } finally {
      await handle.close();
    }
  } catch {
    return new Uint8Array(0);
  }
}

/** A commit worth a mark: a session produced it, or a trace record attributes it. */
function explained(c: WhyCommit): boolean {
  return c.threads.length > 0 || (c.trace?.length ?? 0) > 0;
}

function formatDate(epochSeconds: number | null): string {
  if (!epochSeconds) return "";
  return new Date(epochSeconds * 1000).toLocaleDateString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
  });
}

/** What a session did to the file, as the sidebar says it. */
function touchText(t: WhyThread): string {
  switch (t.touch) {
    case "write":
    case "edit":
      return t.edits === 1 ? "edited the file once" : `edited the file ${t.edits} times`;
    case "read":
      return "read the file";
    case "mention":
      return "mentioned the file";
    default:
      return "";
  }
}

const commandLink = (label: string, command: string, args: unknown[]): string =>
  `[${label}](command:${command}?${encodeURIComponent(JSON.stringify(args))})`;

/** The hover: the commit, its best session (title, agent, date, overlap) with up to three
 *  decisions or gotchas, then any trace attributions, and the two actions. Text from the
 *  index goes through appendText, which escapes markdown. */
function hoverText(c: WhyCommit, file: string, line: number): vscode.MarkdownString {
  const md = new vscode.MarkdownString(undefined, true);
  md.isTrusted = { enabledCommands: ["callimachus.openThread", "callimachus.whyLineAt"] };
  md.appendMarkdown("$(history) **Callimachus** · `");
  md.appendText(c.shortSha.slice(0, 8));
  md.appendMarkdown("` ");
  md.appendText(c.subject);
  md.appendMarkdown("\n\n");
  const t = c.threads[0];
  if (t) {
    md.appendMarkdown("**");
    md.appendText(t.title?.trim() || "(untitled session)");
    md.appendMarkdown("**  \n");
    const overlap = `shares ${t.overlap} ${t.overlap === 1 ? "file" : "files"} with the commit`;
    md.appendText(
      [sourceLabel(t.source), formatDate(t.lastActive), touchText(t), overlap]
        .filter(Boolean)
        .join(" · "),
    );
    md.appendMarkdown("\n\n");
    for (const f of t.facts.slice(0, HOVER_FACTS)) {
      md.appendMarkdown(`- *${f.kind}*: `);
      md.appendText(f.text);
      if (f.rationale) {
        md.appendText(` (because ${f.rationale})`);
      }
      md.appendMarkdown("\n");
    }
    if (t.facts.length > 0) md.appendMarkdown("\n");
    const more = c.threads.length - 1;
    if (more > 0) {
      md.appendText(`+${more} more ${more === 1 ? "session" : "sessions"} linked to this commit`);
      md.appendMarkdown("\n\n");
    }
  }
  for (const a of (c.trace ?? []).slice(0, 2)) {
    md.appendMarkdown("$(git-commit) Agent Trace: ");
    const who = [a.agent, a.model ? `(${a.model})` : null].filter(Boolean).join(" ");
    md.appendText(who || a.contributor);
    if (a.conversationId) md.appendText(`, conversation ${a.conversationId}`);
    if (a.threadId !== null) {
      md.appendMarkdown(
        ` · ${commandLink("Open session", "callimachus.openThread", [a.threadId])}`,
      );
    }
    md.appendMarkdown("\n\n");
  }
  const actions: string[] = [];
  if (t) actions.push(commandLink("Open session", "callimachus.openThread", [t.id]));
  actions.push(commandLink("Why this line", "callimachus.whyLineAt", [file, line]));
  md.appendMarkdown(actions.join(" · "));
  return md;
}
