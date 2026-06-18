# callimachus-vscode

## 0.3.0

### Minor Changes

- 9379aff: Add a rich webview UI for VS Code & Cursor.

  - **Callimachus sidebar** in a new Activity Bar container: live search over your whole AI-history index with All / This-project scope, a recent-threads list, hover insert/copy, and a corpus-stats footer.
  - **Transcript tabs:** pick a result to open its conversation in an editor tab, styled to match the desktop app — right-aligned user bubbles, full-markdown assistant turns, and collapsible tool calls.
  - **Themed to the editor:** the UI follows your active VS Code / Cursor theme (light / dark / high-contrast) via the editor's own theme variables.
  - Built with Vite, reusing the desktop app's shadcn components and Markdown renderer; data flows over a typed message bridge to the `cal` CLI (no Tauri in the editor).
  - Adds `callimachus.vaultPath` (Export destination) and `callimachus.openCommand` (Open-in-CLI agent) settings.

  Replaces the old Explorer "Callimachus History" tree view.

## 0.2.1

### Patch Changes

- 541bd70: Index, search, and use your history across 11 coding agents — plus new ways to reach it.

  - **More sources:** added Gemini CLI, Qwen Code, Goose, OpenCode, Continue, Cline, Roo Code, and Kilo Code indexers (now 11 in total), each with live file-watching and per-source reindex.
  - **Chat:** OpenRouter and Gemini providers; the in-app chat is now a tool-using agent that can search your own history and run shell commands with your approval; streaming is cancellable with live model lists.
  - **MCP server** (`callimachus-mcp`): exposes `search_threads`, `search_current_project`, `recent_threads`, and `get_thread` to any agent, plus a `/recall` skill.
  - **`cal` CLI:** `search` / `recent` / `cat` / `stats` / `export` against the same local index.
  - **VS Code / Cursor extension:** search history, recent-threads sidebar, insert/copy a thread, and a status-bar entry.
  - **Stats** dashboard (per-source / per-role / top projects / embedding coverage).
  - **Storage cleanup:** paginated table to delete old threads and reclaim disk space.
  - **Obsidian export** of a thread, optionally AI-summarized with decisions / gotchas / TODOs.
  - **Performance:** much faster incremental semantic indexing (per-message `embedded` flag) and a precomputed thread-size column so Settings stays responsive on large histories.

## 0.2.0

### Minor Changes

- 4b7f43f: Initial release of Callimachus — index, search, and manage your AI chat threads (Claude, Codex, and more) from one desktop app.
