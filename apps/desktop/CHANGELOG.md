# callimachus

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
