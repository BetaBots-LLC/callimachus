// Thread actions shared by the webview RPC and the palette commands. Each takes a
// plain thread id so both callers hit the same core.

import * as vscode from "vscode";
import { catThread, config, runCal } from "./cal";

/** Insert a thread's transcript at the cursor of the active editor. */
export async function insertThreadById(id: number): Promise<void> {
  const editor = vscode.window.activeTextEditor;
  if (!editor) {
    vscode.window.showInformationMessage("Callimachus: open an editor to insert into.");
    return;
  }
  const md = await catThread(id);
  await editor.edit((b) => b.insert(editor.selection.active, md));
}

/** Copy a thread's packed context to the clipboard. */
export async function copyThreadById(id: number): Promise<void> {
  await vscode.env.clipboard.writeText(await catThread(id));
  vscode.window.showInformationMessage("Callimachus: thread context copied.");
}

/**
 * Export a thread as an Obsidian note. With `callimachus.vaultPath` set, write it
 * into the vault; otherwise open the note as an untitled markdown document.
 */
export async function exportThreadById(id: number): Promise<void> {
  const vault = config<string>("vaultPath", "").trim();
  if (vault) {
    await runCal(["export", String(id), "--vault", vault]);
    vscode.window.showInformationMessage(`Callimachus: exported thread ${id} to ${vault}.`);
    return;
  }
  const md = await runCal(["export", String(id)]);
  const doc = await vscode.workspace.openTextDocument({ content: md, language: "markdown" });
  await vscode.window.showTextDocument(doc, { preview: false });
}

/** Seed a CLI agent (default `claude`) with the thread's context in a terminal. */
export async function openInCli(id: number): Promise<void> {
  const program = config<string>("openCommand", "claude").trim() || "claude";
  const bin = config<string>("calPath", "cal");
  const term = vscode.window.createTerminal(`Callimachus ▸ thread ${id}`);
  term.show();
  // Subshell keeps the (large) context off the visible command line / argv limits.
  term.sendText(`${program} "$(${bin} cat ${id})"`);
}
