---
"callimachus": patch
---

**Recurring issues tracker: surface errors you keep hitting.** Callimachus now mines your whole indexed history for the same error recurring across sessions and tools, so chronic problems and blind spots become visible. A two-stage scan (an FTS pre-filter for error-ish messages, then a precise per-line extractor) pulls real error signatures, and a normalizer collapses the variable parts (paths, line:col, quoted identifiers, hashes) so the same error groups across runs even with different specifics. Surfaces:

- **Coach dashboard:** a "Recurring errors" card (count, threads spanned, last seen).
- **`cal issues [project]`:** the recurring-error list for the last 180 days, most frequent first (`--json` supported).

Entirely on-device; only top-level sessions are scanned (subagents skipped), and it's the kind of cross-tool pattern only a unified local index can compute.
