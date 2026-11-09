# callimachus

## 0.12.2

### Patch Changes

- a646b66: Memory that knows when a decision was reversed.
  
  - **Dated, sourced memory lines.** Every line the SessionStart hook, the compaction refresher, the MCP `project_memory` tool and `.callimachus/memory.md` inject now says when its session happened (not when it was distilled) and where it came from, e.g. `(2026-10-03, codex, thread 1234; replaces 2026-10-01)`; the committed `AGENTS.md` / `CLAUDE.md` block shows the date only. Memory is ordered by session time, so a backlog distilled newest first no longer ranks an old decision above its replacement.
  - **Replaced decisions are history, not memory.** A decision a later session reversed leaves injected memory and stays as history. `check_decision`, `cal check` and the recall tools return each decision with its date and status (current, disputed, flagged or superseded) and name what replaced a superseded one; `recall_decisions`, `recall_gotchas` and `cal memory` take `include_superseded` / `--include-superseded` to show them. Two decisions that disagree with nothing settled are marked disputed for you to settle.
  - **The memory pass notices reversals.** When memory is on, distilling a session now also shows the AI engine the project's current decisions and gotchas (the same engine and destination as the transcript), and records when the session replaces, disputes, reconfirms or retires one. It links only what a session states plainly, never by similarity, and it cannot replace a fact you pinned or edited: that pair is marked disputed instead.
  - **Agents can record a reversal.** `record_decision` / `record_gotcha` take `replaces` (a fact id), and so does `cal remember --replaces ID`. One aimed at a fact you pinned or edited is stored as disputed instead, and so is an agent's sixth replacement in a project within a day, so a misled session cannot empty a project's memory. Over MCP, a `replaces` aimed at another repo's fact records nothing.
  - **New MCP tool `flag_fact`** (23 tools now) marks a fact stale or wrong, with a reason, which keeps it out of memory until you review it. The agent gets the fact's id from `check_decision` or the recall tools; `cal check`, `cal decisions` and `cal gotchas` now print it too. Flags are capped at 10 unreviewed per project, reach only the repo the agent's session runs in (whatever project it names), and a fact you pinned or edited stays in memory, marked flagged.
  - `cal why`, the `threads_for_file` tool and the editor's Why This Line hover label a decision that was replaced or flagged, and name what replaced it.
  - `cal memory publish` holds back facts that are flagged or whose replacement is not being published, so a teammate never imports them as current, and says so; the app's publish preview lists them.
  - **Projects page.** Each fact shows its session date and agent, and whether it replaces, disputes or was confirmed by other sessions. New Replaced and Flagged groups let you undo a replacement, keep a flagged fact or hide it, and an undo you make is remembered by later memory passes. Review conflicts shows both dates and offers Newer replaces older and Both stand.
  - **After a context compaction** the SessionStart hook also re-sends your own prompts from that session (the first ones and the latest, about 2,000 characters), since a compaction summary loses the original request and its constraints. Tool output, system reminders and hook text are never included.
  - **Distilled facts must come from the session.** Before a decision or gotcha reaches memory, every file path, identifier, error code or quoted literal it names is checked against the whole session (and the memory the engine was shown); a fact naming something the session never mentioned is dropped. No AI call is involved.
  - **Stronger redaction.** Secrets hidden with zero-width characters or JSON escapes, keys at the start of a line in JSON-encoded tool output, passwords inside URLs (`postgres://user:pass@host`, even with an `@` or `#` in the password), Google API keys and cut-off private keys are now redacted. Memory blocks, hook output, packed transcripts, MCP results (text and structured) and the human output of `cal search`, `cal memory` and the other query commands also lose terminal escape sequences, bidi overrides and other invisible control characters before an agent or a terminal sees them.
  - `cal memory publish` and the app's publish preview say why each held-back fact is held (flagged, or replaced by a fact this publish leaves out).
  - **Fences hold.** Text inside a transcript can no longer close the `<reference_thread>` wrapper (or the other wrappers) around packed sessions, snapshots, Open-in-agent seeds and injected memory.
  - The first launch after updating adds two small tables to the index (fact links and fact signals); as with every upgrade, keep the app, `cal` and `callimachus-mcp` on the same release.
  - The callimachus.app MCP page lists the new tool and describes flagging.

## 0.12.1

### Patch Changes

- 7473a84: A Cursor plugin (`plugins/cursor`, listed by `.cursor-plugin/marketplace.json` at the repo root so Cursor's marketplace import and cursor.directory find it): the `recall` skill, the MCP server (`npx -y callimachus-mcp`, pinned to the release) and `sessionStart` / `sessionEnd` hooks that run `cal hook --for cursor` and `cal session-end --for cursor` through `~/.local/bin/cal`, staying silent when `cal` is missing or too old for Cursor's protocol. With both the plugin and the app's own Cursor hooks (Settings > Agents), `cal hook --for cursor` now injects the memory once per conversation: the first hook claims it with a marker file under `~/.callimachus/recall`, which `cal session-end --for cursor` removes and a week-old one is pruned.
- a97e1b5: The Claude Code plugin's hooks are now short shell scripts inside the plugin (`scripts/hook.sh`, `index.sh`, `snapshot-hook.sh`, `session-end.sh`) instead of inline commands. Anthropic's plugin directory allows no variable other than the plugin root in a hook command, and the scripts are easier to read. What they run is unchanged: the `cal` CLI at `~/.local/bin/cal`, quietly skipped when it is not installed.
- 60bef36: The Claude Code plugin now ships the MCP launcher inside the plugin (`mcp/`, a copy of the `callimachus-mcp` npm package) and runs it with `node` instead of `npx`. It downloads the same release binary and checks it against the release's `SHA256SUMS` before running it, with no npm registry lookup at session start. The plugin also gains a listing icon.
- 42aec5a: The first five minutes: a report that shows what your agents have done, provenance you can see while coding, and memory for Copilot and Cursor.
  
  - **"Your agents, so far".** One screen over the whole index: sessions per agent and estimated hours, spend by agent and model (all time and the last 30 days) with the priciest commits, the most frequent recurring errors and their fixes, tasks asked again on another day, project memory, the files agents changed most, and the value ledger. Open it from a card on Today after the first index, or from Stats; copy it as markdown or save a 1200 x 630 PNG card to Downloads. The setup checklist gains a "See your report" step. `cal report [--since DAYS] [--markdown] [--json]` prints the same report, with a versioned schema at callimachus.app/schemas/cal/report.v1.json.
  - **Provenance in the editor, and Agent Trace both ways.** The VS Code / Cursor extension marks the lines a past AI session wrote in the gutter and explains them on hover (the session, its agent and date, up to three of its decisions or gotchas, with Open session and Why this line), from one background `cal why` per file version; turn it off with `callimachus.whyGutter` and `callimachus.whyHover`. With `--json-version 1`, `cal why` now returns each commit's line ranges and its Agent Trace attributions (plain `--json` is unchanged); it can also blame an unsaved buffer (`--stdin`) and takes `-n`. Trace logs are read and written without following symlinks, and exported records name no path on your machine. When a repository carries Agent Trace records (agent-trace.dev) in `.agent-trace/`, `cal why` and the `threads_for_file` MCP tool show the agent, model and conversation another tool recorded for a line, even one Callimachus has no transcript of; `cal trace export` writes records for the commits your sessions produced (`--dry-run`, `--since`, `-o`), with a versioned schema at callimachus.app/schemas/cal/trace.v1.json.
  - **Memory for GitHub Copilot and Cursor.** Settings > Agents adds `cal hook --for copilot` to `~/.copilot/hooks/callimachus.json` (Copilot CLI and VS Code's Copilot sessions) and `cal hook --for cursor` to `~/.cursor/hooks.json` (onboarding offers Cursor's too), so both get the repo's memory when a session starts, like Codex and Gemini CLI, with SessionEnd hooks that queue the ended session for auto-distill (Copilot CLI sessions; VS Code Copilot sessions are not indexed from Copilot's session store yet). The first Cursor chat after a restart, or a first prompt sent right away, can miss the memory, since Cursor does not wait for the hook. Cursor also runs the Claude Code hooks it imports; those now stay quiet for a Cursor chat, so the memory arrives once. The privacy policy and SECURITY.md name the new hook connections.

## 0.12.0

### Minor Changes

