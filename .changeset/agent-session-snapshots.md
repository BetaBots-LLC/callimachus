---
"callimachus": patch
---

**Agent Session Snapshots — resumable cross-agent handoff, captured automatically.** Save a durable checkpoint of an indexed thread (its packed transcript plus a carry-forward block of the project's distilled decisions, gotchas, and open TODOs) and reload it to continue, across a context-window compaction or across tools (Claude Code -> Codex -> Cursor).

- **Automatic capture (zero-effort):** installing the Claude Code integration now also registers `PreCompact` and `SubagentStop` hooks, so the live session is snapshotted right before its context is compacted and when a subagent finishes — no tool call required. Capture is best-effort and silent (it never breaks the agent loop), and keeps one rolling auto-snapshot per thread so it can't flood the list.
- **`cal` commands:** `cal snapshot <thread-id> [-l LABEL]`, `cal snapshots [project]`, `cal resume <id> [-a AGENT]` (relaunches any agent CLI seeded with the checkpoint).
- **MCP tools:** `snapshot_session`, `list_snapshots`, `load_snapshot` — so an agent can checkpoint and the next agent picks up exactly where it left off.

Backed by a new `snapshots` table (migration 0019). Uninstalling the integration cleanly removes all three hooks.
