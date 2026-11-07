---
"callimachus": patch
"callimachus-vscode": patch
---

The platform layer, second half: contracts other tools can depend on, a complete archive as vendors change formats, and the Claude Code plugin without the desktop app.

- `cal` has a versioned JSON contract, `--json-version 1`. Nothing existing breaks: plain `--json` prints the same bare arrays and objects as before.
  - With `--json-version 1` (or `CAL_JSON_VERSION=1` in the environment beside `--json`), the outputs of `search`, `recent`, `related`, `cat`, `stats`, `memory`, `decisions`, `gotchas`, `commits` and `audit-pr` open with `schema_version` (1). Lists come as `{"schema_version": 1, "results": [...]}`; `stats`, `memory` and `audit-pr` keep their fields next to the version.
  - Each versioned output has a published JSON Schema at callimachus.app/schemas/cal/ (for example search.v1.json), generated from the code so the two cannot drift.
  - Plain `--json` output stays unversioned and may change in a future major release, so scripts that need a stable shape should pass `--json-version 1`.
  - `cal cat` takes `--json` (the thread's id, source, title, project path and the packed transcript).
  - When the embedding model isn't on the machine, `cal decisions` / `cal gotchas --json-version 1` print an empty list with a `notice` saying why, so a script can tell that from no match.
- Every MCP tool declares an output schema and returns structured content that matches it, which is what Claude Code and Codex pass to the model.
  - Lists sit under a key (`threads`, `facts`, `todos` and so on), transcripts and memory under `markdown`, and setup guidance (no index yet, a version mismatch, a busy index, no embedding model) as `notice`.
  - The schemas are published at callimachus.app/schemas/mcp/ (for example recall_decisions.json) and checked in, so a changed key shows up in review.
  - The text block is unchanged for every client; clients older than MCP 2025-06-18 get no structured content. `get_thread` and `load_snapshot` carry the transcript in both, so their responses are about twice `budget`.
- The VS Code extension asks `cal` for the versioned output. Nothing breaks in either direction: an older `cal` keeps answering with the bare output the extension still reads, and an extension already installed keeps working with the new `cal`. When `cal` is newer than the extension can read, it says to update the extension instead of failing on a missing field.
- More of your agent history gets indexed.
  - Cursor: the agent transcripts under `~/.cursor/projects/` are now read (with their subagents), along with the Cursor CLI's chat stores under `~/.cursor/chats/`, so Cursor CLI sessions are searchable and Cursor threads finally carry their project (it comes from the transcript's folder, so Project pages and project-scoped search and memory now include Cursor). Editor chats already indexed from Cursor's state database stay as they are and gain the project without being re-indexed.
  - Codex: the thread database is found where Codex keeps it, honoring `sqlite_home` in `config.toml` and `CODEX_SQLITE_HOME`, and the newest `state_<N>.sqlite` is read instead of only `state_5`, so a Codex update that bumps the schema no longer drops titles and branches.
  - New source, the GitHub Copilot CLI (`copilot_cli`): sessions under `~/.copilot/session-state/` (or `$COPILOT_HOME`) are indexed with their prompts, answers, tool calls and capped tool output, plus each session's token usage per model for the spend view. It has a source filter under More and a reindex button, and shows in `cal stats`; `cal search -s copilot_cli` filters to it.
  - Only interactive Copilot CLI conversations are indexed: sessions run by Copilot SDK clients (GitHub Desktop's commit messages) are skipped.
  - The site, the README and the VS Code extension now count thirteen agents.
- The Claude Code plugin works without the desktop app on macOS and Linux.
  - `npx callimachus-mcp --install-cal` downloads the release's `cal` CLI (checked against the same `SHA256SUMS` as the MCP server) and links it at `~/.local/bin/cal`. It never replaces another program's `cal` and leaves the app's own link alone. Each time the MCP server starts, a `cal` linked this way moves to the server's release (never back to an older one), and the cache never prunes the version it runs from.
  - The plugin's new SessionStart hook, `cal index --if-stale`, starts a background `cal index` when no full pass finished in the last hour and neither the app nor `cal watch` is running. It does nothing with a `cal` that has no `cal index`, and it upgrades the index schema only with the launcher's `cal` while no app runs. A start it declines because of the schema adds a line to `index.db.index.log`, and typed in a terminal it says in one line what it did.
  - `cal index` now refuses to start while another `cal index` runs on the same index (`index.db.index.lock`).
  - Shrink index and Reclaim space wait while a background `cal index` runs.
  - The npm launcher finishes or cleans up a `cal` download when it exits, removes partial downloads older than an hour, and recognizes its own `cal` link through relative and symlinked paths.
  - When the index needs a schema upgrade, the MCP server and `cal` say to open Callimachus, or on a machine without the app run `cal index`.
  - The source-build instructions in the npm launcher no longer ask for WebKit packages or an empty `dist` folder: the standalone binaries build from the Tauri-free crate.
  - The plugin README, the main README and the CLI and MCP pages on callimachus.app describe the app-free path and what it gives up (the Projects page, Chat, Settings and the harvest controls).
