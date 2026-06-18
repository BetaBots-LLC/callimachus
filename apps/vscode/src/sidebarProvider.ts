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
import { config, relatedThreads } from "./cal";
import { type EditorContext, EditorContextWatcher } from "./editorContext";
import type { FromWebview, InitPayload, ToWebview } from "./protocol";

export class SidebarProvider implements vscode.WebviewViewProvider, vscode.Disposable {
  public static readonly viewId = "callimachus.sidebar";
  private view?: vscode.WebviewView;
  private readonly watcher: EditorContextWatcher;

  constructor(private readonly extensionUri: vscode.Uri) {
    this.watcher = new EditorContextWatcher((ctx) => void this.onContext(ctx));
  }

  resolveWebviewView(view: vscode.WebviewView): void {
    this.view = view;
    view.webview.options = {
      enableScripts: true,
      localResourceRoots: [vscode.Uri.joinPath(this.extensionUri, "media")],
    };
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
        // Now the webview is listening — kick off the first ambient query.
        this.watcher.refresh();
        return;
      }
      handleWebviewMessage(view.webview, msg, {
        openThread: (id, title) => openThreadPanel(this.extensionUri, id, title),
      });
    });
  }

  /** Ambient recall: query `cal related` for the current editor context + push. */
  private async onContext(ctx: EditorContext | null): Promise<void> {
    const view = this.view;
    if (!view?.visible || !config<boolean>("ambientRecall", true)) return;
    if (!ctx) {
      view.webview.postMessage({ kind: "related", label: "", results: [] } satisfies ToWebview);
      return;
    }
    try {
      const results = await relatedThreads(ctx.text);
      // The view may have been hidden during the await.
      if (this.view?.visible) {
        this.view.webview.postMessage({
          kind: "related",
          label: ctx.label,
          results,
        } satisfies ToWebview);
      }
    } catch {
      // Background feature — stay quiet (e.g. cal missing, index not embedded).
    }
  }

  /** Title-bar refresh: reload recent + re-run the current search in the webview. */
  refresh(): void {
    this.view?.webview.postMessage({ kind: "refresh" } satisfies ToWebview);
    this.watcher.refresh();
  }

  dispose(): void {
    this.watcher.dispose();
  }
}