- e93ab7c: The app around the Project page: fewer tabs, one home per repo, search that scopes to it, and a daily reason to open the app.
  
  - A Today tab opens the app: what happened since you last opened it, grouped by repo, with each session's duration and the commits it produced (click a commit to copy its sha), new TODOs, errors seen again and snapshots captured, a 24 hours or 7 days switch, and Copy as standup. It is built from the index alone and never calls an LLM. The tabs are now Today, Search, Chat and Projects; Stats and Settings stay icons, and the activity heatmap moved from Coach to Stats, where a day opens Search for that day.
  - Ask is a mode of Chat: pick Ask in the Chat header to answer from your history with `[thread N]` citations that open the thread, then keep going as a normal chat. Ask answers are saved with your chats. The Coach, Knowledge and Ask tabs are gone.
  - Each repo has a Project page: recent sessions with Resume and Open in CLI, memory (decisions, gotchas and TODOs with every edit action), snapshots (read one, resume it in an agent CLI, or delete it), the commits sessions produced, recurring errors, spend for the last 30 days and all time, and "What agents see": the exact SessionStart hook text and AGENTS.md block a new session in that repo gets, with their sizes against the 10,000-character cap. All projects lists every repo and the open TODOs across them. The page opens on the repo you last looked at, and threads gain "Open project".
  - Search scopes: a repo chip (it starts on your last project, one click clears it), 24h, 7d and 30d presets, semantic search on by default once 90 percent of messages are embedded (the switch still turns it off), and a "Decisions and gotchas" group above the results that opens a fact's thread at the message it came from. An empty scoped search offers "Search all repos" and "Search all time".
  - Settings > AI engine replaces Providers and Knowledge: the memory switch and engine, auto-distill, Chat's provider and model, and the API keys and server URLs in one place. The Chat header now shows only the provider and model, with a link there when no key is saved.
  - The toolbar Reindex button is gone: the watcher indexes new sessions as they land, so a full reindex is now Reindex history in the command palette, Settings > Sources still reindexes one source, and the empty index screen keeps its Index my history button.
  - callimachus.app: the copy now matches the app (the Today, Search, Chat and Projects tabs, search filters, the setup checklist, memory that starts with the recent-sessions digest, the plugin as the first way to connect Claude Code) and has no em-dashes left.
