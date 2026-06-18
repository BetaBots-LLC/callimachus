---
"callimachus": minor
---

Broaden agent coverage and turn the index into shared, agent-accessible memory.

**More sources indexed.** Six new coding agents are now indexed and searchable alongside Claude Code, Codex, and Cursor — **Gemini CLI, Qwen Code, Goose, OpenCode, Continue, and Cline** — bringing the total to nine. Each is parsed into the canonical store, kept current by the background watcher, and gets full-text + on-device semantic search for free. The source filter now keeps the three most-used agents as quick chips with the rest under a **More** dropdown.

**Companion `cal` CLI.** A new terminal binary reads the same index from the shell:
- `cal search <query> [-y]` — keyword or hybrid search
- `cal recent` — most recent threads
- `cal cat <id>` — packed thread context to stdout (pipe into anything)
- `cal stats` — corpus overview (per-source/role, top projects, embedding coverage)
- `cal export <id> [--vault DIR]` — write an Obsidian note

**Agent-accessible memory.** The MCP server gains `recent_threads` and `search_current_project` (auto-scopes to the repo it launched in), and a new **`/recall` skill** teaches agents to query the user's own history before redoing work.

**One-click Claude Code integration.** Settings → "Enable for Claude Code" installs the `/recall` skill and registers Callimachus as an MCP server with no terminal, cargo, or extra binary — the app runs in a dual `--mcp` mode and registers *itself* in `~/.claude.json`.

**Obsidian export.** Threads export as Obsidian-flavored Markdown notes — YAML frontmatter, a `[[project]]` graph link, an optional LLM-synthesized decisions/gotchas/TODOs section, and the full transcript.

**OpenRouter provider.** Chat can now use OpenRouter (one key, many models) in addition to Anthropic, OpenAI, and Ollama, with live model lists fetched per provider.

**In-app agent + tools.** The chat can call read-only tools (`search_history`, `get_thread`) and `run_shell` behind an explicit per-command approval, and streaming responses can be cancelled mid-flight.

**Storage cleanup + analytics.** New tooling to review oldest/largest threads, delete them (cascading to messages, FTS, and vectors), and reclaim space, plus an index analytics view.
