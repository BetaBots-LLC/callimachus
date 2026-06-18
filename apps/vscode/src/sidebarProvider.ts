// The Activity Bar sidebar webview. Registered with retainContextWhenHidden so
// the search state survives the view being hidden; the webview also persists its
// query via getState/setState as a belt-and-suspenders.

import * as vscode from "vscode";
import { getHtml } from "./webviewHtml";
import { handleWebviewMessage } from "./webviewMessages";
import { openThreadPanel } from "./threadPanel";
import type { FromWebview, InitPayload, ToWebview } from "./protocol";

export class SidebarProvider implements vscode.WebviewViewProvider {
  public static readonly viewId = "callimachus.sidebar";
  private view?: vscode.WebviewView;

  constructor(private readonly extensionUri: vscode.Uri) {}

  resolveWebviewView(view: vscode.WebviewView): void {
    this.view = view;
    view.webview.options = {
      enableScripts: true,
      localResourceRoots: [vscode.Uri.joinPath(this.extensionUri, "media")],
    };
    view.webview.html = getHtml(view.webview, this.extensionUri);

    view.webview.onDidReceiveMessage((msg: FromWebview) => {
      if (msg.kind === "ready") {
        const project = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? null;
        const init: InitPayload = { view: "sidebar", projectPath: project };
        view.webview.postMessage({ kind: "init", init } satisfies ToWebview);
        return;
      }
      handleWebviewMessage(view.webview, msg, {
        openThread: (id, title) => openThreadPanel(this.extensionUri, id, title),
      });
    });
  }

  /** Title-bar refresh: reload recent + re-run the current search in the webview. */
  refresh(): void {
    this.view?.webview.postMessage({ kind: "refresh" } satisfies ToWebview);
  }
}
