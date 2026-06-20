---
"callimachus": minor
---

**Project Memory.** Aggregate the knowledge distilled across all of a project's threads into one durable memory: the decisions, gotchas, and open TODOs for that codebase, readable in the app, by agents, and from the CLI.

- **Projects tab** (desktop): pick a project, see its aggregated decisions/gotchas/open-todos with links back to the source threads, plus a distillation-coverage chip.
- **Build memory**: a background, cancellable, project-scoped distill that fills in every not-yet-distilled thread in the project (per-thread progress bar), mutually exclusive with reindex and the embedding build so the writers never collide.
- **Synthesize brief**: an optional LLM summary of the project's memory ("what this is + key decisions"), and **Write memory file** drops a `.callimachus/memory.md` agents can be pointed at.
- **MCP `project_memory` tool**: hands an agent its repo's accumulated memory (defaults to the current git repo) so it can recall what was decided at the start of a session.
- **`cal memory [project]`**: the same memory from the CLI (defaults to the current repo).
- **Open in CLI** now prepends the project's memory to the seeded context, so a relaunched session opens with what was already decided, not just one thread's transcript.
