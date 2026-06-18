// Callimachus VS Code extension. Thin client over the `cal` CLI so the editor
// shares the exact same local index as the desktop app and MCP server — no
// duplicated indexing, no separate DB.

import * as vscode from "vscode";
import {
  catThread,
  openThread,
  recentThreads,
  searchHits,
  stripMarks,
  type SearchHit,
} from "./cal";
import { RecentThreadsProvider, ThreadNode } from "./tree";

type ThreadItem = vscode.QuickPickItem & { threadId: number };

/** Search hits → quick-pick items, deduped to one entry per thread. */
function hitsToItems(hits: SearchHit[]): ThreadItem[] {
  const seen = new Set<number>();
  const items: ThreadItem[] = [];
  for (const h of hits) {
    if (seen.has(h.threadId)) continue;
    seen.add(h.threadId);
    items.push({
      threadId: h.threadId,
      label: h.title?.trim() || "(untitled)",
      description: h.source,
      detail: stripMarks(h.snippet),
    });
  }
  return items;
}

async function pickAndOpen(items: ThreadItem[], placeHolder: string): Promise<void> {
  if (items.length === 0) {
    vscode.window.showInformationMessage("Callimachus: no matching threads.");
    return;
  }
  const choice = await vscode.window.showQuickPick(items, {
    placeHolder,
    matchOnDescription: true,
    matchOnDetail: true,
  });
  if (choice) await openThread(choice.threadId);
}

async function doSearch(scopeToProject: boolean, presetQuery?: string): Promise<void> {
  const query =
    presetQuery ??
    (await vscode.window.showInputBox({
      prompt: scopeToProject
        ? "Search this project's AI agent history"
        : "Search all your AI agent history",
      placeHolder: "e.g. vector index migration",
    }));
  if (!query) return;

  const project = scopeToProject
    ? vscode.workspace.workspaceFolders?.[0]?.uri.fsPath
    : undefined;

  await vscode.window.withProgress(
    { location: vscode.ProgressLocation.Window, title: "Callimachus: searching…" },
    async () => {
      const hits = await searchHits(query, project);
      await pickAndOpen(hitsToItems(hits), `${hits.length} hits for "${query}"`);
    },
  );
}

async function doRecent(): Promise<void> {
  const rows = await recentThreads();
  const items: ThreadItem[] = rows.map((t) => ({
    threadId: t.id,
    label: t.title?.trim() || "(untitled)",
    description: t.source,
    detail: `${t.messageCount} msgs${t.projectPath ? ` · ${t.projectPath}` : ""}`,
  }));
  await pickAndOpen(items, "Recent threads");
}

/** Use the editor selection as the query; fall back to the search input box. */
async function searchSelection(): Promise<void> {
  const editor = vscode.window.activeTextEditor;
  const sel = editor?.document.getText(editor.selection).trim();
  await doSearch(false, sel || undefined);
}

/** Resolve a thread id from a tree node arg, or prompt the user to pick one. */
async function resolveThreadId(arg: unknown): Promise<number | undefined> {
  if (arg instanceof ThreadNode) return arg.thread.id;
  const rows = await recentThreads();
  const choice = await vscode.window.showQuickPick(
    rows.map((t) => ({
      threadId: t.id,
      label: t.title?.trim() || "(untitled)",
      description: t.source,
    })),
    { placeHolder: "Pick a thread", matchOnDescription: true },
  );
  return choice?.threadId;
}

async function insertThread(arg: unknown): Promise<void> {
  const editor = vscode.window.activeTextEditor;
  if (!editor) {
    vscode.window.showInformationMessage("Callimachus: open an editor to insert into.");
    return;
  }
  const id = await resolveThreadId(arg);
  if (id === undefined) return;
  const md = await catThread(id);
  await editor.edit((b: vscode.TextEditorEdit) => b.insert(editor.selection.active, md));
}

async function copyThread(arg: unknown): Promise<void> {
  const id = await resolveThreadId(arg);
  if (id === undefined) return;
  await vscode.env.clipboard.writeText(await catThread(id));
  vscode.window.showInformationMessage("Callimachus: thread context copied.");
}

/** Wrap a command so any thrown error surfaces as a VS Code notification. */
function guard<A extends unknown[]>(
  fn: (...args: A) => Promise<void>,
): (...args: A) => Promise<void> {
  return async (...args: A) => {
    try {
      await fn(...args);
    } catch (err) {
      vscode.window.showErrorMessage(`Callimachus: ${(err as Error).message}`);
    }
  };
}

export function activate(context: vscode.ExtensionContext): void {
  const recent = new RecentThreadsProvider();

  const status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 0);
  status.text = "$(history) Callimachus";
  status.tooltip = "Search your AI coding-agent history";
  status.command = "callimachus.search";
  status.show();

  context.subscriptions.push(
    status,
    vscode.window.registerTreeDataProvider("callimachus.recent", recent),
    vscode.commands.registerCommand("callimachus.search", guard(() => doSearch(false))),
    vscode.commands.registerCommand("callimachus.searchCurrentProject", guard(() => doSearch(true))),
    vscode.commands.registerCommand("callimachus.searchSelection", guard(searchSelection)),
    vscode.commands.registerCommand("callimachus.recent", guard(doRecent)),
    vscode.commands.registerCommand("callimachus.insertThread", guard(insertThread)),
    vscode.commands.registerCommand("callimachus.copyThread", guard(copyThread)),
    vscode.commands.registerCommand("callimachus.refreshRecent", () => recent.refresh()),
    vscode.commands.registerCommand(
      "callimachus.openThread",
      guard((id: number) => openThread(id)),
    ),
  );
}

export function deactivate(): void {
  // no-op: registrations are disposed via context.subscriptions
}
