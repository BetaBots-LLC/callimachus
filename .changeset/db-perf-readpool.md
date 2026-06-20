---
"callimachus": minor
---

**Database performance + scalability overhaul** (from a full DB audit).

- **Read-pool architecture.** UI read commands now run on a pool of read-only connections instead of serializing behind the single writer mutex. WAL allows unlimited concurrent readers, so searches, lists, recall, Ask, and Project Memory no longer queue behind each other or behind a write. The shared `Mutex<Connection>` is now the single writer only.
- **Code-aware search uses an index.** `file:` search and `cal files` now match via a trigram FTS over file paths instead of a full-table `LIKE '%x%'` scan, and build every result row in one join (no per-row round-trip).
- **Project Memory uses indexes.** Aggregation now matches the project path exactly and is backed by a new `facts(thread_id, kind)` index, instead of scanning a whole fact partition per open.
- **New list index.** `idx_threads_subagent_updated` removes the temp-sort on every Recent / Projects / pending-distill list.
- **Pragma tuning.** 64 MiB page cache, memory-mapped reads, in-memory temp store, and bounded WAL (autocheckpoint + size limit) on the writer; lighter read-only pragmas on pooled connections. A passive WAL checkpoint now runs at the end of reindex and the embedding build so the WAL file does not grow unbounded.
- **VACUUM no longer freezes the UI.** It runs on a dedicated background connection instead of holding the shared mutex for the whole file rewrite.

Bug fixes surfaced by the audit:
- The file watcher (a second writer) now retries a lost write-lock race instead of silently dropping a newly indexed thread.
- `cal star`, `cal tag`, and `cal distill` now open a writable connection. They previously failed with SQLITE_READONLY.
