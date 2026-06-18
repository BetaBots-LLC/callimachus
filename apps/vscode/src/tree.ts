// Explorer sidebar view: the user's most recent threads, click to open.

import * as vscode from "vscode";
import { recentThreads, type ThreadSummary } from "./cal";

/** One thread row in the tree. Carries its summary for the context-menu actions. */
export class ThreadNode extends vscode.TreeItem {
  constructor(public readonly thread: ThreadSummary) {
    super(thread.title?.trim() || "(untitled)", vscode.TreeItemCollapsibleState.None);
    this.description = thread.source;
    this.tooltip = `${thread.source} · ${thread.messageCount} msgs${
      thread.projectPath ? `\n${thread.projectPath}` : ""
    }`;
    this.contextValue = "thread";
    this.iconPath = new vscode.ThemeIcon("comment-discussion");
    this.command = {
      command: "callimachus.openThread",
      title: "Open",
      arguments: [thread.id],
    };
  }
}

export class RecentThreadsProvider implements vscode.TreeDataProvider<ThreadNode> {
  private readonly _onDidChange = new vscode.EventEmitter<ThreadNode | undefined | void>();
  readonly onDidChangeTreeData = this._onDidChange.event;

  refresh(): void {
    this._onDidChange.fire();
  }

  getTreeItem(element: ThreadNode): vscode.TreeItem {
    return element;
  }

  async getChildren(): Promise<ThreadNode[]> {
    try {
      const rows = await recentThreads();
      return rows.map((t) => new ThreadNode(t));
    } catch (err) {
      vscode.window.showErrorMessage(`Callimachus: ${(err as Error).message}`);
      return [];
    }
  }
}
