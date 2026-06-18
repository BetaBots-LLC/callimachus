// Callimachus VS Code / Cursor extension. Thin client over the `cal` CLI so the
// editor shares the exact same local index as the desktop app and MCP server.
//
// The primary UI is a React webview: a search/recent/stats sidebar in the
// Callimachus Activity Bar container, and per-thread transcript tabs. The
// QuickPick commands below remain as Command Palette fallbacks.

import * as vscode from "vscode";
import { recentThreads, searchHits, stripMarks } from "./cal";
import { copyThreadById, insertThreadById } from "./actions";
import { openThreadPanel } from "./threadPanel";
import { SidebarProvider } from "./sidebarProvider";
import type { SearchHit } from "./protocol";

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

async function pickAndOpen(
  extensionUri: vscode.Uri,
  items: ThreadItem[],
  placeHolder: string,
): Promise<void> {
  if (items.length === 0) {
    vscode.window.showInformationMessage("Callimachus: no matching threads.");
    return;
  }
  const choice = await vscode.window.showQuickPick(items, {
    placeHolder,
    matchOnDescription: true,
    matchOnDetail: true,
  });
  if (choice) openThreadPanel(extensionUri, choice.threadId, choice.label);
}

async function doSearch(
  extensionUri: vscode.Uri,
  scopeToProject: boolean,
  presetQuery?: string,
): Promise<void> {
  const query =
    presetQuery ??
    (await vscode.window.showInputBox({
      prompt: scopeToProject
        ? "Search this project's AI agent history"
        : "Search all your AI agent history",
      placeHolder: "e.g. vector index migration",
    }));
  if (!query) return;

  const project = scopeToProject ? vscode.workspace.workspaceFolders?.[0]?.uri.fsPath : undefined;

  await vscode.window.withProgress(
    { location: vscode.ProgressLocation.Window, title: "Callimachus: searching…" },
    async () => {
      const hits = await searchHits(query, project);
      await pickAndOpen(extensionUri, hitsToItems(hits), `${hits.length} hits for "${query}"`);
    },
  );
}

async function doRecent(extensionUri: vscode.Uri): Promise<void> {
  const rows = await recentThreads();
  const items: ThreadItem[] = rows.map((t) => ({
    threadId: t.id,
    label: t.title?.trim() || "(untitled)",
    description: t.source,
    detail: `${t.messageCount} msgs${t.projectPath ? ` · ${t.projectPath}` : ""}`,
  }));
  await pickAndOpen(extensionUri, items, "Recent threads");
}

/** Use the editor selection as the query; fall back to the search input box. */
async function searchSelection(extensionUri: vscode.Uri): Promise<void> {
  const editor = vscode.window.activeTextEditor;
  const sel = editor?.document.getText(editor.selection).trim();
  await doSearch(extensionUri, false, sel || undefined);
}

/** Prompt the user to pick a recent thread; returns its id. */
async function pickThreadId(): Promise<number | undefined> {
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

/** Resolve a thread id from a numeric command arg, else prompt. */
async function resolveThreadId(arg: unknown): Promise<number | undefined> {
  return typeof arg === "number" ? arg : pickThreadId();
}

/** Wrap a command so any thrown error surfaces as a VS Code notification. */
function guard<A extends unknown[]>(
  fn: (...args: A) => Promise<void> | void,
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
  const { extensionUri } = context;
  const sidebar = new SidebarProvider(extensionUri);

  const status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 0);
  status.text = "$(history) Callimachus";
  status.tooltip = "Search your AI coding-agent history";
  status.command = "callimachus.sidebar.focus"; // auto-registered for the view id
  status.show();

  context.subscriptions.push(
    status,
    vscode.window.registerWebviewViewProvider(SidebarProvider.viewId, sidebar, {
      webviewOptions: { retainContextWhenHidden: true },
    }),
    vscode.commands.registerCommand("callimachus.search", guard(() => doSearch(extensionUri, false))),
    vscode.commands.registerCommand(
      "callimachus.searchCurrentProject",
      guard(() => doSearch(extensionUri, true)),
    ),
    vscode.commands.registerCommand(
      "callimachus.searchSelection",
      guard(() => searchSelection(extensionUri)),
    ),
    vscode.commands.registerCommand("callimachus.recent", guard(() => doRecent(extensionUri))),
    vscode.commands.registerCommand(
      "callimachus.insertThread",
      guard(async (arg: unknown) => {
        const id = await resolveThreadId(arg);
        if (id !== undefined) await insertThreadById(id);
      }),
    ),
    vscode.commands.registerCommand(
      "callimachus.copyThread",
      guard(async (arg: unknown) => {
        const id = await resolveThreadId(arg);
        if (id !== undefined) await copyThreadById(id);
      }),
    ),
    vscode.commands.registerCommand("callimachus.refreshRecent", () => sidebar.refresh()),
    vscode.commands.registerCommand(
      "callimachus.openThread",
      guard((id: number) => openThreadPanel(extensionUri, id)),
    ),
  );
}

export function deactivate(): void {
  // no-op: registrations are disposed via context.subscriptions
}
