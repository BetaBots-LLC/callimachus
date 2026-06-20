---
"callimachus": minor
---

**Duplicate-work guard — "have I done this before?"** Describe a task and Callimachus surfaces the past *sessions* where you (or your agent) solved something similar, each rolled up to its most-relevant decision or gotcha so you can reuse the earlier solution instead of redoing or re-deciding it.

- **For your agent**: a new `find_prior_work` MCP tool (searches all projects unless scoped), meant to be called at the start of a task. The bundled `/recall` skill now tells agents to reach for it.
- **CLI**: `cal similar <task…>`.
- **In the app**: a "Have you done this before?" search on the Coach tab — results link straight to the source thread.

Built on the existing semantic recall over distilled decisions/gotchas, grouped by thread. Needs distillation enabled to return results.
