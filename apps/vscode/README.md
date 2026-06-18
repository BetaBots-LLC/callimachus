# Callimachus for VS Code & Cursor

Search your AI coding-agent conversation history — across **Claude Code, Codex, Cursor, Gemini, Qwen, Goose, OpenCode, Continue, Cline, Roo Code, Kilo Code** and in-app chats — without leaving the editor. Pick a past thread and open its full transcript as a markdown doc.

This extension is a thin client over the **`cal`** CLI, so it shares the exact same local index as the Callimachus desktop app and MCP server. No separate database, no separate indexing.

## Install

- **VS Code** — [VS Code Marketplace](https://marketplace.visualstudio.com/) (search "Callimachus").
- **Cursor / VSCodium** — [Open VSX](https://open-vsx.org/) (these editors install from Open VSX, not the VS Code Marketplace).
- **Manual** — download the `.vsix` from [Releases](../../../releases) and run *Extensions: Install from VSIX…*.

## Requirements

- The **`cal`** CLI on your `PATH` (ships with Callimachus; `cargo install --path apps/desktop/src-tauri --bin cal`), or set `callimachus.calPath` to its absolute path.
- The Callimachus desktop app run at least once, so the index exists.

## Sidebar

A **Callimachus History** view in the Explorer lists your most recent threads — click to open, right-click to insert or copy, refresh from the title bar.

## Commands

| Command | What it does |
|---|---|
| **Callimachus: Search History** | Search every indexed source, pick a thread, open the transcript. |
| **Callimachus: Search History (current project)** | Same, scoped to the open workspace folder's path. |
| **Callimachus: Search History for Selection** | Use the current editor selection as the query. |
| **Callimachus: Recent Threads** | Browse the most recently updated threads. |
| **Callimachus: Insert Thread into Editor** | Insert a thread's transcript at the cursor (seed a chat / notes). |
| **Callimachus: Copy Thread Context** | Copy a thread's packed transcript to the clipboard. |

There's also a **status-bar** button (`$(history) Callimachus`) that opens search.

## Settings

- `callimachus.calPath` — path to the `cal` binary (default `cal`).
- `callimachus.resultLimit` — max results per search (default 40).

## Develop

```bash
pnpm install          # from the monorepo root (pulls @types/vscode etc.)
pnpm --filter callimachus-vscode build
# then press F5 in VS Code to launch an Extension Development Host
pnpm --filter callimachus-vscode package   # build a .vsix with @vscode/vsce
```
