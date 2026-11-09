# callimachus-vscode

## 0.12.1

### Patch Changes

- 42aec5a: The first five minutes: a report that shows what your agents have done, provenance you can see while coding, and memory for Copilot and Cursor.
  
  - **"Your agents, so far".** One screen over the whole index: sessions per agent and estimated hours, spend by agent and model (all time and the last 30 days) with the priciest commits, the most frequent recurring errors and their fixes, tasks asked again on another day, project memory, the files agents changed most, and the value ledger. Open it from a card on Today after the first index, or from Stats; copy it as markdown or save a 1200 x 630 PNG card to Downloads. The setup checklist gains a "See your report" step. `cal report [--since DAYS] [--markdown] [--json]` prints the same report, with a versioned schema at callimachus.app/schemas/cal/report.v1.json.
  - **Provenance in the editor, and Agent Trace both ways.** The VS Code / Cursor extension marks the lines a past AI session wrote in the gutter and explains them on hover (the session, its agent and date, up to three of its decisions or gotchas, with Open session and Why this line), from one background `cal why` per file version; turn it off with `callimachus.whyGutter` and `callimachus.whyHover`. With `--json-version 1`, `cal why` now returns each commit's line ranges and its Agent Trace attributions (plain `--json` is unchanged); it can also blame an unsaved buffer (`--stdin`) and takes `-n`. Trace logs are read and written without following symlinks, and exported records name no path on your machine. When a repository carries Agent Trace records (agent-trace.dev) in `.agent-trace/`, `cal why` and the `threads_for_file` MCP tool show the agent, model and conversation another tool recorded for a line, even one Callimachus has no transcript of; `cal trace export` writes records for the commits your sessions produced (`--dry-run`, `--since`, `-o`), with a versioned schema at callimachus.app/schemas/cal/trace.v1.json.
  - **Memory for GitHub Copilot and Cursor.** Settings > Agents adds `cal hook --for copilot` to `~/.copilot/hooks/callimachus.json` (Copilot CLI and VS Code's Copilot sessions) and `cal hook --for cursor` to `~/.cursor/hooks.json` (onboarding offers Cursor's too), so both get the repo's memory when a session starts, like Codex and Gemini CLI, with SessionEnd hooks that queue the ended session for auto-distill (Copilot CLI sessions; VS Code Copilot sessions are not indexed from Copilot's session store yet). The first Cursor chat after a restart, or a first prompt sent right away, can miss the memory, since Cursor does not wait for the hook. Cursor also runs the Claude Code hooks it imports; those now stay quiet for a Cursor chat, so the memory arrives once. The privacy policy and SECURITY.md name the new hook connections.

## 0.12.0

### Minor Changes

