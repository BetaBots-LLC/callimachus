---
name: recall
description: Search the user's own past AI coding-agent conversations (across Claude Code, Codex, Cursor, Gemini, Qwen, Goose, OpenCode, Continue, Cline, and in-app chats) before redoing work. Use when the user references something they "did before", "talked about", "figured out earlier", asks "how did we…/what did we decide about…", mentions a past session, or when starting a task that may have prior context. Powered by Callimachus.
version: 1.0.0
user-invocable: true
argument-hint: "[what to recall]"
---

Recall the user's prior work from their **Callimachus** index — one local, searchable store of every AI coding-agent conversation they've had (Claude Code, Codex, Cursor, Gemini, Qwen, Goose, OpenCode, Continue, Cline, and in-app chats). Use it to avoid re-deciding, re-debugging, or re-deriving something already worked out.

## When to reach for this

- The user says "like we did before", "what did we decide about X", "didn't we already solve this", "remember when…", or names a past session.
- You're starting a task in a repo where there's likely earlier context (design choices, gotchas, failed approaches).
- You hit a problem that smells previously-solved.

## How to query

**Prefer the MCP tools** if the `callimachus` MCP server is connected (tools: `search_current_project`, `search_threads`, `recent_threads`, `get_thread`):

1. **Scope to the current repo first** — `search_current_project(query, hybrid?)`. It auto-limits to the git repo you're running in. Best signal when the user means "this project".
2. **Widen if needed** — `search_threads(query, sources?, hybrid?, limit?)` searches everything. Set `hybrid: true` for fuzzy / conceptual recall (semantic + keyword); leave it off for exact terms.
3. **See what's recent** — `recent_threads(limit?, project?)` when the user means "the thing I was just working on".
4. **Read the match** — `get_thread(thread_id)` returns the full thread as packed markdown. Pull the one or two `threadId`s that look right, read them, then use them as context.

**Fallback: the `cal` CLI** (if MCP isn't wired but the app is installed):

```bash
cal search "vector index migration" -y     # -y = hybrid (semantic + keyword)
cal recent -n 10                            # most recent threads
cal cat 42                                  # full thread → stdout (pipe/quote it)
```

## Workflow

1. Turn the user's ask into 1–3 focused queries (keywords > sentences).
2. Search (repo-scoped first, then global). Skim titles + snippets.
3. Fetch the best 1–2 threads in full (`get_thread` / `cal cat`).
4. Summarize what was decided/tried back to the user, cite the thread, and continue the task with that context — don't silently re-litigate settled decisions.

## Notes

- Read-only: this only searches history; it never modifies the user's projects.
- If nothing relevant turns up, say so plainly and proceed fresh — don't invent prior context.
- The index updates in the background, but very recent sessions may not be embedded yet; a keyword (non-hybrid) search still finds them.
