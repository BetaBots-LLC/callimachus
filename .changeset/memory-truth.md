---
"callimachus": patch
---

Memory that knows when a decision was reversed.

- **Dated, sourced memory lines.** Every line the SessionStart hook, the compaction refresher, the MCP `project_memory` tool and `.callimachus/memory.md` inject now says when its session happened (not when it was distilled) and where it came from, e.g. `(2026-10-03, codex, thread 1234; replaces 2026-10-01)`; the committed `AGENTS.md` / `CLAUDE.md` block shows the date only. Memory is ordered by session time, so a backlog distilled newest first no longer ranks an old decision above its replacement.
- **Replaced decisions are history, not memory.** A decision a later session reversed leaves injected memory and stays as history. `check_decision`, `cal check` and the recall tools return each decision with its date and status (current, disputed, flagged or superseded) and name what replaced a superseded one; `recall_decisions`, `recall_gotchas` and `cal memory` take `include_superseded` / `--include-superseded` to show them. Two decisions that disagree with nothing settled are marked disputed for you to settle.
- **Agents can record a reversal.** `record_decision` / `record_gotcha` take `replaces` (a fact id), and so does `cal remember --replaces ID`. One aimed at a fact you pinned or edited is stored as disputed instead, and so is an agent's sixth replacement in a project within a day, so a misled session cannot empty a project's memory.
- **New MCP tool `flag_fact`** (23 tools now) marks an injected or recalled fact stale or wrong, with a reason, which keeps it out of memory until you review it. It is capped at 10 unreviewed flags per project, reaches only the project the agent works in (or names), and a fact you pinned stays in memory, marked flagged.
- The callimachus.app MCP page lists the new tool and describes flagging.
