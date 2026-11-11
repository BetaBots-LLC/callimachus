// The Memory view (`callimachus.memory`, a tree under the History webview): the open
// folder's project memory, from one `cal memory --json` run in that folder, and its
// recurring errors, from `cal issues <project> --json`, fetched only when that group is
// expanded. Read-only: a fact, an error or its fix opens its session
// (`callimachus.openThread`), and no row runs anything else; the title bar has only
// Refresh. What the rows hold and which calls are allowed live in ./memoryModel (no
// `vscode` import, tested with `node --test`).
//
// VS Code API at the 1.85 floor (@types/vscode 1.85.0): createTreeView, TreeView.message /
// description, a Promise from getChildren, withProgress on a view id. A failed load sets
// the context key `callimachus.memoryState` (the `setContext` command), which picks the
// view's `viewsWelcome` text in package.json; welcome content shows only on an empty tree
// with no TreeView.message (code.visualstudio.com/api/extension-guides/tree-view).

import { realpathSync } from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import * as vscode from "vscode";
import { calHasCommand, runCal } from "./cal";
import {
  childNodes,
  type Issue,
  issueNodes,
  issuesProblemText,
  loadIssues,
  loadMemory,
  type MemoryNode,
  type ProjectMemory,
  type RowView,
  rootNodes,
  rowView,
  stateOfError,
  stateOfMemory,
  type ViewState,
} from "./memoryModel";

const VIEW_ID = "callimachus.memory";
const STATE_KEY = "callimachus.memoryState";

const NO_COMMAND = (command: string) =>
  `This Callimachus app has no \`cal ${command}\`. Update the app, then refresh.`;

/** A loaded memory answer, with the folder it was asked in and its top rows. */
type Loaded = { folder: string; mem: ProjectMemory; roots: MemoryNode[] };

type IssuesSlot =
  | { status: "loading"; promise: Promise<MemoryNode[]> }
  | { status: "done"; nodes: MemoryNode[]; count?: number };

const COLLAPSIBLE: Record<RowView["collapsible"], vscode.TreeItemCollapsibleState> = {
  none: vscode.TreeItemCollapsibleState.None,
  collapsed: vscode.TreeItemCollapsibleState.Collapsed,
  expanded: vscode.TreeItemCollapsibleState.Expanded,
};

/** The folder whose project the view shows: the first workspace folder on disk, as the
 *  History view's project scope and the palette's project search use. */
function workspaceFolder(): string | undefined {
  const folder = vscode.workspace.workspaceFolders?.[0];
  return folder?.uri.scheme === "file" ? folder.uri.fsPath : undefined;
}

/** The home folder as cal keys projects: `indexer::canonical_project` canonicalizes both the
 *  folder and home (Rust's fs::canonicalize, the OS call `realpathSync.native` makes too), so
 *  a symlinked home compares by its target. */
function homeFolder(): string {
  const home = os.homedir();
  try {
    return realpathSync.native(home);
  } catch {
    return home;
  }
}

export class MemoryView implements vscode.TreeDataProvider<MemoryNode>, vscode.Disposable {
  private readonly changed = new vscode.EventEmitter<MemoryNode | undefined>();
  readonly onDidChangeTreeData = this.changed.event;
  private readonly view: vscode.TreeView<MemoryNode>;
  private readonly disposables: vscode.Disposable[] = [];
  // Each refresh starts a new generation; a slower earlier load must not land on top of it,
  // and its `cal` runs are killed (`abort`): a scoped `cal issues` mines for up to
  // ISSUES_TIMEOUT_MS, and the group's stable id keeps it expanded across a refresh
  // (TreeItem.id, @types/vscode 1.85.0), so each Refresh would otherwise start one more
  // next to the old ones.
  private seq = 0;
  private abort = new AbortController();
  private load?: { seq: number; promise: Promise<void> };
  private loaded = false;
  private memory?: Loaded;
  private issues?: IssuesSlot;

  constructor() {
    this.view = vscode.window.createTreeView(VIEW_ID, {
      treeDataProvider: this,
      showCollapseAll: true,
    });
    this.disposables.push(
      this.view,
      this.changed,
      vscode.workspace.onDidChangeWorkspaceFolders(() => this.refresh()),
    );
  }

  /** Title-bar Refresh: ask `cal` again (the view never watches files). */
  refresh(): void {
    this.endGeneration();
    this.load = undefined;
    this.loaded = false;
    this.memory = undefined;
    this.issues = undefined;
    this.view.message = undefined;
    void this.setState("loading");
    this.changed.fire(undefined);
  }

