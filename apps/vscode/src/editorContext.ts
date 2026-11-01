// Watches the active editor + cursor and derives a small "context" string to
// recall related threads against — the layered signal: the selection, else the
// symbol around the cursor, else the nearest error. Debounced; never sends whole
// files. Pure VS Code APIs; the actual `cal related` call happens in the provider.
//
// Each new context costs a `cal related` process that loads the embedding model, and
// deriving it costs an LSP symbol request, so the watcher does nothing at all while
// ambient recall can't be shown (sidebar hidden, or the feature off).

import * as vscode from "vscode";
import { config } from "./cal";

/** Settle time before recalling. Must match the `callimachus.ambientRecallThrottle`
 *  default in package.json, which is what the Settings UI shows; this is only the
 *  fallback if the contribution is missing. 1.5 s because each recall spawns a process
 *  that loads the embedding model. */
const DEFAULT_THROTTLE_MS = 1500;
const MIN_THROTTLE_MS = 150;

function throttleMs(): number {
  return Math.max(MIN_THROTTLE_MS, config<number>("ambientRecallThrottle", DEFAULT_THROTTLE_MS));
}

export interface EditorContext {
  /** Text to embed/recall against (already capped). */
  text: string;
  /** Short hint of what matched, for the sidebar section header. */
  label: string;
}

const MAX_CONTEXT = 1500; // bge-small caps ~512 tokens; keep embeds cheap

export class EditorContextWatcher implements vscode.Disposable {
  private timer: ReturnType<typeof setTimeout> | undefined;
  private lastText = "";
  private readonly subs: vscode.Disposable[] = [];

  constructor(
    private readonly onContext: (ctx: EditorContext | null) => void,
    /** Whether a result could be shown right now (sidebar visible + feature on). */
    private readonly isActive: () => boolean,
  ) {
    this.subs.push(
      vscode.window.onDidChangeActiveTextEditor(() => this.schedule()),
      vscode.window.onDidChangeTextEditorSelection(() => this.schedule()),
    );
    this.schedule();
  }

  /** Force a re-evaluation (e.g. when the sidebar becomes visible). */
  refresh(): void {
    this.lastText = "";
    this.schedule();
  }

  private schedule(): void {
    if (this.timer) clearTimeout(this.timer);
    this.timer = undefined;
    // Hidden or disabled: skip even the symbol lookup. refresh() re-arms on show.
    if (!this.isActive()) return;
    this.timer = setTimeout(() => void this.fire(), throttleMs());
  }

  private async fire(): Promise<void> {
    this.timer = undefined;
    if (!this.isActive()) return;
    const ctx = await this.extract();
    // Dedupe: identical context shouldn't re-query.
    if (ctx && ctx.text === this.lastText) return;
    this.lastText = ctx?.text ?? "";
    this.onContext(ctx);
  }

  private async extract(): Promise<EditorContext | null> {
    const editor = vscode.window.activeTextEditor;
    if (!editor) return null;
    const doc = editor.document;
    const minLen = config<number>("ambientRecallMinContext", 10);
    const line = editor.selection.active.line;

    // 1. Selection — the most deliberate signal.
    const sel = doc.getText(editor.selection).trim();
    if (sel.length >= minLen) {
      return { text: sel.slice(0, MAX_CONTEXT), label: "selection" };
    }

    // 2. Symbol around the cursor (LSP-backed; best-effort).
    try {
      const symbols = await vscode.commands.executeCommand<vscode.DocumentSymbol[]>(
        "vscode.executeDocumentSymbolProvider",
        doc.uri,
      );
      const sym = pickSymbol(symbols ?? [], line);
      if (sym && sym.name.trim().length >= 2) {
        const body = doc.getText(sym.range).slice(0, MAX_CONTEXT).trim();
        return { text: body || sym.name, label: sym.name };
      }
    } catch {
      // No symbol provider for this language — fall through.
    }

    // 3. Nearest error diagnostic.
    const errs = vscode.languages
      .getDiagnostics(doc.uri)
      .filter((d) => d.severity === vscode.DiagnosticSeverity.Error);
    if (errs.length > 0) {
      const nearest = errs.sort(
        (a, b) => Math.abs(a.range.start.line - line) - Math.abs(b.range.start.line - line),
      )[0];
      const msg = nearest.message.trim();
      if (msg.length >= minLen) {
        return { text: msg.slice(0, MAX_CONTEXT), label: "error" };
      }
    }

    return null;
  }

  dispose(): void {
    if (this.timer) clearTimeout(this.timer);
    for (const s of this.subs) s.dispose();
  }
}

/** Deepest symbol whose range contains `line`. */
function pickSymbol(symbols: vscode.DocumentSymbol[], line: number): vscode.DocumentSymbol | null {
  for (const s of symbols) {
    if (s.range.start.line <= line && line <= s.range.end.line) {
      return pickSymbol(s.children ?? [], line) ?? s;
    }
  }
  return null;
}
