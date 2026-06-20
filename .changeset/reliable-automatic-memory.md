---
"callimachus": minor
---

**Reliable + automatic memory.** Two changes that make the project-memory layer trustworthy and self-injecting.

**Canonical project identity.** Threads now carry a normalized `project_key` (computed at index time, backfilled at startup): a repo's git root with symlinks resolved, `~` expanded, and trailing slashes trimmed. Project Memory, scoped recall, write-back, and the picker all group on this key, so the same repo opened via a worktree, a symlink, `~/x` vs `/Users/me/x`, or a subdir no longer fragments into separate, half-empty memories. The `cal memory` / MCP `project_memory` / `cal remember` inputs are canonicalized the same way.

**Automatic memory injection.** Get a project's distilled memory into an agent's context without manual lookup:

- `cal agents [project] [-o FILE]` and a desktop **Update AGENTS.md** button write/refresh a managed memory block (between markers, preserving your own content) in the repo's `AGENTS.md` (or `CLAUDE.md`), so any agent that reads project context opens with the prior decisions and gotchas.
- `cal hook [project]` prints the repo's memory for use as a Claude Code SessionStart hook command (emits nothing when there's no memory).
