---
"callimachus": patch
---

**Search quality + distillation cost fixes** (from the app audit).

- **Hybrid search now respects project scope and filters noise.** The semantic arm previously ignored the project filter entirely (a project-scoped hybrid search leaked cross-project hits) and applied no relevance floor (a query with no good match still injected its nearest neighbors). It now scopes by `COALESCE(project_key, project_path)` like the rest of the app, and drops sub-threshold cosine matches. The keyword arm's project filter was aligned to the same `COALESCE` scoping.
- **Keyword search recall is much higher.** Full-text queries were built as a strict AND of exact-phrase tokens, so a multi-word natural-language query only matched messages containing every term verbatim. Tokens are now prefix-matched (`embed` matches `embedder`/`embedding`), and a strict-AND pass is backfilled with a looser OR pass when it under-fills — precise hits still rank first.
- **No more wasted re-distills.** Distillation staleness keyed off total `message_count`, which includes tool/system rows; agent transcripts grow mostly via tool output, so threads kept flipping "stale" and re-running paid LLM distillation that produced identical results. Staleness now keys off a stored `distillable_count` (user/assistant messages only). A migration backfills it and keeps already-distilled threads from re-running.
