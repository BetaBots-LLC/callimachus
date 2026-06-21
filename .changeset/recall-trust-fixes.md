---
"callimachus": patch
---

**Trustworthy recall.** A code audit surfaced two correctness bugs in cross-thread recall that could make the memory layer confidently wrong; both are fixed.

- **Similarity floor.** Semantic recall (`recall_decisions` / `recall_gotchas` / `cal decisions|gotchas`) and the `find_prior_work` / `cal similar` guard ran a pure k-nearest-neighbor search with no relevance threshold, so a query with no real match still returned its nearest (irrelevant) neighbors — the "have I done this before?" guard could fabricate prior work that didn't exist. Recall now drops neighbors below a cosine floor and returns an explicit empty result; the prior-work guard holds to a stricter floor since an agent acts on it.
- **Project scoping.** Project-scoped recall filtered on `project_path` while facts are written and aggregated by `COALESCE(project_key, project_path)`, so the canonical-key threads (the whole point of the project-key backfill) silently dropped out of scoped results. Recall now scopes the same way writes do.
