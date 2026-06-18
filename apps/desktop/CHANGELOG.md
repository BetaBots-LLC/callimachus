# callimachus

## 0.4.2

### Patch Changes

- 118eb13: Make the VS Code / Cursor extension work without manual setup, and fail gracefully when it can't.

  The extension is a thin client over the `cal` CLI, so without it nothing worked — and `cal` wasn't installed by anything. Now:

  - **The desktop app installs `cal`.** The one-click "Enable for Claude Code" action symlinks `~/.local/bin/cal` to the app, which runs in `cal` mode when invoked by that name (same dual-mode trick as `--mcp`). No separate binary to ship, no cargo.
  - **The extension auto-discovers `cal`** in the app's known install locations (`~/.local/bin`, `/Applications/Callimachus.app/...`, Homebrew, Windows install dirs) before falling back to PATH — zero-config for app users.
  - **Friendly empty state.** If `cal` is missing or the index hasn't been built, the extension shows a "Download Callimachus" prompt instead of a raw error, and points to the download page.

## 0.4.1

### Patch Changes

- 7c82648: Fix native scrollbars (and other native controls) showing the wrong color in packaged builds. The app set its theme via a `.dark` class but never declared CSS `color-scheme`, so the WebView painted native scrollbars using the macOS system appearance instead of the app theme. Declaring `color-scheme: light` / `dark` ties them to the active theme.

## 0.4.0

### Minor Changes

- 4fa22ed: Broaden agent coverage and turn the index into shared, agent-accessible memory.

  **More sources indexed.** Six new coding agents are now indexed and searchable alongside Claude Code, Codex, and Cursor — **Gemini CLI, Qwen Code, Goose, OpenCode, Continue, and Cline** — bringing the total to nine. Each is parsed into the canonical store, kept current by the background watcher, and gets full-text + on-device semantic search for free. The source filter now keeps the three most-used agents as quick chips with the rest under a **More** dropdown.

  **Companion `cal` CLI.** A new terminal binary reads the same index from the shell:

  - `cal search <query> [-y]` — keyword or hybrid search
  - `cal recent` — most recent threads
  - `cal cat <id>` — packed thread context to stdout (pipe into anything)
  - `cal stats` — corpus overview (per-source/role, top projects, embedding coverage)
  - `cal export <id> [--vault DIR]` — write an Obsidian note

  **Agent-accessible memory.** The MCP server gains `recent_threads` and `search_current_project` (auto-scopes to the repo it launched in), and a new **`/recall` skill** teaches agents to query the user's own history before redoing work.

  **One-click Claude Code integration.** Settings → "Enable for Claude Code" installs the `/recall` skill and registers Callimachus as an MCP server with no terminal, cargo, or extra binary — the app runs in a dual `--mcp` mode and registers _itself_ in `~/.claude.json`.

  **Obsidian export.** Threads export as Obsidian-flavored Markdown notes — YAML frontmatter, a `[[project]]` graph link, an optional LLM-synthesized decisions/gotchas/TODOs section, and the full transcript.

  **OpenRouter provider.** Chat can now use OpenRouter (one key, many models) in addition to Anthropic, OpenAI, and Ollama, with live model lists fetched per provider.

  **In-app agent + tools.** The chat can call read-only tools (`search_history`, `get_thread`) and `run_shell` behind an explicit per-command approval, and streaming responses can be cancelled mid-flight.

  **Storage cleanup + analytics.** New tooling to review oldest/largest threads, delete them (cascading to messages, FTS, and vectors), and reclaim space, plus an index analytics view.

## 0.3.0

### Minor Changes

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
