---
"callimachus": minor
---

**Auto-distillation.** A new opt-in setting (Settings, under Knowledge) that distills new and changed threads in the background as they're indexed, so the knowledge surfaces (Ask, cross-thread recall, Project Memory, the `get_thread_knowledge` MCP tool) stay populated without ever clicking "Build memory" or distilling thread-by-thread.

- Drains the corpus in paced batches, skips threads that previously errored, and re-distills threads that changed since their last pass.
- Runs at startup and after each reindex; turning the setting on kicks an immediate drain.
- Background and low-priority: it yields to a user-initiated reindex or semantic-index build (no write-lock contention), and is cancellable.
- Free and on-device with Ollama; with a cloud engine it has a per-thread cost (hence opt-in). A subtle "Distilling knowledge N/M threads" indicator shows in the search header while it runs.
