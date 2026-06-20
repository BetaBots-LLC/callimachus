---
name: recall
description: Search the user's own past AI coding-agent conversations (across Claude Code, Codex, Cursor, Gemini, Qwen, Goose, OpenCode, Continue, Cline, and in-app chats) before redoing work. Use when the user references something they "did before", "talked about", "figured out earlier", asks "how did we…/what did we decide about…", mentions a past session, or when starting a task that may have prior context. Powered by Callimachus.
version: 1.0.0
user-invocable: true
argument-hint: "[what to recall]"
---

Recall the user's prior work from their **Callimachus** index — one local, searchable store of every AI coding-agent conversation they've had (Claude Code, Codex, Cursor, Gemini, Qwen, Goose, OpenCode, Continue, Cline, and in-app chats). Use it to avoid re-deciding, re-debugging, or re-deriving something already worked out — and to write back what you settle or discover so it's there next time.

## When to reach for this

- The user says "like we did before", "what did we decide about X", "didn't we already solve this", "remember when…", or names a past session.
- You're starting a task in a repo where there's likely earlier context (design choices, gotchas, failed approaches). Load the project's memory first.
- You hit a problem that smells previously-solved.

## How to query

**Prefer the MCP tools** if the `callimachus` MCP server is connected. The server exposes 15 tools — 12 read, 3 write.

**Read:** `search_threads`, `search_current_project`, `recent_threads`, `get_thread`, `list_tags`, `list_open_todos`, `get_thread_knowledge`, `recall_decisions`, `recall_gotchas`, `find_prior_work`, `project_memory`, `ask_history`, `threads_for_file`.
**Write (to Callimachus's own memory only):** `complete_todo`, `record_decision`, `record_gotcha`.

1. **Load the project's memory FIRST** -- `project_memory(project?)` returns a project's durable memory: the decisions, gotchas, and open TODOs distilled across ALL past sessions on it, with coverage counts. Omit `project` to use the repo the server runs in. Call this at the START of work on a repo to recall what was already decided and what to watch out for.
2. **Scope to the current repo** -- `search_current_project(query, hybrid?)`. It auto-limits to the git repo you're running in. Best signal when the user means "this project".
3. **Widen if needed** -- `search_threads(query, sources?, hybrid?, limit?)` searches everything. Set `hybrid: true` for fuzzy / conceptual recall (semantic + keyword); leave it off for exact terms.
4. **See what's recent** -- `recent_threads(limit?, project?)` when the user means "the thing I was just working on".
5. **Ask a question over history (cited RAG)** -- `ask_history(question)` retrieves the most relevant threads and returns a synthesized answer with `[thread N]` citations plus the source list. Use for "how did we…" / "what did I decide about…" instead of reading many threads yourself.
6. **Recall past decisions BEFORE re-deciding** -- `recall_decisions(query, project?, limit?)` does cross-thread semantic recall of concrete decisions the user already made (and why). Call it before settling anything the user may have settled already.
7. **Recall known gotchas** -- `recall_gotchas(query, project?, limit?)` surfaces pitfalls and non-obvious constraints the user hit before, so you don't repeat a known mistake.
8. **Check for prior work BEFORE starting a task** -- `find_prior_work(query, project?, limit?)` returns past SESSIONS where the user did something similar (each with its most-relevant decision/gotcha and threadId). Use it at the start of a task to reuse an earlier solution instead of redoing it. Searches all projects unless `project` is given.
8. **Find which sessions touched a file** -- `threads_for_file(path)` returns the past sessions that mentioned a file path (e.g. `embed/mod.rs`). Handy before editing a file to pull up its prior history.
9. **Get a high-signal recap of one thread** -- `get_thread_knowledge(thread_id)` returns a short summary plus key decisions, gotchas, and open TODOs for that thread. Prefer it over reading the full transcript when you just need the gist.
10. **Optionally orient with tags / TODOs** -- `list_tags()` lists the user's topic labels (collections) with counts; pass one to `recent_threads` to filter. `list_open_todos(project?, source?, limit?)` lists unfinished action items left across sessions.
11. **Read the full match** -- `get_thread(thread_id)` returns the full thread as packed markdown. Pull the one or two `threadId`s that look right, read them, then use them as context.

**Write back what you settle or discover** (persists into Callimachus's memory; pinned + embedded so it surfaces in future recall):

- `record_decision(text, project?)` -- persist a technical choice you/the user just settled. Omit `project` to use the repo the server runs in.
- `record_gotcha(text, project?)` -- persist a pitfall / non-obvious constraint just discovered, same way.
- `complete_todo(id)` -- mark a tracked TODO done (the `id` comes from `list_open_todos`) so it drops out of the open-TODO lists. The completion persists across re-indexing.

**Fallback: the `cal` CLI** (if MCP isn't wired but the app is installed):

```bash
cal search "vector index migration" -y     # -y = hybrid (semantic + keyword)
cal recent -n 10                            # most recent threads
cal cat 42                                  # full thread -> stdout (pipe/quote it)
cal decisions "auth flow"                   # recall past decisions (semantic)
cal gotchas "rate limiting"                 # recall known gotchas/pitfalls
cal similar "add stripe webhooks"           # prior sessions like this task
cal knowledge 42                            # distilled recap of one thread
cal memory [project]                        # a project's durable memory (decisions/gotchas/TODOs)
cal ask "how did we wire the embedder?"     # cited RAG answer over your history
cal files embed/mod.rs                      # which sessions touched a file path
cal done <todo-id>                          # mark an open TODO done (id from `cal todos --json`)
cal remember decision "use sqlite-vec for KNN"   # persist a decision (or: remember gotcha "...")
cal agents                                  # write the repo's memory into AGENTS.md (any agent reads it)
cal hook                                    # print the repo's memory (Claude Code SessionStart hook)
```

The repo's `AGENTS.md` / `CLAUDE.md` may already contain a Callimachus-managed memory block (it's kept fresh by `cal agents` / a SessionStart hook), so prior decisions and gotchas can already be in your context before you call any tool.

## Workflow

1. At the start of repo work, call `project_memory` (or `cal memory`) to load prior decisions/gotchas/open TODOs.
2. Turn the user's ask into 1–3 focused queries (keywords > sentences).
3. Search (repo-scoped first, then global) or `ask_history` for a cited answer. Skim titles + snippets.
4. Fetch the best 1–2 threads in full (`get_thread` / `cal cat`).
5. Summarize what was decided/tried back to the user, cite the thread, and continue the task with that context — don't silently re-litigate settled decisions.
6. As you go, write back: `record_decision` / `record_gotcha` for things you settle or discover, and `complete_todo` when you finish a tracked TODO.

## Notes

- It only ever reads/writes **Callimachus's own** index + memory (search history, mark TODOs done, record decisions/gotchas). It never edits the user's files or runs commands in their projects.
- If nothing relevant turns up, say so plainly and proceed fresh -- don't invent prior context.
- The index updates in the background, but very recent sessions may not be embedded yet; a keyword (non-hybrid) search still finds them.
- Decisions, gotchas, per-thread knowledge recap, `project_memory`, and `ask_history` come from **opt-in LLM distillation** (needs local Ollama or a cloud API key). If the user hasn't distilled their threads, `recall_decisions`, `recall_gotchas`, the decisions/gotchas/summary fields of `get_thread_knowledge` and `project_memory` come back empty, and `ask_history` has nothing to synthesize from. Empty results mean "not distilled," not "no prior decisions" -- fall back to `search_threads` / `get_thread` before concluding there's no prior context. (Open TODOs are extracted heuristically and work with no API key.)
