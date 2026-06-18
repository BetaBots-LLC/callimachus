// Dispatches messages coming from a webview. Both surfaces share this for the RPC
// methods and the thread actions; the `ready` handshake (which init payload to
// send) is handled by each provider since it differs per surface.

import * as vscode from "vscode";
import { catThread, recentThreads, searchHits, showCalSetupPrompt, stats } from "./cal";
import { copyThreadById, exportThreadById, insertThreadById, openInCli } from "./actions";
import type { FromWebview, RpcMethod, ToWebview } from "./protocol";

export interface WebviewDeps {
  openThread(id: number, title?: string | null): void;
}

const post = (webview: vscode.Webview, msg: ToWebview) => webview.postMessage(msg);

export async function handleWebviewMessage(
  webview: vscode.Webview,
  msg: FromWebview,
  deps: WebviewDeps,
): Promise<void> {
  if (msg.kind === "rpc") {
    try {
      post(webview, { kind: "rpc-result", id: msg.id, ok: true, result: await runRpc(msg.method, msg.params) });
    } catch (err) {
      showCalSetupPrompt(err); // download/setup toast for missing cal / no index
      post(webview, { kind: "rpc-result", id: msg.id, ok: false, error: (err as Error).message });
    }
    return;
  }
  if (msg.kind === "action") {
    try {
      switch (msg.action) {
        case "openThread":
          deps.openThread(msg.id, msg.title);
          break;
        case "insertThread":
          await insertThreadById(msg.id);
          break;
        case "copyThread":
          await copyThreadById(msg.id);
          break;
        case "exportThread":
          await exportThreadById(msg.id);
          break;
        case "openInCli":
          await openInCli(msg.id);
          break;
      }
    } catch (err) {
      if (!showCalSetupPrompt(err)) {
        vscode.window.showErrorMessage(`Callimachus: ${(err as Error).message}`);
      }
    }
  }
}

async function runRpc(method: RpcMethod, params: unknown): Promise<unknown> {
  switch (method) {
    case "search": {
      const p = params as { query: string; project?: string | null };
      return searchHits(p.query, p.project ?? undefined);
    }
    case "recent":
      return recentThreads();
    case "stats":
      return stats();
    case "cat":
      return catThread((params as { id: number }).id);
  }
}
