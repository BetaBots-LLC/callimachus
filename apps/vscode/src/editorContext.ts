// Watches the active editor + cursor and derives a small "context" string to
// recall related threads against — the layered signal: the selection, else the
// symbol around the cursor, else the nearest error. Debounced; never sends whole
// files. Pure VS Code APIs; the actual `cal related` call happens in the provider.

import * as vscode from "vscode";
import { config } from "./cal";

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

  constructor(private readonly onContext: (ctx: EditorContext | null) => void) {
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
    const ms = Math.max(150, config<number>("ambientRecallThrottle", 500));
    this.timer = setTimeout(() => void this.fire(), ms);
  }

  private async fire(): Promise<void> {
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
