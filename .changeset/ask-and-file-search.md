---
"callimachus": minor
---

Two new ways to search your history.

**Ask your history (RAG).** A new **Ask** tab (and `cal ask <question>`): ask a question in plain language → Callimachus retrieves the most relevant past threads, has your configured LLM answer with inline `[thread N]` citations, and lists the source threads to open. Needs distillation/LLM enabled. (No MCP tool — agents already synthesize from `search_threads` themselves.)

**Code-aware search.** File-path mentions are now extracted from message text at index time (`src/embed/mod.rs`, `package.json`, …) into a `file_mentions` index. Search **`file:embed/mod.rs`** in the search bar to find every thread that touched that file; `cal files <path>` does the same from the CLI. Re-derived each index, so it never goes stale.