- 2a5ca18: Memory that travels: facts keep their identity, a project's memory can live in its repo, and the index answers "why is this line like this", "what did that cost" and "how did I fix this last time".
  
  - **Facts keep their identity.** Every remembered fact has a stable id that survives a reindex and keeps its link to the message it came from. The index records which files each session read, edited or wrote, from the agents' own tool calls (Claude Code, Codex, Gemini CLI, Qwen Code, Cline, Cursor, Copilot CLI); sessions from before the update are filled in once, in the background, with no reindex needed. Commits are linked to the sessions that produced them as those sessions are indexed, and recurring errors are kept in a table refreshed in the background (by the app, or by `cal index`) after new sessions land. `cal issues`, and the `recurring_issues` MCP tool when asked about every project, answer from that table while it is current (refreshed in the last 10 minutes, or up to a day when nothing new was indexed since) instead of re-mining your history; a call scoped to one project, as the MCP tool is by default, still reads that project's sessions directly.
  - **Repo memory.** Publish to repo (a project's Memory section, under More) and `cal memory publish` write the repo's pinned facts and the ones you or an agent recorded to `.callimachus/memory/decisions.jsonl` and `gotchas.jsonl`, one fact per line sorted by id so branches merge cleanly; `--all` (or the switch in the preview) adds the distilled ones. In the app, a preview shows every fact added, updated or removed before anything is written; `cal memory publish` writes the files and then lists the same changes. Text is redacted, Claude Code's auto memory is never included, lines a teammate added are kept, and nothing is committed or pushed for you. Everyone with Callimachus who works in the repo gets those facts when a session there is indexed (by the app or `cal index`), or right away with `cal memory import` (`--review` lists the changes without writing them). Imported facts are marked "From the repo", are never pinned, and appear in the session-start hook and the AGENTS.md block as `(repo)` under a line naming the files. Your edit of an imported fact wins over the file, hiding one is local to you (publish never removes a teammate's line), a fact removed from the files is hidden on the next import (once no other clone of the repo on this machine still has it), and facts you published show as "Shared".
  - **Why this line.** `cal why <file>:<line>` runs `git blame` on the line, finds the sessions linked to the commit behind it, puts the ones that edited the file first and prints their decisions and gotchas. Leave off the line to see the commits behind the whole file (up to 10, preferring the ones a session produced). When no session is linked to the commit yet, it says so and points to `cal commits`. The MCP tool `threads_for_file` takes an optional `line` and then also carries the commit, its sessions and their decisions, so an agent can check the reasoning before changing the code. VS Code and Cursor get **Callimachus: Why This Line** (also in the editor's right-click menu); it needs a Callimachus app that has `cal why`.
  - **Cost to outcome.** Spend now covers Codex, Gemini CLI, Qwen Code and OpenCode as well as Claude Code and Copilot: their token usage is read at index time, and sessions indexed before the update get theirs filled in on their own, once, in the background (by the app, or by `cal index`), with no reindex. `cal cost --by thread|commit|project|day [--since DAYS]` breaks the estimate down per session (a Claude Code session's subagents folded in, with short notes on where the spend went: cache reads, turns past 50, subagents, one tool failing several times in a row), per commit those sessions produced, per project or per local day. The Stats spend card adds the last 30 days and the priciest commits, and a Project page shows its priciest commits once commits are linked. Prices come from a table shipped with the app, with OpenAI and Gemini models next to Anthropic's; figures are labeled as list-price equivalents (a subscription bills differently), and models without a published flat list price (Qwen's, open-weight ones, a release the providers have not priced yet) are listed as unpriced with the reason instead of being guessed. Gemini CLI sessions indexed or changed after the update no longer show a reply twice when its token count arrived after the text.
  - **Failure playbooks.** When a session that hit a recurring error then edited the file the error named, and a commit linked to that session changed that file, that commit is recorded as the fix in that repo. The Project page shows "Last fixed in `<sha>`" under the error with a link to the session, `cal issues` prints the fix under each error, and the fix becomes a playbook gotcha in the repo's memory ("`<error>`: seen N times; fixed in `<sha>`"), refreshed in place as the count or the fix changes and never pinned; edit or delete it like any other fact. Generic errors (a bare `error:`, a Python traceback header, a path that was not there) get none. New opt-in Claude Code hook, Settings, Agents, **Error playbooks** (off by default): when a shell command Claude runs fails with an error a commit in the same repo fixed before, Claude is told that fix, once per error per session; output without an error line costs a few milliseconds. The hook ships for Claude Code first; Codex and Gemini CLI do not get it yet. Its per-session note of what it already said is deleted when the session ends.
  - **Versioned JSON.** `cal why`, `cal cost` and `cal issues` print their versioned shapes under `--json-version 1` (schemas at callimachus.app/schemas/cal/why.v1.json, cost.v1.json and issues.v1.json); plain `--json` keeps its old shape, and the `recurring_issues` MCP tool returns the sessions behind each error, the fix and the playbook.
  - Site: the privacy policy names repo memory and the error-playbook hook, and SECURITY.md says the repo memory files are written only by your own publish action and leave your machine only if you commit and push them.
- 730cd97: The platform layer, second half: contracts other tools can depend on, a complete archive as vendors change formats, and the Claude Code plugin without the desktop app.
  
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

## 0.11.3

No changes in this release.

## 0.11.2

No changes in this release.

## 0.11.1

No changes in this release.

## 0.11.0

## 0.10.0

## 0.9.1

## 0.9.0

## 0.8.2

## 0.8.1

## 0.8.0

## 0.7.0

## 0.6.2

## 0.6.1

## 0.6.0

## 0.5.0

### Minor Changes

- Add **Ambient Recall**: the sidebar surfaces past threads related to your selection, the symbol under the cursor, or the nearest error (via `cal related`) with no searching. Toggle it with the **Callimachus: Toggle Ambient Recall** command or the `callimachus.ambientRecall` setting, and tune it with `ambientRecallThrottle`, `ambientRecallMinContext`, and `ambientRecallLimit`.

## 0.4.3

### Patch Changes

- e4be669: Ship standalone `cal` and `callimachus-mcp` binaries on every release for CLI/MCP-only users, and make the bundled `cal` resolve on Windows — the desktop app now places `cal.exe` in its install directory, where the VS Code / Cursor extension already looks, so the extension works on Windows without a manual PATH edit.

  Harden the extension's webview RPC: unknown methods now raise an error instead of silently returning nothing, `cal --json` output is parsed defensively (a clear message instead of a raw `SyntaxError`), and transcript attribute matching escapes its pattern.

## 0.4.2

### Patch Changes

- 118eb13: Make the VS Code / Cursor extension work without manual setup, and fail gracefully when it can't.

  The extension is a thin client over the `cal` CLI, so without it nothing worked — and `cal` wasn't installed by anything. Now:

  - **The desktop app installs `cal`.** The one-click "Enable for Claude Code" action symlinks `~/.local/bin/cal` to the app, which runs in `cal` mode when invoked by that name (same dual-mode trick as `--mcp`). No separate binary to ship, no cargo.
  - **The extension auto-discovers `cal`** in the app's known install locations (`~/.local/bin`, `/Applications/Callimachus.app/...`, Homebrew, Windows install dirs) before falling back to PATH — zero-config for app users.
  - **Friendly empty state.** If `cal` is missing or the index hasn't been built, the extension shows a "Download Callimachus" prompt instead of a raw error, and points to the download page.

## 0.4.1

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
