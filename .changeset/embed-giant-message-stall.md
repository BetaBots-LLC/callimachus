---
"callimachus": patch
---

Fix the semantic-index build appearing to get stuck, and the Build-memory button silently doing nothing.

- **Giant messages no longer stall embedding.** A pasted log of a few hundred KB was chunked into hundreds of vectors (a 600 KB message became ~430 chunks), so any batch containing one crawled and looked frozen. Chunks per message are now capped (the first 16 capture the semantic gist; FTS still searches the full text), which also shrinks the vector index.
- **A failed batch is skipped, not fatal.** If the embedder errors on a batch, those messages are marked done (still FTS-searchable) and the job continues, instead of the whole build stopping at that point.
- **Build memory now shows why it is blocked.** Distillation shares the write lock with the embedding build and reindex, so it is mutually exclusive with them; the Build-memory button now disables and shows "Embedding..." / "Indexing..." instead of silently no-op'ing while one of those runs.