  /** Drop the current generation's answers and kill its running `cal` processes. */
  private endGeneration(): void {
    this.seq++;
    this.abort.abort();
    this.abort = new AbortController();
  }

  getTreeItem(node: MemoryNode): vscode.TreeItem {
    const count =
      node.type === "errors" && this.issues?.status === "done" ? this.issues.count : undefined;
    const row = rowView(node, count);
    const item = new vscode.TreeItem(row.label, COLLAPSIBLE[row.collapsible]);
    item.id = row.id;
    item.description = row.description;
    item.tooltip = row.tooltip;
    item.iconPath = new vscode.ThemeIcon(row.icon);
    if (row.command) {
      item.command = {
        command: row.command.command,
        title: row.command.title,
        arguments: [...row.command.arguments],
      };
    }
    return item;
  }

  getChildren(node?: MemoryNode): vscode.ProviderResult<MemoryNode[]> {
    if (!node) {
      if (this.loaded) return this.memory?.roots ?? [];
      return this.startLoad().then(() => this.memory?.roots ?? []);
    }
    if (node.type === "errors") return this.errorChildren(node);
    return childNodes(node);
  }

  private startLoad(): Promise<void> {
    if (this.load?.seq === this.seq) return this.load.promise;
    const seq = this.seq;
    const { signal } = this.abort;
    const promise = Promise.resolve(
      vscode.window.withProgress({ location: { viewId: VIEW_ID } }, () => this.fetch(seq, signal)),
    );
    this.load = { seq, promise };
    return promise;
  }

  private async fetch(seq: number, signal: AbortSignal): Promise<void> {
    const folder = workspaceFolder();
    let state: ViewState;
    let message: string | undefined;
    let loaded: Loaded | undefined;
    if (!folder) {
      state = "noFolder";
    } else {
      try {
        // The app binary under its own name opens its window for a command it doesn't know.
        if (!(await calHasCommand("memory"))) throw new Error(NO_COMMAND("memory"));
        const mem = await loadMemory(runCal, folder, { signal });
        loaded = { folder, mem, roots: rootNodes(mem, homeFolder()) };
        state = stateOfMemory(mem);
      } catch (err) {
        ({ state, message } = stateOfError(err));
      }
    }
    if (seq !== this.seq) return; // a refresh (or dispose) superseded this run
    // An empty memory still has rows when it has a project (a notice and Recurring errors);
    // with none, the tree is empty and the `empty` welcome text shows.
    this.memory = loaded;
    this.loaded = true;
    this.view.message = state === "error" ? `Callimachus: ${message}` : undefined;
    this.view.description = loaded ? path.basename(loaded.mem.project) || undefined : undefined;
    await this.setState(state);
  }

  private setState(state: ViewState): Thenable<unknown> {
    return vscode.commands.executeCommand("setContext", STATE_KEY, state);
  }

  /** The Recurring errors group's rows, fetched on its first expand per refresh. */
  private errorChildren(node: MemoryNode): MemoryNode[] | Promise<MemoryNode[]> {
    const slot = this.issues;
    if (slot?.status === "done") return slot.nodes;
    if (slot?.status === "loading") return slot.promise;
    const loaded = this.memory;
    if (!loaded) return [];
    const seq = this.seq;
    const { signal } = this.abort;
    // A scoped `cal issues` mines live and can take seconds (ISSUES_TIMEOUT_MS): show it.
    const fetching = vscode.window.withProgress({ location: { viewId: VIEW_ID } }, () =>
      this.fetchIssues(loaded.folder, loaded.mem.project, signal),
    );
    const promise = Promise.resolve(fetching).then((result) => {
      if (seq === this.seq) {
        this.issues = { status: "done", ...result };
        this.changed.fire(node); // the group's description shows the count
      }
      return result.nodes;
    });
    this.issues = { status: "loading", promise };
    return promise;
  }

  private async fetchIssues(
    folder: string,
    project: string,
    signal: AbortSignal,
  ): Promise<{ nodes: MemoryNode[]; count?: number }> {
    try {
      if (!(await calHasCommand("issues"))) {
        return { nodes: [{ type: "info", text: NO_COMMAND("issues") }] };
      }
      const issues: Issue[] | null = await loadIssues(runCal, folder, project, {
        signal,
        home: homeFolder(),
      });
      if (!issues) return { nodes: [] };
      return { nodes: issueNodes(issues), count: issues.length };
    } catch (err) {
      return { nodes: [{ type: "info", text: issuesProblemText(err) }] };
    }
  }

  dispose(): void {
    // Kill a running `cal` (cal is spawned detached, so nothing else stops it) and keep its
    // late answer off the disposed view.
    this.endGeneration();
    for (const d of this.disposables) d.dispose();
  }
}
