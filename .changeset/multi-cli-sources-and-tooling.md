---
"callimachus": minor
"callimachus-vscode": patch
---

Index, search, and use your history across 11 coding agents — plus new ways to reach it.

- **More sources:** added Gemini CLI, Qwen Code, Goose, OpenCode, Continue, Cline, Roo Code, and Kilo Code indexers (now 11 in total), each with live file-watching and per-source reindex.
- **Chat:** OpenRouter and Gemini providers; the in-app chat is now a tool-using agent that can search your own history and run shell commands with your approval; streaming is cancellable with live model lists.
- **MCP server** (`callimachus-mcp`): exposes `search_threads`, `search_current_project`, `recent_threads`, and `get_thread` to any agent, plus a `/recall` skill.
- **`cal` CLI:** `search` / `recent` / `cat` / `stats` / `export` against the same local index.
- **VS Code / Cursor extension:** search history, recent-threads sidebar, insert/copy a thread, and a status-bar entry.
- **Stats** dashboard (per-source / per-role / top projects / embedding coverage).
- **Storage cleanup:** paginated table to delete old threads and reclaim disk space.
- **Obsidian export** of a thread, optionally AI-summarized with decisions / gotchas / TODOs.
- **Performance:** much faster incremental semantic indexing (per-message `embedded` flag) and a precomputed thread-size column so Settings stays responsive on large histories.
