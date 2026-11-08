<p align="center">
  <img src="assets/hero.png" alt="Callimachus: the catalogue for your AI coding history" width="100%">
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-AGPL--3.0-2C436C.svg" alt="License: AGPL-3.0"></a>
  <img src="https://img.shields.io/badge/platform-macOS%20%7C%20Windows%20%7C%20Linux-C16E2C.svg" alt="Platform: macOS, Windows, Linux">
  <img src="https://img.shields.io/badge/Tauri-2-24C8DB?logo=tauri&logoColor=white" alt="Tauri 2">
  <img src="https://img.shields.io/badge/Rust-stable-E0A93C?logo=rust&logoColor=white" alt="Rust">
  <img src="https://img.shields.io/badge/React-19-61DAFB?logo=react&logoColor=white" alt="React 19">
  <img src="https://img.shields.io/badge/data-local--first-3FA34D.svg" alt="Local-first">
</p>

> **Local index & search for your AI coding-agent threads** across **13 tools** (Claude Code, Codex, Cursor, Gemini CLI, Qwen Code, Goose, OpenCode, Continue, Cline, Roo Code, Kilo Code, Copilot Chat, Copilot CLI), plus a provider-agnostic chat, an MCP server, a CLI, and a VS Code / Cursor extension. Your history stays on your machine.

