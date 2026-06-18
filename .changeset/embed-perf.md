---
"callimachus": patch
---

Keep the UI responsive while the semantic index builds.

- **Cap inference threads.** The on-device embedding model (fastembed/ONNX) ran with no thread limit, pinning every CPU core for the whole build and starving the UI. It now leaves 2 logical cores free (`available_parallelism() - 2`).
- **Stop holding the DB lock across query inference.** Hybrid/semantic search embedded the query *while holding* the single SQLite connection, which froze every other UI command during a build. The query vector is now computed before the DB lock is taken (new `embed_query` / `semantic_search_vec` / `hybrid_vec` split).
- **Push-based embedding progress.** The UI polled `embedding_status` every 700ms (two locked `COUNT(*)` scans); it now updates from `embed:progress`/`embed:done` events the backend already emitted, with only a slow safety-net refetch. Also disabled `refetchOnWindowFocus` (which fired a ~5-query burst, each serialized behind the one connection) and added a small `staleTime`.
