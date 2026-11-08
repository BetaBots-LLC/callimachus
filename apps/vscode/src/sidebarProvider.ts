// The Activity Bar sidebar webview. Registered with retainContextWhenHidden so
// the search state survives the view being hidden; the webview also persists its
// query via getState/setState as a belt-and-suspenders.
//
// Also drives "ambient recall": an EditorContextWatcher derives what you're
// looking at, and — only while this view is visible and the feature is enabled —
// the provider queries `cal related` and pushes the matches to the webview.

import * as vscode from "vscode";
import { getHtml } from "./webviewHtml";
import { handleWebviewMessage } from "./webviewMessages";
import { openThreadPanel } from "./threadPanel";
import { clearRelatedCache, config, relatedThreads, showCalSetupPrompt, whyLine } from "./cal";
import { type EditorContext, EditorContextWatcher } from "./editorContext";
import type { FromWebview, InitPayload, ToWebview, WhyState } from "./protocol";

export class SidebarProvider implements vscode.WebviewViewProvider, vscode.Disposable {
  public static readonly viewId = "callimachus.sidebar";
  private view?: vscode.WebviewView;
  private readonly watcher: EditorContextWatcher;
  // Single-flight, latest-wins: at most one `cal related` process at a time. Contexts that
  // arrive meanwhile collapse into `pending` (only the newest survives), and a result
  // that a newer context has superseded is dropped instead of flashing in.
  private inFlight = false;
  private pending: { ctx: EditorContext | null } | undefined;
  // The webview has sent `ready` (it listens for pushes). Until then the latest "Why this
  // line" state waits here: the command focuses the view, which may not exist yet.
  private webviewReady = false;
  private whyState: WhyState | null = null;
  // Each "Why this line" run; a slower earlier run must not overwrite a newer answer.
  private whySeq = 0;

  constructor(private readonly extensionUri: vscode.Uri) {
    this.watcher = new EditorContextWatcher(
      (ctx) => void this.onContext(ctx),
      () => this.recallActive(),
    );
  }

  /** Ambient recall can be shown: the view is visible and the feature is enabled. */
  private recallActive(): boolean {
    return !!this.view?.visible && config<boolean>("ambientRecall", true);
  }

  resolveWebviewView(view: vscode.WebviewView): void {
    this.view = view;
    view.webview.options = {
      enableScripts: true,
      localResourceRoots: [vscode.Uri.joinPath(this.extensionUri, "media")],
    };
    this.webviewReady = false;
    view.webview.html = getHtml(view.webview, this.extensionUri);

    // Re-evaluate related threads whenever the panel is shown again.
    view.onDidChangeVisibility(() => {
      if (view.visible) this.watcher.refresh();
    });

    view.webview.onDidReceiveMessage((msg: FromWebview) => {
      if (msg.kind === "ready") {
        const project = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? null;
        const init: InitPayload = { view: "sidebar", projectPath: project };
        view.webview.postMessage({ kind: "init", init } satisfies ToWebview);
        this.webviewReady = true;
        if (this.whyState) this.postWhy();
        // Now the webview is listening — kick off the first ambient query.
        this.watcher.refresh();
        return;
      }
      if (msg.kind === "closeWhy") {
        this.clearWhy();
        return;
      }
      handleWebviewMessage(view.webview, msg, {
        openThread: (id, title) => openThreadPanel(this.extensionUri, id, title),
      });
    });
  }

  /** Ambient recall: query `cal related` for the current editor context + push. */
  private async onContext(ctx: EditorContext | null): Promise<void> {
    if (this.inFlight) {
      this.pending = { ctx };
      return;
    }
    this.inFlight = true;
    try {
      let next: { ctx: EditorContext | null } | undefined = { ctx };
      while (next) {
        this.pending = undefined;
        await this.recall(next.ctx);
        next = this.pending;
      }
    } finally {
      this.inFlight = false;
    }
  }

  private async recall(ctx: EditorContext | null): Promise<void> {
    const view = this.view;
    if (!view || !this.recallActive()) return;
    if (!ctx) {
      view.webview.postMessage({ kind: "related", label: "", results: [] } satisfies ToWebview);
      return;
    }
    try {
      const results = await relatedThreads(ctx.text);
      // Superseded by a newer context, or the view was hidden during the await.
      if (this.pending || !this.recallActive()) return;
      this.view?.webview.postMessage({
        kind: "related",
        label: ctx.label,
        results,
      } satisfies ToWebview);
    } catch {
      // Background feature — stay quiet (e.g. cal missing, index not embedded).
    }
  }

  /** "Why this line": show the sidebar, then the commits and sessions behind `line` of
   *  `file` (1-based) as `cal why` reports them. */
  async why(file: string, line: number): Promise<void> {
    const seq = ++this.whySeq;
    this.setWhy({ status: "loading", file, line });
    await vscode.commands.executeCommand(`${SidebarProvider.viewId}.focus`);
    try {
      const result = await whyLine(file, line);
      if (seq === this.whySeq) this.setWhy({ status: "done", file, line, result });
    } catch (err) {
      showCalSetupPrompt(err); // download/setup toast for missing cal / no index
      if (seq === this.whySeq) {
        this.setWhy({ status: "error", file, line, message: (err as Error).message });
      }
    }
  }

  /** Close the "Why this line" section. */
  clearWhy(): void {
    this.whySeq++;
    this.setWhy(null);
  }

  private setWhy(state: WhyState | null): void {
    this.whyState = state;
    if (this.webviewReady) this.postWhy();
  }

  private postWhy(): void {
    this.view?.webview.postMessage({ kind: "why", state: this.whyState } satisfies ToWebview);
  }

  /** Title-bar refresh: reload recent + re-run the current search in the webview. */
  refresh(): void {
    this.view?.webview.postMessage({ kind: "refresh" } satisfies ToWebview);
    clearRelatedCache(); // an explicit refresh should re-query, not replay the cache
    if (!config<boolean>("ambientRecall", true)) {
      // Turned off: clear the section rather than leaving stale matches on screen.
      this.view?.webview.postMessage({
        kind: "related",
        label: "",
        results: [],
      } satisfies ToWebview);
    }
    this.watcher.refresh();
  }

  dispose(): void {
    this.watcher.dispose();
  }
}