Named for [Callimachus](https://en.wikipedia.org/wiki/Callimachus), who built the first catalogue of the Library of Alexandria.

## Download

Grab the latest signed build from **[Releases](../../releases/latest)**: macOS (`.dmg`, Apple Silicon), Windows (`.msi` or `-setup.exe`), or Linux (`.AppImage`, `.deb` or `.rpm`). The app auto-updates from there on. Prefer to build it yourself? See [Develop](#develop).

**macOS:** drag Callimachus into Applications and open it from there before enabling the agent integration. While it runs from the disk image or a quarantined download, Settings won't register it, because that path disappears when the app quits.

**Windows uninstall:** the `-setup.exe` uninstaller removes what the app added (the `/recall` skill, the Claude Code, Codex and Gemini CLI hooks, the MCP entries for Claude Code, Codex, Cursor and Gemini CLI, and `cal.exe`), but the `.msi` uninstaller doesn't. Before removing an MSI install, click **Remove** for Claude Code and under **Other agents** in **Settings → Agents** (the Other agents Remove also takes out the Codex and Gemini CLI memory hooks), or run `callimachus.exe --uninstall-integration`; otherwise the hooks keep pointing at the deleted app and each agent reports a hook error at every session start.

The same release also carries standalone `cal-<target>` and `callimachus-mcp-<target>` binaries for CLI/MCP-only use, with their SHA-256 sums in `SHA256SUMS`. **Linux** builds and binaries need **glibc 2.38 or newer** (Ubuntu 24.04+, Debian 13+, Fedora 39+); Ubuntu 22.04, Debian 12 and RHEL 9 are too old for the bundled ONNX Runtime.

**Updates:** the first launch after an update upgrades the index in the background; the splash keeps animating, and on a large index this can take tens of seconds. An older build can't open an upgraded index: an older app shows "This index is from a newer Callimachus", and an older `cal`, `callimachus-mcp` or pinned `npx callimachus-mcp@<version>` stops with an error. Whichever of them opens the index first does the upgrade, so keep the app, the npx launcher and any standalone binaries on the same release.

## What it does

- **Indexes** every conversation from 13 coding agents into one local SQLite store: Claude Code, Codex, Cursor (the editor's chats, its agent transcripts and the Cursor CLI), Gemini CLI, Qwen Code, Goose, OpenCode, Continue, Cline, Roo Code, Kilo Code, Copilot Chat (the built-in chat panel of VS Code, Cursor, VSCodium and Windsurf), and the Copilot CLI. Adding another source is a [small, documented contract](apps/desktop/src-tauri/core/src/indexer/README.md).
- **Searches** them with hybrid ranking: keyword (SQLite FTS5 / BM25) fused with on-device semantic similarity (sqlite-vec KNN, no cloud) via Reciprocal Rank Fusion. Filter by source, project, subagents, starred, and tags.
- **Finds code-aware**: type `file:embed/mod.rs` in the search bar (or `cal files <path>`) to find every thread that touched a path; backed by a file-mention index built at index time.
- **Distills knowledge**: free heuristic TODO extraction (always on, no LLM), plus LLM distillation of decisions, gotchas, and summaries behind one consent click that names the engine and where transcripts go (the Turn on memory card in Projects or Chat's Ask mode preselects a detected Claude Code or Codex CLI, then local Ollama, then a saved key; Settings > AI engine has the same switch), with cross-thread semantic recall of past decisions/gotchas. Optional **auto-distillation** works through threads active in the last 30 days in the background, up to a daily limit you set next to its toggle (50 threads by default, 0 pauses it; Settings shows what's left today), so memory self-populates without an open-ended bill. Runs on local Ollama (keyless), your logged-in **Claude Code or Codex CLI** (keyless, billed to whatever that CLI is signed in with), or a cloud API key.
- **Imports what agents already write**: Claude Code's own auto memory (the topic files under `~/.claude/projects/<project>/memory/`) joins each repo's project memory with no LLM: feedback memories become gotchas and project memories decisions, tagged "Claude Code memory", refreshed when a file changes and removed when it goes (Settings > Agents turns it off and removes them). The managed `AGENTS.md` / `CLAUDE.md` block and `.callimachus/memory.md` leave them out, since Claude Code already loads them and those files get committed. When auto-distillation is on, a SessionEnd hook moves the session that just ended to the front of the queue, so its memory lands minutes after it ends.
- **Curates the facts**: pin, edit, or hide distilled facts so your edits survive re-distilling, record a decision or gotcha yourself with **Remember** in the Projects tab (saved pinned), plus an LLM **"Review conflicts"** pass that flags decisions that contradict each other.
- **Remembers per project**: a **Projects** tab gives each repo one page: its recent sessions, its decisions, gotchas, and open TODOs aggregated into durable memory (grouped by a canonical project key, so worktrees / symlinks / `~` don't split one repo), its snapshots, and exactly what its agents are given (the SessionStart hook's text and the managed `AGENTS.md` block, with their sizes), plus an LLM brief and a managed `.callimachus/memory.md`. That memory is prepended when you "Open in CLI", and you can inject it into any agent automatically: **Update AGENTS.md** (or `cal agents`) writes a managed block into the repo's `AGENTS.md` / `CLAUDE.md`, and `cal hook` feeds a size-budgeted version to a Claude Code SessionStart hook, labeled as notes from past sessions rather than instructions. A repo with no memory yet still gets something on day one, with no LLM: a digest of its last few sessions (title, agent, branch, age, the latest summary or prompt, touched files, open TODOs, and the latest snapshot to resume), also appended under the facts when they leave room. The hook runs on startup, `/clear` and compaction (a shorter, facts-only refresher), stays quiet on a resumed session, and skips a repo where Claude Code already loads the managed block (a `CLAUDE.md`, `CLAUDE.local.md` or a file one `@`-imports, or `AGENTS.md` when no `CLAUDE.md` exists), so the memory is injected once. `cal hook --explain` shows what a new session would get, `cal hook --mute` turns the digest off for one repo, and Settings > Agents turns it off everywhere. **Codex and Gemini CLI get the same memory through their own hooks** (Settings > Agents > Other agents > Memory hooks): `cal hook --for codex` / `--for gemini` on session start and `/clear` (Codex also after compaction), wrapped in each agent's hook output format, plus their imported Claude Code memories, marked as such, since neither loads Claude's auto memory. The hook stays quiet in a repo whose `AGENTS.md` (Codex) or `GEMINI.md` (Gemini CLI) already holds the managed block. Their SessionEnd hooks queue the ended session for auto-distillation like Claude Code's, and a Codex PreCompact hook snapshots the session. Codex asks you to trust new hooks once (`/hooks` in Codex). The `/recall` skill is also copied to `~/.agents/skills/`, the shared skills folder other agents read. Cursor gets no hook: its session-start payload has no working directory. Gemini CLI loads every context file it is configured with plus their `@` imports, and Codex its fallback filenames too, so the hook stays silent wherever the managed block already lives.
- **Memory that travels with the repo**: **Publish to repo** on a project's Memory section (or `cal memory publish`) writes its pinned facts and the ones you or an agent recorded (with `--all`, or the switch in the preview, the distilled ones too) to `.callimachus/memory/decisions.jsonl` and `gotchas.jsonl` in the checkout, one JSON object per line sorted by a stable id, so branches merge line by line. The app shows the counts and every added, updated or removed fact before anything is written (`cal memory publish` writes, then lists the same changes), secrets are redacted, Claude Code's auto memory is never included, lines a teammate added are kept, and nothing is committed or published on its own. Everyone with Callimachus who works in the repo gets those facts as the app (or `cal index`) indexes a session there, or with `cal memory import` (`--review` lists the changes first): marked "From the repo", never pinned, included in the hook and the `AGENTS.md` block as `(repo)` under a line naming the files, and with your own edit of one winning over the file (hiding one is local to you: publish never removes a teammate's line). A fact removed from the files is hidden for everyone on their next import (once no other clone of the repo on their machine still has it); the ones you published show as "Shared".
- **Asks your history (RAG)**: the Ask mode in Chat gives a synthesized, cited answer over your own threads, with `[thread N]` citations that open the sources it used; it is saved with your chats and follow-ups continue as a normal chat. Needs an LLM engine (memory turned on in Settings > AI engine).
- **Organizes into collections**: star threads and attach free-form tags, then filter the list by starred or by tag.
- **Chats** with an in-app agent (Anthropic / OpenAI / Gemini / OpenRouter / Ollama, local or Cloud, or your logged-in **Claude Code / Codex CLI** with no key) that can **search your own history** and **run shell commands with your approval**; streaming, cancellable, with live model lists. Chats are saved and become searchable too.
- **Carries context across tools**: open any thread in any agent CLI ("Open in Claude / Codex / Gemini …", seeded with the packed transcript), resume a Claude Code / Codex thread in its native CLI, copy context, or export a thread to Obsidian (optionally AI-summarized with decisions / gotchas / TODOs).
- **Links threads to commits**: infers **on-device** which git commits a thread produced, by overlapping the files a thread discussed with `git log`'s changed files inside the thread's time window (shared-file count = confidence). See it as a thread→commit timeline (`cal commits`), per-thread in the desktop UI ("Produced commits"), or via the `linked_commits` MCP tool. Run it backwards from code with `cal why <file>:<line>`: `git blame` names the commit behind the line, and you get the sessions that produced it (the ones that edited the file first) with their decisions and gotchas; the same answer comes from `threads_for_file` with a `line`, and from **Why This Line** in the VS Code / Cursor extension. The background watcher links new sessions' commits as they are indexed, and records which files each session read, edited or wrote from its tool calls (sessions indexed before the update are filled in once, in the background).
- **Snapshots agent sessions**: durable, resumable checkpoints of a thread (packed transcript + carry-forward project memory) for handoff across a context-window compaction or across tools; auto-captured by a Claude Code PreCompact hook right before a context compaction, which also keeps Claude Code's compaction summary as the thread's summary. Take, list, and resume them (`cal snapshot` / `cal snapshots` / `cal resume`, or the `snapshot_session` / `list_snapshots` / `load_snapshot` MCP tools), or in the app, where a project's page lists its snapshots to read, delete, or resume in any agent CLI.
- **Guards decisions**: decisions can carry a **rationale** ("why"), and an active guard surfaces settled decisions on a topic before an agent re-litigates one (`cal check "<proposal>"` / the `check_decision` MCP tool).
- **Recalls before you redo**: an opt-in Claude Code hook quietly injects "you may have solved this before" into the agent's context on each prompt when it strongly matches prior work (Settings → Agents → **Proactive recall**, off by default, signal-floored and per-session deduped).
- **Mines recurring issues**: surfaces the errors you keep hitting across all your tools (normalized signatures, last 180 days) so you fix the pattern, not the instance (on the Project page, or `cal issues`).
- **Remembers how you fixed them**: when a session that hit a recurring error then edited the file the error named and a commit linked to that session changed that file, that commit is recorded as the fix in that repo. The Project page shows "Last fixed in `<sha>`" with a link to the session, `cal issues` and the `recurring_issues` MCP tool return the fix with the sessions behind the error, and the fix becomes a **playbook** gotcha in the repo's memory ("`<error>`: seen N times; fixed in `<sha>` (`<subject>`), see thread `<id>`"), refreshed in place, never pinned. Generic errors (a bare `error:`, a Python traceback header, a path that wasn't there) get no playbook. Fixes need the files a session edited, which come from its tool calls; sessions indexed before the update are filled in once, in the background, with no reindex. An opt-in Claude Code hook (Settings → Agents → **Error playbooks**, off by default) adds that line to Claude's context when a shell command it runs fails with an error fixed before in the same repo, once per error per session; it runs after every shell command, and output without an error line costs a few milliseconds. The hook ships for Claude Code first; Codex and Gemini CLI do not get it yet.
- **Shows what it did for you**: a Stats card counts, per week, the sessions that started with injected memory or a recent-sessions digest, the prompts where recall surfaced prior work, and the MCP tool calls answered (`cal stats` prints the same as one line). The counts are appended to `ledger.jsonl` next to the index (`<name>.ledger.jsonl` for any other index file, such as the dev build's `index.dev.db`), and once a day is over the app moves that day's counts into the index itself; neither leaves the machine. The same card shows how many indexed Claude Code sessions are still searchable after Claude Code deleted their transcripts (its `cleanupPeriodDays`, 30 days by default), with a reversible "Keep transcripts for a year" that sets it to 365 in Claude Code's `settings.json` after you confirm.
- **Shows what it cost**: captures per-message token usage and model at index time (Claude Code, Codex, Gemini CLI, Qwen Code, OpenCode and Copilot), then estimates dollar spend by model, by session, by the commits those sessions produced, by project and by day. Each session gets short neutral notes on where its spend went (cache reads, turns past 50, subagents, a tool failing over and over). Figures are list-price equivalents from an embedded price table (`apps/desktop/src-tauri/core/prices.json`), so for a subscription they show what the same usage would cost at API rates; models without a flat published list price (open-weight models, Qwen's prompt-tiered models, a release not priced yet, Claude Code's placeholder turns) are listed as unpriced with the reason. See it in the Stats card (with the last 30 days and the priciest commits), on a Project page, or with `cal cost --by thread|commit|project|day`. Codex, Gemini CLI, Qwen Code and OpenCode sessions indexed before the update get their usage filled in on their own, once, in the background (by the app, or by `cal index`), with no reindex.
- **Feeds PR audits**: `cal audit-pr <repo> --changed-files … --shas …` returns one JSON bundle (per-commit provenance back to the session that produced it, per-file prior threads + reasoning, repo recurring errors, project memory) so an external local PR-review tool can show the history behind a diff.
- **Surfaces to your agents**: a bundled MCP server (`callimachus-mcp`) exposes the index as tools any agent can call mid-session, and it's two-way: agents can write back into Callimachus's own memory (close TODOs, record decisions/gotchas, snapshot a session) without ever touching your files. The `/recall` skill teaches them when to use it.
- **Stays current** via a background file watcher, plus a catch-up pass at launch for the sessions your agents wrote while the app was closed; **stays private**: the index never leaves your machine, API keys live in the OS keychain, conversation content goes only to an AI provider you pick, and usage analytics are opt-in and content-free (see [Security & privacy](#security--privacy)).

## Stack

- **Shell:** Tauri 2 (Rust) + React 19 + TypeScript + **Vite 8**
- **Store/search:** bundled SQLite + FTS5 (`rusqlite`); on-device embeddings via `fastembed` (bge-small-en-v1.5, 384-dim); KNN in SQL via `sqlite-vec` (vec0)
- **Watcher:** `notify` + debouncer
- **Chat:** multi-provider via the `genai` crate (Anthropic / OpenAI / Gemini / OpenRouter / Ollama local + Ollama Cloud), each with a configurable base URL, streaming tokens over a Tauri Channel, cancellable, with agent tool-calls (history search + approved shell)
- **Secrets:** OS credential store via the cross-platform `keyring` crate: macOS Keychain, Windows Credential Manager, Linux Secret Service
- **Entry points:** the app binary also runs as `cal` (through the `~/.local/bin/cal` link or `cal.exe`) and as the MCP server (`--mcp`); the standalone `cal` and `callimachus-mcp` release binaries are the same code built on their own. All of them run the Tauri-free core crate (`callimachus-core`, which the standalone binaries link without Tauri, GTK or WebKit) against the same `index.db`
- **Editor:** a VS Code / Cursor extension (`apps/vscode`, published to the Marketplace + Open VSX) that shells out to `cal`

## Monorepo

This is a [Turborepo](https://turborepo.com) + pnpm workspace.

```
apps/
  desktop/            # the Tauri 2 desktop app; src-tauri/core is the Tauri-free core (index,
                      # cal, MCP server, jobs), src-tauri/standalone the cal + callimachus-mcp bins
  vscode/             # VS Code / Cursor extension (search history from the editor)
  web/                # callimachus.app, the marketing + download site (TanStack Start)
packages/
  callimachus-mcp/    # the npm launcher behind `npx -y callimachus-mcp`
.changeset/           # version + changelog management
scripts/              # version-sync, release tagging
```

Releases, versioning, and the auto-updater are documented in [RELEASING.md](RELEASING.md).

## Develop

```bash
pnpm install
pnpm desktop:dev      # launches the desktop window (tauri dev) against a separate dev index

# from the repo root, across all apps:
pnpm build            # turbo: build every app's frontend
pnpm typecheck        # turbo: typecheck every app
```

First launch: the index is empty, so the Search screen offers **Index my history** (or use **Reindex** in **Settings → Sources**); then **Build semantic index** enables semantic search. Later launches also index whatever your agents wrote while the app was closed. **Reindex** runs as a background job with a per-source progress bar, separate from **Build semantic index**. The two are mutually exclusive (one pauses while the other holds the write lock).

### Tests

```bash
cd apps/desktop/src-tauri
cargo test                                   # fast unit tests, every crate in the workspace
cargo test -p callimachus-core -- --ignored --nocapture   # real-data + model + keychain smoke tests
```

The `--ignored` tests touch live data on this machine: each source has a `real_<source>_index` smoke test that indexes your real history read-only (`~/.claude`, `~/.codex`, Cursor, `~/.gemini`, `~/.qwen`, Goose, OpenCode, Continue, Cline/Roo/Kilo), plus the embedding-model download (first run, needs network) and a Keychain round-trip.

## Use your history anywhere

Beyond the desktop window, the same local index is reachable from your agents, terminal, and editor, all reading one `index.db`.

**Claude Code plugin**: the one-step setup for Claude Code. It bundles the `/recall` skill (as `/callimachus:recall`), the MCP server (through `npx -y callimachus-mcp@<version>`, pinned to the plugin's release) and the SessionStart, PreCompact and SessionEnd memory hooks ([plugins/claude-code](plugins/claude-code)):

```bash
claude plugin marketplace add BetaBots-LLC/callimachus
# then, inside Claude Code:
/plugin install callimachus@callimachus
```

The hooks run the `cal` CLI at `~/.local/bin/cal`, so open the desktop app (which builds the index) and click **Install the cal CLI** in **Settings → Agents** once (the Claude Code button's label while the plugin is enabled): that installs only `cal` and removes the app's own skill, MCP entry and hooks, so memory is never injected twice and only one server starts; Settings then says the integration is managed by the plugin. Until `cal` exists the hooks do nothing. **Without the desktop app** (macOS and Linux), `npx callimachus-mcp --install-cal` downloads the release's `cal` (checked against the same `SHA256SUMS` as the MCP server) and links it at `~/.local/bin/cal`; run `cal index` once to build the index, and from then on the plugin's SessionStart hook (`cal index --if-stale`) starts a catch-up pass in the background whenever the index is more than an hour old and neither the app nor `cal watch` keeps it fresh. You give up the app's windows (the Projects page, Chat, the harvest controls and Settings, where memory distillation is turned on); search, recall, the memory hooks and the MCP server work the same ([plugins/claude-code](plugins/claude-code/README.md#without-the-desktop-app)). The plugin needs the app at 0.11.4 or later: the marketplace serves it from the latest release tag, with the MCP server pinned to that release, so keep the app on the latest release too. **On Windows, use Settings → Agents instead of the plugin**: Claude Code runs plugin hooks through Git Bash (or PowerShell, where they fail with a hook error), and the app keeps its own skill, MCP entry and hooks there, so the plugin would load them twice.

**MCP server**: let any agent search its own past work mid-session. It reads the index the desktop app builds and does not index anything by itself, so install the app and let it index once first (or, without the app, run `cal index`; see below). The quickest way to register it is `npx` (it downloads the prebuilt binary on first run and verifies its SHA-256); it's also listed on the official [MCP registry](https://registry.modelcontextprotocol.io) as `io.github.BetaBots-LLC/callimachus`:

```bash
claude mcp add callimachus -- npx -y callimachus-mcp    # or any MCP client
```

Already running the desktop app? You don't need npm: **Settings → Agents → Enable for Claude Code** registers the app itself (launched with `--mcp`) as Claude Code's MCP server, and **Settings → Agents → Other agents** does the same for Codex, Cursor and Gemini CLI. The app keeps those entries pointed at itself after an update or a move. You can also download the standalone `callimachus-mcp-<target>` binary from [Releases](../../releases/latest), or build from a checkout with `cargo install --locked --path apps/desktop/src-tauri/standalone --bin callimachus-mcp` (add `--force` over a source install from an earlier version).

Tools (22), read **and** write. Reads (18): `search_threads`, `search_current_project` (auto-scoped to the repo it runs in), `recent_threads`, `get_thread`, `list_tags`, `list_open_todos`, `get_thread_knowledge`, `recall_decisions`, `recall_gotchas`, `find_prior_work` (the "have I done this before?" guard: prior sessions similar to a task), `project_memory` (a project's aggregated decisions / gotchas / open TODOs), `ask_history` (a cited RAG answer over your history), `threads_for_file` (which sessions touched a path; with a `line`, the commits behind it and the decisions of the sessions that wrote them), `check_decision` (surface settled decisions before re-litigating a proposal), `linked_commits` (the commits a thread likely produced), `recurring_issues` (errors you keep hitting across sessions), `list_snapshots` (a project's session snapshots), and `load_snapshot` (restore a saved checkpoint). Writes (4, into Callimachus's own memory, never your code): `complete_todo` (close an open TODO), `record_decision` (optionally with a `rationale`), `record_gotcha` (persist a fact into a project's memory), and `snapshot_session` (checkpoint a thread for handoff). The bundled `/recall` skill ([resources/recall/SKILL.md](apps/desktop/src-tauri/resources/recall/SKILL.md)) tells agents when to reach for them.

Results are compact JSON sized for an agent's context. The search tools return one entry per thread with its matching `snippets`, `get_thread` and `load_snapshot` take an optional `budget` (default 24,000 chars, max 96,000), and `project_memory` returns size-budgeted markdown by default (`format: "json"` for data, plus `limit` and `kinds`). Until the app has indexed something, every read tool answers with setup instructions instead of an empty list. The server never upgrades the index itself: when its version and the index's schema disagree, every tool says to open the app once (the index needs an upgrade) or to update `callimachus-mcp` or the app (the index is newer). Memory that is not distilled yet still answers: when nothing matches in a scope that isn't fully distilled, `recall_decisions`, `recall_gotchas` and `check_decision` return the distillation `coverage` and up to three `closestSessions` by search, `project_memory` adds the repo's recent sessions while it has no decisions or gotchas, and a search with zero hits says which sources the index holds, when it last indexed, and its blind spots (cloud sessions, Cursor's missing project paths).

Every tool also declares an `outputSchema` and returns `structuredContent` that matches it, which is what Claude Code and Codex hand the model: a list sits under a key (`{"threads": [...]}`, `{"facts": [...]}`), markdown under `markdown`, and an answer that is guidance instead of results (no index yet, a version skew, a busy write, no embedding model) is `{"notice": "..."}`, which every tool's schema allows. The text block is unchanged for every client; clients older than MCP 2025-06-18 get no structured content. The output schemas are checked in under [`schemas/mcp/`](schemas/mcp/) (served at `https://callimachus.app/schemas/mcp/<tool>.json`) and a test fails when one drifts. `get_thread` and `load_snapshot` carry the transcript in both the text and `markdown`, so their response is about twice `budget`.

The server also offers three MCP prompts, `recall` (a `topic`), `check_decision` (a `proposal`) and `prior_work` (a `task`), which spell out the tool order of the `/recall` skill; Claude Code lists them as slash commands such as `/mcp__callimachus__recall`. Project memory is also a resource: `callimachus://memory` for the repo the server runs in, `callimachus://memory/{project}` for any other (a path, a project key, or `current`).

**CLI**: `cal`, pipe-friendly. The desktop app installs it with **Settings → Agents → Enable for Claude Code**: on macOS and Linux it is linked at `~/.local/bin/cal` (if `~/.local/bin` isn't on your shell's `PATH`, as on stock macOS, Settings shows the `export PATH=...` line to add), and on Windows it is placed as `cal.exe` next to the app. If another program already owns `~/.local/bin/cal`, or a per-machine Windows install can't write `cal.exe`, Callimachus leaves `cal` out and says so in Settings; the hooks, MCP server and `/recall` skill work without it. You can also run `npx callimachus-mcp --install-cal` (macOS and Linux), which downloads the release's `cal` and links it at `~/.local/bin/cal` (never over another program's `cal`), download the standalone `cal-<target>` binary from [Releases](../../releases/latest), or build from a checkout with `cargo install --locked --path apps/desktop/src-tauri/standalone --bin cal` (add `--force` over a source install from an earlier version).

```bash
cal search "vector index migration" -y    # -y = hybrid (semantic + keyword)
cal recent -n 10
cal cat 42 | pbcopy                        # packed transcript → clipboard
cal stats                                  # index totals, per-source breakdown, times helped this week
cal export 42 --vault ~/Obsidian           # write a thread as an Obsidian note
cal ask "how did we set up releases?"      # cited RAG answer over your history
cal files embed/mod.rs                     # threads that touched a file path
cal memory                                 # this repo's distilled memory (decisions/gotchas/TODOs)
cal memory publish                         # write its pinned + recorded facts to .callimachus/memory/ to commit (--all: distilled too)
cal memory import --review                 # what the repo's committed memory would change here (drop --review to import now)
cal done 17                                # mark an open TODO done (id from `cal todos`)
cal remember decision "use sqlite-vec for KNN" --because "no cloud, KNN in SQL"  # record a fact (+ rationale) into the repo's memory
cal check "switch to pgvector"             # surface settled decisions before re-litigating one
cal commits                                # infer the thread→commit timeline for this repo (--json; or pass a path)
cal why src/auth.rs:42                     # why a line is the way it is: blame → commits → the sessions behind them + their decisions
cal issues                                 # recurring errors you keep hitting across sessions (last 180 days), with the commit that fixed each
cal cost                                   # estimated $ spend by model + your priciest threads
cal cost --by commit --since 30            # what the sessions behind each recent commit cost (also thread, project, day)
cal audit-pr . --changed-files a,b --shas s1,s2  # one JSON bundle for an external PR auditor
cal snapshot 42 -l "pre-refactor"          # checkpoint a thread for handoff (transcript + project memory)
cal snapshots                              # list saved session snapshots (optionally for a project)
cal resume 7 -a claude                     # resume a snapshot in an agent CLI
cal agents                                 # write the repo's memory into AGENTS.md (any agent reads it)
cal hook                                   # print the repo's memory (use as a Claude Code SessionStart hook)
cal hook --explain                         # what a new session in this repo would get, or why nothing
cal hook --explain --for codex             # the same for a Codex session (also --for gemini)
```

`star`, `tag`, `tags`, `todos`, `knowledge`, `distill`, `decisions`, `gotchas`, `related`, and `similar` (the "have I done this before?" check) also exist. Run `cal help` for the full list.

**`--json-version 1` is the versioned contract** for scripts and tools (the VS Code extension asks for it). With it (or `CAL_JSON_VERSION=1` in the environment beside `--json`), the outputs of `search`, `recent`, `related`, `cat`, `stats`, `memory`, `decisions`, `gotchas`, `commits`, `why`, `cost`, `issues` and `audit-pr` open with `schema_version` (now `1`); a list comes as `{"schema_version": 1, "results": [...]}` and an object keeps its fields next to the version. When `decisions` or `gotchas` can't run (no embedding model on the machine), the list is empty and a `notice` says why. Each versioned output has a JSON Schema at `https://callimachus.app/schemas/cal/<command>.v1.json` (in the repo, [`schemas/cal/`](schemas/cal/)), generated from the Rust types: a test fails when the two drift. A new field can appear within a version, so ignore fields you don't know; removing or renaming one, or changing its type, bumps the version, and a reader should refuse a version newer than it was written for. Plain `--json` still prints the bare arrays and objects it always has, so existing scripts keep working, but that shape is unversioned and may change in a future major release.

**Without the app** (a server, a CI box, with the standalone `cal` above): `cal` runs the app's background jobs itself, against the same index and with the same rules.

```bash
cal index                  # one pass over every agent's history (creates the index; exit 1 on errors)
cal index --if-stale       # that pass in the background, only when no full pass finished in the last hour (a hook)
cal watch                  # keep it fresh as agents write: the app's watcher, one line per batch
cal embed                  # build the semantic index (downloads the ~128 MB model once)
cal embed --no-download    # the same, only with a model already on this machine
cal distill --auto         # one drain of the auto-distill worklist under the daily cap
```

`cal index` and `cal watch` build and refresh the index with no app at all. `cal distill --auto` (and the drain `cal watch` runs when auto-distill is on) needs memory turned on with an engine, which only the app's **Settings → AI engine** does today. Once that is done it drains with the app closed: a logged-in `claude` or `codex` CLI and local Ollama need nothing more, and an API key is read from the OS keychain where the app saved it. On a machine that never ran the app, `cal distill --auto` says memory is off.

Only one process watches an index at a time: the app holds `index.db.gui.lock` next to the index while it runs and `cal watch` holds `index.db.watch.lock`. `cal watch` refuses to start while the app runs, and the app keeps its own watcher off while `cal watch` runs (Settings → Index says so) and takes over within a minute after it stops (a second app open on the same index waits for the first the same way). `cal distill --auto` refuses while either runs, since both distill on their own, and the app starts no auto-distill round while `cal watch` or `cal distill --auto` runs. `cal index` and `cal embed` are safe alongside either; `cal index` holds `index.db.index.lock`, so two passes never overlap. `cal index --if-stale`, the Claude Code plugin's SessionStart hook, starts a pass only when no app, `cal watch` or other pass runs, the index exists, is not empty and needs no upgrade, and no full pass finished in the last hour; it appends the pass's output to `index.db.index.log` and, like every hook, prints nothing and never creates the index and upgrades it only with the launcher's `cal` while no app runs the index. These four may create and upgrade the index; `cal embed` without `--no-download` is the only `cal` command that downloads the model, and hooks and the MCP server still never do. `cal distill --auto` holds `index.db.distill.lock` and needs auto-distill on in Settings → AI engine. `cal index` and `cal embed` do not yield to the app's Shrink index or Reclaim space, and the app refuses those two while `cal watch`, `cal distill --auto` or a background `cal index` runs; rerun the command afterwards if one interrupted it.

**VS Code / Cursor**: the extension adds a "Callimachus History" sidebar, a status-bar search button, commands to search / insert / copy threads, and **Why This Line** (editor context menu) for the sessions behind the line under the cursor (it shells out to `cal`). Install from the **[VS Code Marketplace](https://marketplace.visualstudio.com/items?itemName=BetaBotsLLC.callimachus-vscode)** or **[Open VSX](https://open-vsx.org/extension/BetaBotsLLC/callimachus-vscode)** (the registry **Cursor** and VSCodium use), or grab the `.vsix` from [Releases](../../releases). See [apps/vscode/README.md](apps/vscode/README.md).

## Notes / limitations

- Cross-platform (macOS / Windows / Linux), including the "Open in CLI" / "Resume" launchers. They open Terminal on macOS, a new console window on Windows, and on Linux the first terminal they find (`$TERMINAL`, then `x-terminal-emulator`, GNOME Terminal, Konsole, xfce4-terminal, kitty, Alacritty, WezTerm, xterm). On a Linux machine with none of these, the buttons are hidden.
- Cline / Roo Code / Kilo Code are editor extensions with no CLI, so they are index-only (searchable, but not relaunchable via "Resume").
- Claude Code subagent transcripts are indexed but hidden behind a "subagents" toggle by default.
- Moved your Claude Code, Codex or Copilot CLI data (`CLAUDE_CONFIG_DIR` / `CODEX_HOME` / `COPILOT_HOME`)? An app opened from the Dock, Finder or Start menu doesn't see shell variables, so set those folders under **Settings → Sources** (changing one reindexes that source).
- Large first index is a one-time cost (the Claude corpus here was ~90k messages in ~25s); subsequent passes skip unchanged files.
- The index (`index.db`), the embedding model (`models/`) and the app's diagnostics log (`callimachus.log`) live together in the per-user app data folder `dev.shaller.callimachus` (`~/Library/Application Support` on macOS, `%LOCALAPPDATA%` on Windows, `~/.local/share` on Linux). Attach `callimachus.log` to a bug report.
- More sources (Charm Crush, Factory Droid) are scoped but not yet integrated. See [the indexer guide](apps/desktop/src-tauri/core/src/indexer/README.md).

## Contributing

Issues and PRs welcome. [CONTRIBUTING.md](CONTRIBUTING.md) covers local setup, conventions, and the release flow. Adding support for another agent is a [small, documented contract](apps/desktop/src-tauri/core/src/indexer/README.md), usually one indexer module + a migration + a few wiring points.

## Security & privacy

Callimachus is local-first. Your index, transcripts and memory never leave your machine, except conversation content you explicitly send to an LLM provider you choose (the in-app chat, Knowledge distillation, Ask your history, project briefs and conflict review, and AI-summarized Obsidian exports; local Ollama keeps even that on-device) and the excerpts that agents you connect over MCP or hooks pull into their own sessions. API keys live in the OS keychain, never on disk.

The desktop app can send anonymous, **opt-in** usage analytics to PostHog (US): event names, app version, OS, CPU architecture and language, and a few properties picked from fixed lists (such as which agent source, AI provider or setting an event concerns, an error category, or a count rounded into a range), tied to a random install ID. It never sends file paths, thread titles, prompts, transcripts, project names or API keys. It is off until you choose **Allow**, and you can turn it off any time in **Settings → General** or with `CALLIMACHUS_TELEMETRY=0` / `DO_NOT_TRACK=1`. The CLI and MCP server never send analytics. The only other network traffic is the update check against GitHub Releases and the embedding-model download from Hugging Face (desktop app only, the first time you build the semantic index). The website uses PostHog in cookieless mode (no cookies, no cross-site tracking).

The full breakdown is in [SECURITY.md](SECURITY.md) and the [privacy policy](https://callimachus.app/privacy). To report a vulnerability, see [SECURITY.md](SECURITY.md).

## License

**Dual-licensed.** Free and open source under **[AGPL-3.0-or-later](LICENSE)**: use,
modify, and share it; if you distribute it or run a modified version as a network
service, you must release your source under the AGPL.

For **closed-source, proprietary, or for-profit/SaaS use** that the AGPL doesn't
permit, a **commercial license** is required. See **[COMMERCIAL.md](COMMERCIAL.md)**
(contact ari@shaller.dev).

© 2026 Ari Shaller. See [NOTICE](NOTICE) for attributions. Contributions are accepted
under the terms in [CONTRIBUTING.md](CONTRIBUTING.md).

## Acknowledgements

Built on [Tauri](https://tauri.app), [fastembed-rs](https://github.com/Anush008/fastembed-rs), [sqlite-vec](https://github.com/asg017/sqlite-vec), and [genai](https://github.com/jeremychone/rust-genai). Named for [Callimachus of Cyrene](https://en.wikipedia.org/wiki/Callimachus), who catalogued the Library of Alexandria.

<p align="center"><sub>Social preview: <a href="assets/og.png"><code>assets/og.png</code></a> · brand sources in <a href="assets/brand"><code>assets/brand/</code></a></sub></p>
