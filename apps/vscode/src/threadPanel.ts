// Per-thread transcript, shown as an editor webview tab. One panel per thread id
// (re-revealed on a repeat open). The same bundle as the sidebar renders here.

import * as vscode from "vscode";
import { getHtml } from "./webviewHtml";
import { handleWebviewMessage } from "./webviewMessages";
import type { FromWebview, InitPayload, ToWebview } from "./protocol";

const panels = new Map<number, vscode.WebviewPanel>();

export function openThreadPanel(extensionUri: vscode.Uri, id: number, title?: string | null): void {
  const existing = panels.get(id);
  if (existing) {
    existing.reveal();
    return;
  }

  const panel = vscode.window.createWebviewPanel(
    "callimachus.thread",
    title?.trim() || `Thread ${id}`,
    vscode.ViewColumn.Active,
    {
      enableScripts: true,
      retainContextWhenHidden: true,
      localResourceRoots: [vscode.Uri.joinPath(extensionUri, "media")],
    },
  );
  panels.set(id, panel);
  panel.onDidDispose(() => panels.delete(id));
  panel.webview.html = getHtml(panel.webview, extensionUri);

  panel.webview.onDidReceiveMessage((msg: FromWebview) => {
    if (msg.kind === "ready") {
      const init: InitPayload = { view: "thread", threadId: id, title: title ?? null };
      panel.webview.postMessage({ kind: "init", init } satisfies ToWebview);
      return;
    }
    handleWebviewMessage(panel.webview, msg, {
      openThread: (tid, t) => openThreadPanel(extensionUri, tid, t),
    });
  });
}