- dd18928: Every agent, one memory: the memory Callimachus fills now reaches Codex and Gemini CLI through their own hooks, Callimachus installs into Claude Code in one step, the first run walks you to a connected and remembering setup, and Stats shows what it did for your agents.
  
  - Codex and Gemini CLI get each repo's memory too: Settings > Agents > Other agents has a Memory hooks switch per detected agent that installs `cal hook --for codex|gemini` on session start (with imported Claude Code memories, marked as such), queues an ended session for auto-distillation, and snapshots a Codex session before compaction. The `/recall` skill is also copied to the shared `~/.agents/skills/` folder.
  - Install Callimachus in Claude Code in one step: a plugin brings the `/recall` skill, the MCP server and the memory hooks (`claude plugin marketplace add BetaBots-LLC/callimachus`, then `/plugin install callimachus@callimachus`). While the plugin is enabled, Settings > Agents installs only the `cal` CLI its hooks run and removes the app's own skill, MCP entry and hooks, so memory is never injected twice and only one MCP server starts; Settings says the integration is managed by the plugin. The marketplace serves the plugin from the release tag, with its MCP server pinned to the same release, so it matches the app you installed (0.11.4 or later). On Windows, keep using Settings > Agents: the plugin's hooks need Git Bash there, and the app keeps its own install.
  - The MCP server no longer upgrades the index itself. When its version and the index disagree, every tool says what to do, naming the version: update the Callimachus app and open it once (the index needs an upgrade), or update `callimachus-mcp` and restart your agent (the index is newer than the server). These notices never count in Stats. It picks up again as soon as the app has upgraded the index.
  - A first-run setup checklist replaces the one-button welcome screen: index your history, connect your agents, turn on memory with the one-click engine choice, and build semantic search in the background. Each step says what it unlocks, every step after indexing can be skipped, and the checklist reopens from the command palette ("Setup checklist") or Settings > General. The anonymous-usage question moves into the checklist's footer instead of covering it.
  - Stats shows what Callimachus did for your agents this week (sessions started with injected memory or a recent-sessions digest, recall hits, MCP tool calls), counted in a local `ledger.jsonl` (the dev build's index gets its own) that never leaves the machine and is folded into the index once each day is over, safely across a crash; `cal stats` prints the same line. Stats also counts the indexed Claude Code sessions whose transcripts Claude Code has deleted (`cleanupPeriodDays`, 30 days by default) and offers a reversible "Keep transcripts for a year".
  - callimachus.app: the MCP and desktop pages mention the plugin, the FAQ mentions the checklist.
- 4430037: `cal` can now build and refresh the index without the desktop app. Four new `cal` commands run the app's background jobs from a terminal: `cal index` builds or refreshes the index from every agent's history (exit code 1 when a session could not be indexed), `cal watch` keeps it fresh as agents write (one line per batch), `cal embed` builds the semantic index (it downloads the embedding model once; `cal embed --no-download` uses only a model already there), and `cal distill --auto` distills recent sessions under the daily cap. `cal distill --auto`, like the drain `cal watch` runs, uses the memory engine turned on in the app's Settings > AI engine: once memory is on it runs with the app closed (a logged-in `claude` or `codex` CLI and local Ollama need nothing else, and an API key comes from the OS keychain where the app saved it), and on a machine that never ran the app it says memory is off. Only one process watches or distills an index: `cal watch` refuses to start while the app is open, and the app leaves live indexing to a running `cal watch` (Settings > Index says so) and takes over within a minute after it stops, and it starts no auto-distill round while `cal watch` or `cal distill --auto` runs. A second app open on the same index now waits the same way instead of running a second watcher. `cal distill --auto` refuses while the app or `cal watch` runs, since both distill on their own. The standalone `cal` and `callimachus-mcp` binaries and the Docker image now build without GTK or WebKit (the released binaries never loaded them at run time).
- 84536fb: Memory that fills itself: every memory surface now says something useful on day one, and the knowledge layer fills from what your agents already write.
  
  - The Claude Code SessionStart hook helps on day one: a repo with no distilled memory gets a short digest of its recent sessions (agent, branch, age, the latest summary or prompt, touched files, open TODOs and the latest snapshot), with no LLM. `cal hook --explain` shows what a new session would get, `cal hook --mute` turns the digest off for one repo, and Settings > Agents turns it off everywhere. The hook no longer injects memory twice when Claude Code already reads the managed block from `AGENTS.md` (only when no `CLAUDE.md` exists) or `CLAUDE.local.md`.
  - The MCP server stays useful before memory is distilled: recall and `check_decision` point to the closest past sessions with a coverage count, `project_memory` adds the recent-sessions digest, and a zero-hit search says what the index covers and what it cannot see (cloud sessions, Cursor project paths). It also offers `recall`, `check_decision` and `prior_work` prompts (slash commands in Claude Code) and project memory as the `callimachus://memory` resource. Facts carry their origin (`extractor`, `originRef`).
  - Turning on memory is one consent click that names the engine and where transcripts go: the card preselects the saved engine, else a detected Claude Code or Codex CLI (no API key), then local Ollama (with a model the server has pulled), then a saved key, and shows how many sessions qualify and the daily cap. Existing settings are never switched. Heuristic TODOs and Remember no longer need the Knowledge switch, which now means only "LLM distillation"; the Projects tab and the Knowledge tab's TODOs work without it.
  - Project memory fills from what your agents already write: Claude Code's auto memory joins each repo's memory without an LLM (feedback as gotchas, project notes as decisions, shown with a badge; turn it off in Settings to remove them), the PreCompact hook keeps Claude Code's compaction summary as the session's summary, and a new SessionEnd hook moves a finished session to the front of the auto-distill queue.
  - The hooks that write (PreCompact and SessionEnd) no longer upgrade the index themselves: after an update they stay quiet until the app or a `cal` command you run has applied the upgrade, so a newer hook can never lock an older app out of its index.
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

### Patch Changes

- 10f2165: Faster, lighter, and better search results, measured on a 5 GB index of 1.4 million messages.
  
  - **Memory:** the embedding model unloads after it has been idle (10 minutes in the app, 1 minute in the MCP server), and bulk builds use their own model instance that is dropped when the build ends. The app no longer sits at 1.5 GB after a day, and an MCP server no longer keeps 400 MB after one semantic query.
  - **Semantic search:** results are filtered by a calibrated similarity floor, so a query no longer pads out with unrelated hits; Starred, tag and date filters now apply to semantic results too; the keyword and semantic passes run in parallel and the semantic pass is capped, which takes about 200 ms off each hybrid query. Turning the semantic switch on warms the model so the first query is not keyword-only.
  - **Keyword search:** quoted phrases are honored as phrases (about 14x faster and precise), tool output ranks below prose, identical snippets collapse, recent activity gets a nudge, and a page no longer comes back short when many threads match. A new search cancels the previous one.
  - **Threads:** opening a long thread sends tool output as a preview and fetches the full text only when a row is expanded (the largest thread went from a 6 MB payload to a fraction of it); switching tabs keeps the Search view, its scroll positions and the open thread; expanded rows stay expanded when scrolled away and back; scroll height no longer drifts.
  - **Background work:** new messages are embedded automatically about a minute after agents go quiet, so semantic search, Ask and recall stay current without pressing Build; a message appended to a distilled thread no longer wipes its facts; the launch check for new sessions no longer flashes "Indexing…" or disables Reindex; live refresh pauses while the window is hidden and never re-runs a typed search behind your back; every background job logs its duration.
  - **Reindex toasts** report the messages that were actually new.
  - **Storage:** large tool calls from Claude Code (a Write of a whole file, for example) are now stored like tool output, head and tail with the middle elided, which keeps the index smaller; Shrink index applies the same cap to calls stored by older versions.
- fcbafcc: - Smaller downloads and updates on macOS and Linux (about 23 MB less): the app no longer bundles two unused copies of its backend as separate `cal` and `callimachus-mcp` binaries (the app itself already serves both). The deb and rpm packages also stop installing `/usr/bin/cal` and `/usr/bin/callimachus-mcp` (that `cal` collided with the system calendar). If you used them, **Settings → Claude Code → Enable for Claude Code** links the app as `~/.local/bin/cal` (or download the standalone `cal-<target>` from Releases), and an MCP client can run `/usr/bin/callimachus --mcp`; the app re-points a Callimachus MCP entry left on the removed `callimachus-mcp` by itself, and the VS Code extension finds `/usr/bin/callimachus` on its own. Windows installs still ship `cal.exe` and `callimachus-mcp.exe`, built with the VC runtime linked in as before.
  - Reclaim disk space now shows how much it would free ("Reclaimable: 248 KB") and is only available once at least 50 MB and 1% of the index are free, instead of rewriting a multi-GB index to give back a few kilobytes. When it runs, the toast reports the space actually freed, or says it will be returned shortly when another read held the file back. Shrink index follows the same rule and says when the space it saved stays inside the index until enough is free.
  - Tool output stored by older versions is now trimmed in the background a little after launch, a few thousand rows at a time, for threads whose transcript is still on disk (a re-index would store the same trimmed text). Threads whose transcript the agent has since deleted keep their full text, since the index holds the only copy; Shrink index still trims those when you choose to. The trim runs once per index and never alongside Shrink index or Reclaim space.
  - No more splash flash on launch: the splash screen only appears when startup takes longer than about 0.4 s or the index is being upgraded, and then stays up for at least 0.6 s, so a normal launch opens one window, once.
  - Long threads now fill their context budget when packed for Add to chat, MCP `get_thread`, distill, snapshots and `cal context`: tool calls and shortened tool output are kept while room remains instead of all being dropped at once, and elision keeps as many turns as fit instead of at most 12 (40 large threads went from 19 KB to 48 KB of a 48 KB budget on average).
- 44a8ab3: Search results and threads are easier to scan and to move through.
  
  - **Results:** one row per thread, two lines: the title with its match count and time, then where the match came from ("You:", "Assistant:", "Bash output:") and the project. Arrow keys move through the list from the search box, Enter opens, Esc returns to the box. No-match states name the active filters and offer "Clear filters" and "Try semantic search"; an empty index offers Reindex inline.
  - **Threads:** runs of tool calls fold into one line ("12 tool calls · Bash 5 · Read 4 · Edit 3") that expands in place, with results attached to their calls; a Match stepper walks the search hits (Enter / Shift+Enter, Cmd+G) and a Prompts stepper walks your messages ([ and ]); the header is two lines (title and actions, then source, project, branch, time and inline tags), and the commits row only appears when there are commits.
  - **Elsewhere:** Coach's recurring errors open a search for that error; the Projects toolbar keeps Build memory and Remember and moves the rest under More, with facts as a plain list; Stats opens with one summary row; the command palette gains "Search for …" and recent threads and uses the same labels as the tabs; Chat has one New chat button and focuses its box on open; drawn icons replace the few text glyphs.

## 0.11.2

### Patch Changes

- da3c838: Settings redesigned, with notifications and an in-app update flow.
  
  - **Settings:** a section list on the left (General, Providers, Agents, Sources, Index & storage, Knowledge, Export, Updates) and the open section on the right, instead of one long page of cards. Settings are laid out as labeled rows; every control has a real label, secrets have a show/hide toggle, paths read in monospace, and the native dropdowns are gone. "Enable in Settings" and the command palette open the right section directly.
  - **Notifications:** toasts (bottom right) report outcomes you can't see in place: a key saved or removed, a folder changed, an integration installed, a reindex or semantic build finishing (with counts), a cleanup or reclaim result, and update progress.
  - **Updates:** Settings › Updates shows the running version, checks on demand, shows the release notes, and installs with a progress bar. Background checks still run on launch and every few hours; a found release now arrives as a toast with an Install button, and the first launch after an update says what changed. The command palette gained "Check for updates".
  - **Fixed:** the "Delete old threads" table in Settings stayed empty after loading (the list is seeded into a form, and the form library's array field no longer re-renders on a reset), and its age filter showed the raw value instead of its label.
  - **Development:** `pnpm desktop:dev` now runs the dev app against its own `index.dev.db` instead of the installed app's index. Opening the real index with a dev build upgrades it in place, after which the installed app can no longer read it; that is what made 0.11.0 crash at launch. A browser mock (`mock.html`) lets the UI run without the Rust backend for screenshots and UI work.

## 0.11.1

### Patch Changes

- 586e7c3: Dependency sweep: every direct dependency is current and the security audits are clean (0 Rust advisories, 0 production npm advisories).
  
  - **MCP server:** moved to rmcp 3.8. The initialize handshake and the 22-tool surface are unchanged (a test pins the negotiated protocol versions), so Claude Code, Codex and Cursor connect as before.
  - **Keychain:** keyring 4. On Linux the Secret Service backend changed to the zbus-based store; macOS Keychain and Windows Credential Manager behave the same.
  - **Release tooling:** Changesets 3 (private packages stay versioned), changesets/action 2, and the actions/checkout, setup-node and pnpm/action-setup majors, all pinned to commit SHAs. tauri-action stays on 0.6.2 on purpose, so updater downloads keep using browser URLs instead of rate-limited API asset URLs.
  - Held back: fastembed 7 (its ONNX runtime build needs compiler-rt linking proven on CI and raises the macOS minimum), rusqlite 0.41 (rusqlite_migration does not support it yet), and prerelease-only crates.
- 586e7c3: Update the MCP server to rmcp 3.8 (same tools and handshake; 2024-11-05 through 2025-11-25 protocol versions) and the OS credential store to keyring 4 (existing saved API keys keep working; on Linux it reaches the Secret Service through pure-Rust zbus instead of libdbus).
- cd492ac: Production hardening across the desktop app, MCP server, CLI and release pipeline.
  
  - **Leaner memory injection:** the SessionStart hook, the managed `AGENTS.md` / `CLAUDE.md` block and the `project_memory` MCP tool now render a size-budgeted summary (gotchas before decisions, pinned facts first, long facts trimmed), skip resumed sessions, and label the notes as context from past sessions rather than instructions.
  - **Hooks:** the SessionStart hook runs only on startup, clear and compact, every hook has a timeout, the SubagentStop snapshot hook is removed (PreCompact snapshots stay), and hook commands exit quietly if the app has been moved or removed.
  - **Embeddings:** the embedding model is cached next to the index instead of in whatever folder the app was started from, hooks and the MCP server never download it, and tool-call output no longer inflates the semantic-index backlog.
  - **MCP server:** compact JSON results, smaller default limits, an optional `budget` for `get_thread` and `load_snapshot` (default 24k chars), shorter tool descriptions, tool annotations, a slimmer `tools/list`, and a clear message instead of empty results when there is no index yet (or when the semantic model isn't downloaded). Two output shapes changed: search results now come back as one entry per thread with its `snippets`, and `project_memory` returns budgeted markdown by default (`format: "json"` for data, plus `limit` and `kinds`).
  - **Integrations:** config files are written atomically (symlinked dotfiles stay links, unreadable or non-UTF-8 files are never overwritten), `CLAUDE_CONFIG_DIR` / `CODEX_HOME` and Settings overrides for relocated Claude / Codex data are honored, the app refuses to register itself from a disk image, App Translocation or an AppImage mount, installed hooks and MCP entries are brought up to date at launch, `cal` no longer replaces another program's `~/.local/bin/cal`, and a per-machine Windows install no longer fails when `cal.exe` can't be created. The Windows uninstaller now removes the integrations.
  - **Settings and Project Memory:** Settings → Sources has fields for relocated Claude Code and Codex data folders (changing one reindexes that source, after any index job already running); the auto-distill daily limit can be set next to its toggle (0 pauses it) and shows what is left today; Storage cleanup has a **Shrink index** button that trims tool output stored by older versions and returns the space to disk; and Project Memory has a **Remember** action for recording a decision or gotcha yourself (saved pinned).
  - **Responsive app:** searching, Stats, Coach, cleanup, reindexing a single source and the other database, git, keychain and embedding work now run off the UI thread, so the window no longer freezes ("Not Responding") during them. Startup opens and upgrades the index in the background, so the splash keeps animating during a long one-time upgrade, and if the index can't be opened (made by a newer version, or damaged) the app shows what happened and how to recover instead of quitting silently. Diagnostics go to `callimachus.log` next to the index.
  - **Performance:** keyword search no longer turns short and common words ("how", "do", "the") into prefix searches, and falls back to a looser any-word search only when a page would otherwise come up short, so natural-language searches that took several seconds on a large index now return in well under a second; a huge pasted message (over 256 KB) no longer adds seconds to every search it matches (its result shows the opening text instead of a highlighted snippet). New covering indexes let search, recurring issues, the Coach heatmap and commit linking read a small fraction of the data they did, so on a large index with a cold cache the first search takes about a second instead of 10 to 15, and the first Coach open a fraction of a second instead of about 8. Building the semantic index is about twice as fast, and at the end of a build the vector store is compacted to pack the slots left by removed tool-call vectors, making semantic search about a third faster. That frees space inside the index for reuse but doesn't shrink the file (a 4.7 GB index grows about 230 MB once); only Shrink index or Reclaim space returns space to disk. At launch the app now indexes sessions that finished while it was closed, so search, the MCP server and hooks see them without a manual Reindex. The one-time upgrade from v0.11.0 runs on the first launch in a single transaction: it builds four message indexes, maps every existing vector to its message, and removes old tool-call vectors. It reads the index file sequentially first, so it takes about 17 seconds on a 4.7 GB index with a cold cache, and it needs roughly 0.5 GB of free disk while it runs. The app no longer loads the embedding model at every launch, only when it has embedding work queued or a semantic feature first needs it, which saves roughly 400 MB of memory in sessions that don't use one. Release builds no longer produce the unused static and shared library copies of the app, so they finish sooner. The window appears as soon as the app has loaded instead of after counting every message (0.6 to 0.8 s sooner on a cold launch), the markdown and syntax-highlighting code loads just after the first screen instead of at startup, a streaming reply no longer re-runs language detection on an unlabeled code block every frame, a closed thread is dropped from memory after a minute instead of five, and a multi-megabyte message renders only when expanded, in 200k-character pieces with a Copy full message button. The VS Code / Cursor sidebar loads about half as much code, since the markdown renderer now loads only in the thread panel.
  - **Semantic index repair:** when the embedding model can't be downloaded or loaded, building the semantic index now stops with an error and leaves every message queued, instead of marking the whole history done with no vectors. Indexes affected by the old behavior are re-queued once on upgrade, and tool-call vectors from older builds are removed.
  - **Fixes:** deleted threads stay deleted instead of returning on the next scan; Ask answers from the matching part of each thread instead of its opening; Open in CLI and `cal resume` inject project memory once instead of twice; `cal resume` runs the agent in the current terminal; the chat agent's shell tool works on Windows; a second chat message stops the first stream instead of making it unstoppable; and background auto-distill only covers threads active in the last 30 days, at most 50 per day by default.
  - **Opt-in usage analytics:** the desktop app now asks before sending anything. If you choose Allow, it sends anonymous usage events to PostHog (US): event names, app version, OS, CPU architecture and language, and a few properties picked from fixed lists (such as which agent source, AI provider or setting an event concerns, an error category, or a count rounded into a range), tied to a random install ID. It never sends file paths, thread titles, prompts, transcripts, project names or API keys. Turn it off any time in Settings → General, or with `CALLIMACHUS_TELEMETRY=0` / `DO_NOT_TRACK=1`. The CLI and MCP server never send analytics, and builds from source contain no analytics key.
  - **Privacy policy:** callimachus.app/privacy now lists every analytics event and property, the update check, the features that send conversation content to the AI provider you pick (chat, distillation, Ask, project briefs and conflict review, and AI-summarized Obsidian exports) and what agents you connect over MCP or hooks receive, and the website moved to cookieless PostHog analytics. Common secret formats (API keys, tokens, private keys) are now redacted, best-effort, everywhere conversation content goes to an AI provider or another agent: chat (including the history its tools look up), distillation, Ask, project briefs and conflict review, every MCP tool result, hooks and the managed memory block, Open in CLI context, Obsidian exports, and the output of `cal` query commands (human and `--json`; pass `--raw` to print your history as stored), titles and error signatures included. The index keeps the original text. It is a safety net, not a guarantee: a key in an unusual format can slip through.
  - **Dependencies** updated across the Rust and JavaScript workspaces.
  - **Releases and the npm launcher:** releases stay drafts until every platform has uploaded, so the updater never sees a half-finished release; the standalone macOS `cal` and `callimachus-mcp` binaries are now signed and notarized; each release ships a `SHA256SUMS` file, which `npx callimachus-mcp` verifies its download against. The launcher also stops with a clear message on Linux systems older than glibc 2.38 (instead of failing to load) and removes binaries cached for older versions once they have gone unused for 30 days.

## 0.11.0

### Minor Changes

- 8dcef8d: feat: add an optional "close to system tray" setting (Settings → General, off by
  default). When enabled, closing the main window hides Callimachus to the system tray
  instead of quitting; a tray icon with Show / Quit appears while the setting is on, and
  left-clicking it restores the window. The preference is persisted, and the tray icon is
  added or removed as you toggle it.

## 0.10.0

### Minor Changes

- f7bf2e9: feat: add an Ollama Cloud provider with custom server URL support. Adds
  Bearer-authenticated Ollama Cloud as a chat provider alongside a configurable custom
  server URL for self-hosted or remote Ollama endpoints, unifies provider base-URL
  handling, and ensures the Ollama Cloud API key is never forwarded to local or custom
  servers.

### Patch Changes

- f7bf2e9: fix: correct Anthropic model pricing in the spend estimate and improve currency
  formatting. Prices Claude Fable/Mythos usage at their $10/$50-per-Mtok list rates
  (previously unmatched and counted as untracked) and updates the stale Opus
  ($15/$75 → $5/$25) and Haiku ($0.80/$4 → $1/$5) rates. The Spend panel now formats
  amounts with thousands separators and shows "<$0.01" for tiny-but-nonzero estimates
  instead of collapsing them to "$0.00".

## 0.9.1

### Patch Changes

- 9914f5d: feat: index VS Code-native / GitHub Copilot chat and capture per-message models. A new
  `copilot` source reads `chatSessions` and `emptyWindowChatSessions` across VS Code-family
  editors (Code, Cursor, VSCodium, Windsurf, Insiders), extracting user/assistant turns, the
  project, timestamps, and the model that produced each assistant turn (e.g. `gpt-5.3-codex`).
  The per-message model is now surfaced in the thread view. Also fixes the file watcher's
  live-reindex routing on Windows (it matched forward-slash paths only), so auto-reindex now
  works on Windows for every source.
- 86ffcfc: fix(windows): resolve the index DB and per-agent data dirs with platform-correct
  per-user paths. The DB location and every agent indexer were hardcoded to the
  macOS `$HOME/Library/Application Support` layout. On Windows `$HOME` is normally
  unset, so the DB path collapsed to a relative path under the install dir (e.g.
  `C:\Program Files\Callimachus`), which a standard user cannot write: the app
  crashed unless run as administrator, and indexing never found any agent data.
  Now uses `dirs::data_local_dir` for the index and `dirs::home_dir` /
  `dirs::config_dir` for agent discovery. macOS paths are byte-identical (no
  migration); Windows and Linux now resolve correctly.

## 0.9.0

### Minor Changes

- ff7caff: feat: add OpenCode SQLite session DB support — reads V1 tables from `opencode.db`, combines with legacy JSON tree when both exist (SQLite wins on conflict)

### Patch Changes

- 89534de: fix: Authenticode-sign the standalone Windows `cal` and `callimachus-mcp` binaries via Azure Trusted Signing. They previously bypassed the Tauri bundler's signCommand and shipped unsigned, tripping SmartScreen's "Unknown publisher" warning. Gated on the Azure secret, so it's a no-op until Trusted Signing is configured.

## 0.8.2

### Patch Changes

- eb47be0: Stream the Claude Code CLI chat engine token-by-token. The keyless `claude` backend now runs with `--output-format stream-json --include-partial-messages` and forwards each text/thinking delta as it arrives, so replies stream in smoothly instead of popping in all at once when the response completes. (Codex CLI still emits its reply in one chunk.)

  Define the scrollbar utilities the shadcn message-scroller viewport relies on (`scrollbar-gutter-stable`, `scrollbar-thin`, `scrollbar-none`, and the `data-autoscrolling` variant), which aren't shipped in `shadcn/tailwind.css`. Reserving the scrollbar gutter stops the chat column from shifting horizontally as a reply streams in, and the scrollbar no longer flickers during autoscroll.

## 0.8.1

### Patch Changes

- cd6cf43: Rebuild the in-app chat UI on the new shadcn conversation primitives (Bubble, Message, Marker, Attachment, Message Scroller) instead of the bespoke layout. User turns render as muted bubbles, assistant turns as ghost-bubble markdown, injected context as a compact Attachment chip, and system notes as inline markers. The message list now uses Message Scroller for autoscroll / stick-to-bottom (replacing `use-stick-to-bottom`), with a scroll-to-latest button.

## 0.8.0

### Minor Changes

- 1a22fa3: Three small, high-leverage improvements across the chat agent, CLI, and MCP server:

  - **In-app chat now has a system prompt.** The chat agent is told its edge is your own indexed coding history and when to reach for each tool (`search_history` / `get_thread` / `run_shell`), with a `[thread N]` citation convention. Previously it had tools but no instructions, so it rarely searched your history — especially on weaker or local models.
  - **`cal audit-pr` resolves abbreviated commit SHAs.** Commit provenance now prefix-matches the short hashes that `git log --oneline` and PR tooling produce against the stored full SHAs, instead of silently returning empty provenance for them.
  - **New `recurring_issues` MCP tool.** Any connected agent can now ask which errors you keep hitting in a repo (scoped to the current project by default, `"*"` for all) before retrying a known-broken approach — the same analysis behind `cal issues`, now reachable over MCP.

## 0.7.0

### Minor Changes

- fe1902e: **`cal audit-pr`: a one-call PR-audit context bundle for external tools.** A local PR-audit app (or any reviewer) can now shell out to Callimachus and get, in one JSON object, the history behind a change that a diff alone can't show:

  ```
  cal audit-pr <repo> --changed-files src/auth.rs,src/lib.rs --shas <sha1>,<sha2>
  ```

  The bundle:

  - **`bySha`** , commit provenance: for each branch SHA, the AI session(s) inferred to have produced it (by file-overlap) with that thread's distilled summary/decisions/gotchas. "Commit abc came from the session where you decided JWT because X, then flagged an offline-refresh gotcha." SHAs with no link return an explicit empty array, never omitted.
  - **`byFile`** , for each changed file, prior threads that touched it + their reasoning.
  - **`recurringErrors`** , the repo's cross-tool recurring error signatures (the caller intersects with the touched-thread set client-side; there's no error→file edge).
  - **`projectMemory`** , the repo's distilled decisions / gotchas / open TODOs as a standing pre-merge checklist.

  It refreshes thread↔commit links first (so feed branch SHAs pre-squash), and degrades gracefully: with distillation off, knowledge fields are null but provenance, file-touch, open TODOs, and repo errors still populate. Reuses the existing gitlink / search / knowledge / issues primitives; the only net-new query is `commits_by_sha` (the inverse of `linked_commits`), backed by a new `thread_commits(sha)` index (migration 0023). The interactive deep-dive (per-hunk `check_decision`, `get_thread`, snapshots) stays on the existing MCP server.

### Patch Changes

- a6630f8: **Use your agent CLI instead of an API key.** Distillation, Ask, project memory, and the in-app chat can now run on your logged-in **Claude Code** or **Codex** CLI subscription, no raw API key required. Pick "Claude Code CLI (subscription · no key)" as the distillation engine, or select it in the chat provider dropdown, and Callimachus shells out to the CLI in non-interactive print mode (tools off, neutral cwd) to get the completion.

  - **Keyless, like Ollama:** CLI backends need no stored key. The engine/provider pickers offer whichever CLIs are installed, and the key field disappears when one is selected.
  - **PATH-aware:** a GUI app launched from Finder doesn't inherit your shell PATH (nvm/homebrew/npm dirs), so the CLI is resolved via your login shell, the same `claude` you use in the terminal.
  - **Honest privacy note:** unlike Ollama, CLI distillation still sends thread text to that CLI's provider (via your subscription); the UI says so. It's "no key", not "on-device".
  - **Knowledge completions** (distill, ask, project brief, conflict review) route through the CLI cleanly. **In-app chat** runs as a single completion per turn, the genai history-search tools and token streaming are keyed-provider only, so CLI chat is plain Q&A.

  Claude Code is verified end-to-end; Codex is wired to `codex exec` but untested locally. Refactors the five LLM call sites onto one `complete()` helper that branches CLI vs genai.

- a6630f8: **Fix the macOS keychain prompt that re-popped on every Deny.** The in-memory key cache only stored successful reads and "no entry" results, a denied / cancelled / locked-keychain read returned an error without caching, so the next `has_key` probe re-read the keychain and macOS re-prompted, on and on. Denials are now cached for the session (respecting your Deny), so it asks at most once; re-enter a key or restart to retry.

  Paired with the new CLI backends, keyless engines stay out of the keychain entirely: `provider_has_key` short-circuits for Ollama and the CLI providers without a keychain read, and the chat view skips the key probe when a keyless engine is selected. Net effect: pick a CLI (or Ollama) and Callimachus never touches your keychain.

- 0071bfe: **Proactive recall: surface prior work before you redo it.** A new opt-in **Proactive recall** toggle (Settings, under Claude Code) wires up a `UserPromptSubmit` hook that, on every prompt, quietly checks whether you've already solved this in a past session and injects a short "you may have worked on this before" note into Claude's context, so the decision or gotcha gets reused instead of rediscovered. One switch configures the hook for you, no editing `settings.json`.

  - **Opt-in, off by default:** it's a separate toggle from the base integration _because_ it reads every prompt. Flip it on to enable, off to remove the hook cleanly. Enabling also ensures the `cal` CLI is installed so the hook resolves.
  - **Silent + best-effort:** a fresh process per prompt that exits 0 with no output on a weak/no match, a missing index, or any error, so it never blocks or breaks the prompt.
  - **Signal, not noise:** a strict similarity floor (well above the on-demand guard) and per-session dedup (`~/.callimachus/recall/<session>.json`) mean it only speaks up on a clearly relevant match and never repeats the same thread twice in a session. Scoped to the repo via the hook's `cwd`.
  - **Reuses** the existing semantic `find_prior_work` over distilled decisions/gotchas, so it lights up once threads are distilled. All on-device.

  Note: each prompt loads the embedding model in a fresh process, a sub-second pre-flight before the agent starts.

- 592f362: **Publish the bundled MCP server to the official MCP registry.** Adds a
  `callimachus-mcp` npm package (`packages/callimachus-mcp`): an `npx`-friendly
  launcher that downloads the prebuilt `callimachus-mcp` binary for your platform on
  first run and execs it over stdio, so any MCP client can run it with
  `claude mcp add callimachus -- npx -y callimachus-mcp`. Adds a root `server.json`
  (listed as `io.github.betabots-llc/callimachus`), keeps it on the shared version
  via `sync-versions.mjs`, and adds a `publish-mcp` CI job that publishes to npm +
  the registry after each release.
- 0a3fe13: **Spend Ledger: what your AI coding actually cost.** Callimachus now captures per-message token usage + model (from the source files' usage blocks, currently Claude Code's input/output/cache tokens) and turns it into dollars with a built-in pricing table. Because it's the only tool with a unified cross-tool index, it's the only place that can show total spend, a per-model breakdown, and your most expensive threads in one view. Surfaces:

  - **Stats:** a "Spend" card (estimated total, by-model, priciest threads).
  - **`cal cost [project]`:** the same as text/JSON.

  The estimate uses published list prices, it's a cost x-ray, not a billing record, and calls on models with no price on file are flagged, not guessed. Token usage is captured during indexing into new `messages` columns (migration 0022); **run Reindex once** to backfill, since data indexed before this feature has no usage stored (the source files still carry it). All on-device.

## 0.6.2

### Patch Changes

- e5c2a5e: **Agent Session Snapshots — resumable cross-agent handoff, captured automatically.** Save a durable checkpoint of an indexed thread (its packed transcript plus a carry-forward block of the project's distilled decisions, gotchas, and open TODOs) and reload it to continue, across a context-window compaction or across tools (Claude Code -> Codex -> Cursor).

  - **Automatic capture (zero-effort):** installing the Claude Code integration now also registers `PreCompact` and `SubagentStop` hooks, so the live session is snapshotted right before its context is compacted and when a subagent finishes — no tool call required. Capture is best-effort and silent (it never breaks the agent loop), and keeps one rolling auto-snapshot per thread so it can't flood the list.
  - **`cal` commands:** `cal snapshot <thread-id> [-l LABEL]`, `cal snapshots [project]`, `cal resume <id> [-a AGENT]` (relaunches any agent CLI seeded with the checkpoint).
  - **MCP tools:** `snapshot_session`, `list_snapshots`, `load_snapshot` — so an agent can checkpoint and the next agent picks up exactly where it left off.

  Backed by a new `snapshots` table (migration 0019). Uninstalling the integration cleanly removes all three hooks.

- f549122: **Codebase hardening (internal).** CI now gates Rust quality, not just tests: `cargo fmt --check` and `cargo clippy --lib --bins --tests -- -D warnings` run on every PR alongside the existing typecheck/build/test, so lint regressions and formatting drift can't land. Cleared the 8 pre-existing clippy warnings to make the gate green. Added the first tests over previously-untested entry points: the embedded migrations validate + apply cleanly on a fresh DB (catches a malformed/out-of-order migration in CI instead of on a user's first launch), the `cal` flag parser, and the MCP `search_threads` / `get_thread` tools end-to-end. No user-facing behavior change.
- 29371b6: **ADR-style decisions + a contradiction guard.** Decisions can now carry a rationale (the "why"), and there's an active guard that surfaces settled decisions on a topic _before_ an agent re-litigates one. Call `check_decision` (MCP) or `cal check "<proposal>"` with what you're about to do; it returns the closest prior decisions, each with its rationale and a match score, held to a strict similarity floor so a false "you already decided X" stays rare. Record the why with `record_decision`'s new `rationale` field or `cal remember decision "<text>" --because "<why>"`. Turns the existing passive, post-hoc conflict review into a guardrail an agent (or you) can hit at decision time. Backed by a `rationale` column on facts (migration 0020).
- cfadfb7: **Git linkage: see which commits a conversation produced.** Callimachus now infers, entirely on-device, which git commits each thread led to, by overlapping the files a thread discussed (`file_mentions`) with `git log`'s changed files inside the thread's time window (shared-file count = a confidence cue). Surfaces:

  - **Desktop:** a "Produced commits" section on each thread, with a "Find produced commits" button that scans the project's `git log`.
  - **`cal commits`** (run in a repo, or `cal commits <path>`): the thread-to-commit timeline, one row per commit with its linked-thread count.
  - **MCP `linked_commits`**: an agent asks "which commit came out of this conversation?".

  Only top-level threads are linked (subagent transcripts are skipped, since their work is attributed to the parent session), and the timeline is grouped one row per commit so a big commit doesn't flood it. No git library is bundled (it shells out to `git`), and nothing leaves the machine. Backed by a new `thread_commits` table (migration 0021).

- 5b5db0e: **Switch HTTP from rustls to native TLS (OS trust store).** `reqwest`, `genai`, and the Tauri updater now use the platform's native TLS (Security.framework on macOS, SChannel on Windows, OpenSSL on Linux) instead of rustls. This drops `aws-lc-sys`/`aws-lc-rs` from the build entirely — a C library whose link step requires `libclang_rt.osx.a`, which recent GitHub `macos-latest` Xcode images don't ship, breaking CI/release linking. Using native TLS keeps the build on `macos-latest` (matching Tauri's recommended pipeline) and uses the OS certificate store. No user-facing behavior change.
- 0917128: **Search results are now diversified across threads.** A single long thread with many matching messages could previously fill the entire result list and bury every other thread. Both keyword and hybrid search now cap how many message-hits any one thread contributes (3), so other threads surface. The keyword path over-fetches before capping so the freed slots back-fill with other threads rather than shrinking the list; the hybrid path applies the cap once on the fused output, leaving the pre-fusion per-thread signal intact. Per-thread depth is still one click away via opening the thread. Factored into a tested `cap_per_thread` helper.
- 5066f75: **Recurring issues tracker: surface errors you keep hitting.** Callimachus now mines your whole indexed history for the same error recurring across sessions and tools, so chronic problems and blind spots become visible. A two-stage scan (an FTS pre-filter for error-ish messages, then a precise per-line extractor) pulls real error signatures, and a normalizer collapses the variable parts (paths, line:col, quoted identifiers, hashes) so the same error groups across runs even with different specifics. Surfaces:

  - **Coach dashboard:** a "Recurring errors" card (count, threads spanned, last seen).
  - **`cal issues [project]`:** the recurring-error list for the last 180 days, most frequent first (`--json` supported).

  Entirely on-device; only top-level sessions are scanned (subagents skipped), and it's the kind of cross-tool pattern only a unified local index can compute.

- c434695: **Hybrid search now weights fusion by semantic similarity, not just rank.** Reciprocal Rank Fusion previously gave every semantic match the same `1/(K+rank)` weight, so a marginal 0.4-cosine match counted as much as a strong 0.9 one at the same position. The semantic arm's RRF contribution is now scaled by a similarity factor over the `[0.35, 1.0]` retained range (`0.5×` at the floor, full weight at the top), so strong matches outrank marginal ones. The keyword (BM25) arm is untouched and the factor never exceeds 1.0, so the tuned keyword/semantic balance can't blow out, it only ever demotes weak semantic hits. Fusion is factored into testable `fuse_rrf` / `sem_weight` helpers with unit tests locking the behavior.

## 0.6.1

### Patch Changes

- 6979579: **Auto-update now actually runs.** The updater plugin, signing, and `latest.json` endpoint were all configured, but nothing in the app ever checked for updates, so installed builds never updated themselves. The app now checks on startup and, when a newer signed release is available, shows an "Update available" prompt; one click downloads + installs it (with a progress bar) and relaunches. Implemented per the Tauri v2 updater guide (`check()` → `update.downloadAndInstall(...)` → `relaunch()`); the check fails silently offline or in dev builds. Added the `process:allow-restart` capability and the explicit Windows `passive` install mode.

## 0.6.0

### Minor Changes

- ec3a772: **Activation + discoverability.** The feature set ran deep but was hard to find on a fresh install. This surfaces it.

  - **First-run onboarding.** A brand-new index no longer opens to a blank list: the Search landing welcomes you and indexes your local agent history in one click (with per-source progress). Once threads exist it steps aside.
  - **Always-visible feature tabs.** Knowledge, Ask, and Project Memory tabs are now always shown. When the Knowledge layer is off, each shows a short teaser + an "Enable in Settings" CTA instead of being hidden behind a flag.
  - **Command palette.** Press Cmd/Ctrl-K (or the header ⌘K) to jump between views and run common actions (reindex, build semantic index, toggle theme).
  - **Teaching empty states.** The Ask tab now offers clickable example questions, and the search bar hints the `file:` operator.

- 7af7b60: Two new ways to search your history.

  **Ask your history (RAG).** A new **Ask** tab (and `cal ask <question>`): ask a question in plain language → Callimachus retrieves the most relevant past threads, has your configured LLM answer with inline `[thread N]` citations, and lists the source threads to open. Needs distillation/LLM enabled. (No MCP tool — agents already synthesize from `search_threads` themselves.)

  **Code-aware search.** File-path mentions are now extracted from message text at index time (`src/embed/mod.rs`, `package.json`, …) into a `file_mentions` index. Search **`file:embed/mod.rs`** in the search bar to find every thread that touched that file; `cal files <path>` does the same from the CLI. Re-derived each index, so it never goes stale.

- eb97e0b: **Auto-distillation.** A new opt-in setting (Settings, under Knowledge) that distills new and changed threads in the background as they're indexed, so the knowledge surfaces (Ask, cross-thread recall, Project Memory, the `get_thread_knowledge` MCP tool) stay populated without ever clicking "Build memory" or distilling thread-by-thread.

  - Drains the corpus in paced batches, skips threads that previously errored, and re-distills threads that changed since their last pass.
  - Runs at startup and after each reindex; turning the setting on kicks an immediate drain.
  - Background and low-priority: it yields to a user-initiated reindex or semantic-index build (no write-lock contention), and is cancellable.
  - Free and on-device with Ollama; with a cloud engine it has a per-thread cost (hence opt-in). A subtle "Distilling knowledge N/M threads" indicator shows in the search header while it runs.

- ebffe40: **Coach: your history, surfaced proactively.** A new dashboard that turns the memory layer from something you query into something that tells you what it knows.

  - **Coding heatmap.** A GitHub-style grid of the last 52 weeks that fills the width, with a hover tooltip per day. It counts your own activity (user/assistant messages, excluding subagent transcripts), backed by a new `messages.ts` index so it stays fast on large histories.
  - **This week's digest.** The decisions and gotchas captured from your sessions in the last 7 days (LLM-distilled or agent-recorded), each clickable straight to its source thread.

  Available from the new **Coach** tab (and the Cmd-K palette). The heatmap works without the Knowledge layer; the digest fills in once distillation has run.

- 6302ab7: **Database performance + scalability overhaul** (from a full DB audit).

  - **Read-pool architecture.** UI read commands now run on a pool of read-only connections instead of serializing behind the single writer mutex. WAL allows unlimited concurrent readers, so searches, lists, recall, Ask, and Project Memory no longer queue behind each other or behind a write. The shared `Mutex<Connection>` is now the single writer only.
  - **Code-aware search uses an index.** `file:` search and `cal files` now match via a trigram FTS over file paths instead of a full-table `LIKE '%x%'` scan, and build every result row in one join (no per-row round-trip).
  - **Project Memory uses indexes.** Aggregation now matches the project path exactly and is backed by a new `facts(thread_id, kind)` index, instead of scanning a whole fact partition per open.
  - **New list index.** `idx_threads_subagent_updated` removes the temp-sort on every Recent / Projects / pending-distill list.
  - **Pragma tuning.** 64 MiB page cache, memory-mapped reads, in-memory temp store, and bounded WAL (autocheckpoint + size limit) on the writer; lighter read-only pragmas on pooled connections. A passive WAL checkpoint now runs at the end of reindex and the embedding build so the WAL file does not grow unbounded.
  - **VACUUM no longer freezes the UI.** It runs on a dedicated background connection instead of holding the shared mutex for the whole file rewrite.

  Bug fixes surfaced by the audit:

  - The file watcher (a second writer) now retries a lost write-lock race instead of silently dropping a newly indexed thread.
  - `cal star`, `cal tag`, and `cal distill` now open a writable connection. They previously failed with SQLITE_READONLY.

- 6d1100d: **Duplicate-work guard — "have I done this before?"** Describe a task and Callimachus surfaces the past _sessions_ where you (or your agent) solved something similar, each rolled up to its most-relevant decision or gotcha so you can reuse the earlier solution instead of redoing or re-deciding it.

  - **For your agent**: a new `find_prior_work` MCP tool (searches all projects unless scoped), meant to be called at the start of a task. The bundled `/recall` skill now tells agents to reach for it.
  - **CLI**: `cal similar <task…>`.
  - **In the app**: a "Have you done this before?" search on the Coach tab — results link straight to the source thread.

  Built on the existing semantic recall over distilled decisions/gotchas, grouped by thread. Needs distillation enabled to return results.

- e090b94: **Memory curation + trust.** Now that Project Memory is auto-generated (and fed to agents via the MCP tool, CLI seeding, and the memory file), you can vet it.

  - **Pin / edit / delete distilled facts** in the Projects view. Hover a decision, gotcha, or TODO for pin, edit (inline), and delete actions. Pinned facts rank first.
  - **Curated facts survive re-distillation.** Pinning, editing, or deleting a fact takes it out of the LLM's hands: auto-distill and re-index keep your pinned/edited facts and never resurrect a deleted one (kept as a tombstone). Edited facts are re-embedded so cross-thread recall matches the new wording.
  - **Conflict review.** A "Review conflicts" action asks the configured LLM which of a project's decisions contradict or supersede each other, and surfaces the pairs with a one-line reason and a quick hide action.
  - Hidden facts are filtered out everywhere they surface: Project Memory, cross-thread recall, the per-thread knowledge panel, and open-TODO lists.

- f62eddf: **Project Memory.** Aggregate the knowledge distilled across all of a project's threads into one durable memory: the decisions, gotchas, and open TODOs for that codebase, readable in the app, by agents, and from the CLI.

  - **Projects tab** (desktop): pick a project, see its aggregated decisions/gotchas/open-todos with links back to the source threads, plus a distillation-coverage chip.
  - **Build memory**: a background, cancellable, project-scoped distill that fills in every not-yet-distilled thread in the project (per-thread progress bar), mutually exclusive with reindex and the embedding build so the writers never collide.
  - **Synthesize brief**: an optional LLM summary of the project's memory ("what this is + key decisions"), and **Write memory file** drops a `.callimachus/memory.md` agents can be pointed at.
  - **MCP `project_memory` tool**: hands an agent its repo's accumulated memory (defaults to the current git repo) so it can recall what was decided at the start of a session.
  - **`cal memory [project]`**: the same memory from the CLI (defaults to the current repo).
  - **Open in CLI** now prepends the project's memory to the seeded context, so a relaunched session opens with what was already decided, not just one thread's transcript.

- 1d71c8c: **Reliable + automatic memory.** Two changes that make the project-memory layer trustworthy and self-injecting.

  **Canonical project identity.** Threads now carry a normalized `project_key` (computed at index time, backfilled at startup): a repo's git root with symlinks resolved, `~` expanded, and trailing slashes trimmed. Project Memory, scoped recall, write-back, and the picker all group on this key, so the same repo opened via a worktree, a symlink, `~/x` vs `/Users/me/x`, or a subdir no longer fragments into separate, half-empty memories. The `cal memory` / MCP `project_memory` / `cal remember` inputs are canonicalized the same way.

  **Automatic memory injection.** Get a project's distilled memory into an agent's context without manual lookup:

  - `cal agents [project] [-o FILE]` and a desktop **Update AGENTS.md** button write/refresh a managed memory block (between markers, preserving your own content) in the repo's `AGENTS.md` (or `CLAUDE.md`), so any agent that reads project context opens with the prior decisions and gotchas.
  - `cal hook [project]` prints the repo's memory for use as a Claude Code SessionStart hook command (emits nothing when there's no memory).

- 8013e6a: **Two-way agent memory.** Agents (and you) can now WRITE to memory, not just read it, and the richest capabilities reach the MCP surface agents actually use. The MCP server is now a read+write tool (15 tools).

  - **Closeable TODOs.** Mark an open TODO done so it drops out of every open-TODO list: a check button on each TODO in the Knowledge tab, `cal done <id>`, and a `complete_todo` MCP tool. Completion (and pin/dismiss) now survives re-indexing, so the task list stops re-filling with noise you already handled.
  - **Agent write-back.** Record a decision or gotcha mid-session that persists in the project's memory and immediately surfaces in recall: `record_decision` / `record_gotcha` MCP tools and `cal remember <decision|gotcha> <text>`. Recorded facts are pinned, embedded for cross-thread recall, and flow through Project Memory and the memory file like distilled facts.
  - **MCP parity.** The already-built RAG and code-aware search are now MCP tools too: `ask_history` (a synthesized, cited answer over the user's history) and `threads_for_file` (which past sessions touched a path). The ask retrieval is factored into one shared path used by the app, `cal ask`, and MCP.
  - The MCP server and the standalone `callimachus-mcp` binary now open the index read-write (WAL + busy_timeout let them coexist with the desktop app's writer).

- e355ee4: **Zero-config memory injection.** The one-click Claude Code integration now also installs a **SessionStart hook**, so each repo's distilled memory (decisions, gotchas, open TODOs) is automatically injected at the start of every Claude Code session — no manual hook setup, and nothing to remember to run.

  - The "Enable for Claude Code" action now writes a Callimachus `SessionStart` hook into `~/.claude/settings.json` alongside the `/recall` skill, the MCP server, and the `cal` CLI. It's merged safely (preserves your other settings and hooks, refuses to touch an unparseable file) and is fully idempotent — re-installing never duplicates it.
  - "Remove" cleanly strips the hook (and only ours) back out.
  - The Settings card shows the hook's status and Reinstall picks it up for anyone who set up the integration before this release.

  **One-click multi-agent setup.** A new "Other agents" section in Settings registers the `callimachus` MCP server with the _other_ agents you have installed — **Codex** (`~/.codex/config.toml`), **Cursor** (`~/.cursor/mcp.json`), and **Gemini CLI** (`~/.gemini/settings.json`) — so they can search your history too. It only touches agents whose config already exists (never creates one for an agent you don't use), merges safely (preserves the rest of each config, refuses unparseable files, format-preserving for Codex's TOML), is idempotent, and is fully removable. The per-repo "Update AGENTS.md" action already covers agents that read `AGENTS.md`.

### Patch Changes

- 6302ab7: Fix the semantic-index build appearing to get stuck, and the Build-memory button silently doing nothing.

  - **Giant messages no longer stall embedding.** A pasted log of a few hundred KB was chunked into hundreds of vectors (a 600 KB message became ~430 chunks), so any batch containing one crawled and looked frozen. Chunks per message are now capped (the first 16 capture the semantic gist; FTS still searches the full text), which also shrinks the vector index.
  - **A failed batch is skipped, not fatal.** If the embedder errors on a batch, those messages are marked done (still FTS-searchable) and the job continues, instead of the whole build stopping at that point.
  - **Build memory now shows why it is blocked.** Distillation shares the write lock with the embedding build and reindex, so it is mutually exclusive with them; the Build-memory button now disables and shows "Embedding..." / "Indexing..." instead of silently no-op'ing while one of those runs.

- c225960: **Incremental indexing + indexer reliability.**

  - **Incremental re-index.** Re-indexing a thread used to delete and re-insert (and re-FTS) every message, so an actively-growing session got progressively more expensive to keep fresh, both on manual re-index and on every file-watcher save. Now, when the stored messages are an exact prefix of the new parse, only the new tail is inserted; any mismatch or shrink falls back to a correct full replace. Heuristic TODOs and file mentions are preserved on append (with their per-thread caps still enforced against the thread total), and LLM-distilled knowledge is still invalidated when content changes (including same-length in-place edits).
  - **No more silently dropped threads.** The single-DB sources (Cursor, Goose) and OpenCode recorded their `index_state`/fingerprint _before_ the upserts succeeded, so a thread that failed mid-pass on a transient write-lock could be marked "done" and skipped on the watcher's retry. State is now recorded only after the work succeeds.
  - **Correct source labels.** Roo and Kilo task files are now recorded in `index_state` under their own source kind instead of `cline`.

- ec3a772: **Indexing: no more "database is locked", and real progress.**

  - **Concurrency fix.** Every write transaction now uses `BEGIN IMMEDIATE` instead of a DEFERRED transaction. With multiple writer connections (the app's shared connection, the reindex's own connection, and the file watcher's), a DEFERRED transaction that read-then-upgrades could hit `SQLITE_BUSY` immediately, bypassing `busy_timeout` — which surfaced as intermittent "database is locked" failures that stalled a reindex. `busy_timeout` was also raised (5s to 15s) so concurrent writers queue instead of erroring.
  - **Live, thread-granular progress.** Reindex progress is now reported per thread (not per source), so the bar keeps moving with a running "N scanned" count even while one large source (usually Claude Code) works through thousands of files, instead of sitting at 0%. The total is estimated from the existing thread count (accurate on a re-index, indeterminate on a first run).
  - **Consistent DB path.** The desktop app now resolves its index location through the same `CALLIMACHUS_DB`-aware resolver as the indexer, watcher, and sidecars, instead of hardcoding the app-data path. Setting `CALLIMACHUS_DB` to a throwaway path now correctly drives the whole app (handy for exercising a clean first-run).

- 4dff053: Knowledge layer (slice 2): opt-in LLM distillation — decisions, gotchas & summaries.

  Turn a thread into a high-signal recap. Distillation is **off by default** and never sends anything until you turn it on and pick an engine:

  - **Local-first:** run a local model via **Ollama** (nothing leaves your machine), or bring your own cloud API key.
  - **Per-thread & on demand:** click **Distill** on a thread — no bulk job, no surprise spend. Decisions/gotchas/summary render right in the thread view.
  - **Never stale:** re-indexing a thread whose messages changed automatically invalidates its distilled knowledge.

  Surfaces: a **Knowledge** section in the desktop thread view + a Settings card to enable/choose the engine; `cal distill <id>` / `cal knowledge <id>`; and an MCP `get_thread_knowledge` tool so agents can pull a thread's recap instead of the whole transcript.

  Built on the same `facts` table as the free TODO tier. Structured output uses a portable prompt-and-parse approach that works identically across Ollama and cloud providers. Cross-thread semantic recall of decisions/gotchas is wired for a follow-up (the `vec_facts` table ships here, unused).

- 75263ee: Cross-thread knowledge recall — search your distilled decisions & gotchas across every thread.

  When you distill a thread, its decisions and gotchas are now embedded on-device into a vector index, so you can recall them semantically across your whole history:

  - **Knowledge tab:** type a query → semantic recall of decisions/gotchas from any thread, ranked by match, each linking back to its source thread. (An empty box still shows your open TODOs.)
  - **`cal decisions <query>`** / **`cal gotchas <query>`** from the CLI.
  - **MCP:** `recall_decisions` / `recall_gotchas` tools so an agent can ask "did I already decide this?" before re-deciding, or "have I hit this before?" before repeating a mistake.

- 4dff053: Knowledge layer: an opt-in **Knowledge** feature that surfaces what matters from your history.

  Off by default — enable it in Settings. The free, on-device tier scans your threads for action items (markdown task checkboxes `- [ ]` and word-boundaried `TODO`/`FIXME` markers, with a noise filter for code/table/JSON blobs) and stores them in a new `facts` table. Enabling backfills from already-indexed text (no re-index); disabling clears it.

  - **Desktop:** a **Todos** tab (shown only when the feature is on) lists every open TODO with its source/thread; click to jump to the conversation.
  - **`cal todos`** `[-p PROJECT] [-s SOURCE] [-n LIMIT] [--json]` — list open TODOs from the CLI.
  - **MCP:** a `list_open_todos` tool so agents can ask "what did I leave unfinished?".

  TODOs re-derive on every index, so they never go stale. The LLM-distilled tier (decisions, gotchas, summaries — lazy, per-thread, with consent) reuses the same `facts` table.

- 41726e5: **Trustworthy recall.** A code audit surfaced two correctness bugs in cross-thread recall that could make the memory layer confidently wrong; both are fixed.

  - **Similarity floor.** Semantic recall (`recall_decisions` / `recall_gotchas` / `cal decisions|gotchas`) and the `find_prior_work` / `cal similar` guard ran a pure k-nearest-neighbor search with no relevance threshold, so a query with no real match still returned its nearest (irrelevant) neighbors — the "have I done this before?" guard could fabricate prior work that didn't exist. Recall now drops neighbors below a cosine floor and returns an explicit empty result; the prior-work guard holds to a stricter floor since an agent acts on it.
  - **Project scoping.** Project-scoped recall filtered on `project_path` while facts are written and aggregated by `COALESCE(project_key, project_path)`, so the canonical-key threads (the whole point of the project-key backfill) silently dropped out of scoped results. Recall now scopes the same way writes do.

- fc74698: Reindex is now a background job with a per-source progress bar.

  - **Non-blocking:** re-indexing your sources runs in the background and reports a per-source progress bar, so the UI stays responsive while it works.
  - **No write-lock fights:** the reindex and the semantic-index build are now mutually exclusive; each defers to the other so they never contend for the SQLite write lock.
  - **Resilient embedding:** when the embed job hits a locked batch it re-queues that batch and retries instead of aborting the whole job.

- ddf1ef3: **Search quality + distillation cost fixes** (from the app audit).

  - **Hybrid search now respects project scope and filters noise.** The semantic arm previously ignored the project filter entirely (a project-scoped hybrid search leaked cross-project hits) and applied no relevance floor (a query with no good match still injected its nearest neighbors). It now scopes by `COALESCE(project_key, project_path)` like the rest of the app, and drops sub-threshold cosine matches. The keyword arm's project filter was aligned to the same `COALESCE` scoping.
  - **Keyword search recall is much higher.** Full-text queries were built as a strict AND of exact-phrase tokens, so a multi-word natural-language query only matched messages containing every term verbatim. Tokens are now prefix-matched (`embed` matches `embedder`/`embedding`), and a strict-AND pass is backfilled with a looser OR pass when it under-fills — precise hits still rank first.
  - **No more wasted re-distills.** Distillation staleness keyed off total `message_count`, which includes tool/system rows; agent transcripts grow mostly via tool output, so threads kept flipping "stale" and re-running paid LLM distillation that produced identical results. Staleness now keys off a stored `distillable_count` (user/assistant messages only). A migration backfills it and keeps already-distilled threads from re-running.

- 4dff053: Thread view: rich rendering + chat-style scrolling.

  - Indexed messages now render as **Markdown** with **syntax-highlighted** code blocks (previously plain text); tool calls and JSON results are pretty-printed.
  - Threads **open at the newest message and scroll up for history**, like a chat — on the same virtualized list.
  - Big scroll-performance win on long threads: message HTML is parsed once and cached, so scrolling back through hundreds of messages no longer re-parses + re-highlights each one. Thin, inset scrollbars so a code block's bar no longer overlaps the line beneath it.
  - The thread header is decluttered into Knowledge · ★ · Resume · a "more" menu.

## 0.5.0

### Minor Changes

- ba7be6c: Stars, tags & collections — organize your archive, not just search it.

  - **Star** any thread and attach free-form **tags**, then filter the list by a ⭐ Starred toggle and tag chips in the search bar.
  - Stars and tags survive re-indexing (stars live on the thread row but are never overwritten by the indexer; tags are keyed separately).
  - Reaches every surface: desktop UI, the `cal` CLI (`cal star <id> [--off]`, `cal tag <id> <tag…>`, `cal tags`, plus `--starred` / `-t <tag>` on `recent`/`related`/`search`), and the MCP server (`recent_threads` gains `starred`/`tags`, new `list_tags` tool) so agents can ask for "my starred auth threads".
  - Added a `busy_timeout` on the SQLite connection so `cal` writes (star/tag) wait for the app's lock instead of failing with "database is locked".

### Patch Changes

- 9ce2bae: Keep the UI responsive while the semantic index builds.

  - **Cap inference threads.** The on-device embedding model (fastembed/ONNX) ran with no thread limit, pinning every CPU core for the whole build and starving the UI. It now leaves 2 logical cores free (`available_parallelism() - 2`).
  - **Stop holding the DB lock across query inference.** Hybrid/semantic search embedded the query _while holding_ the single SQLite connection, which froze every other UI command during a build. The query vector is now computed before the DB lock is taken (new `embed_query` / `semantic_search_vec` / `hybrid_vec` split).
  - **Push-based embedding progress.** The UI polled `embedding_status` every 700ms (two locked `COUNT(*)` scans); it now updates from `embed:progress`/`embed:done` events the backend already emitted, with only a slow safety-net refetch. Also disabled `refetchOnWindowFocus` (which fired a ~5-query burst, each serialized behind the one connection) and added a small `staleTime`.

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
