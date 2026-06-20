---
"callimachus": patch
---

**Incremental indexing + indexer reliability.**

- **Incremental re-index.** Re-indexing a thread used to delete and re-insert (and re-FTS) every message, so an actively-growing session got progressively more expensive to keep fresh, both on manual re-index and on every file-watcher save. Now, when the stored messages are an exact prefix of the new parse, only the new tail is inserted; any mismatch or shrink falls back to a correct full replace. Heuristic TODOs and file mentions are preserved on append (with their per-thread caps still enforced against the thread total), and LLM-distilled knowledge is still invalidated when content changes (including same-length in-place edits).
- **No more silently dropped threads.** The single-DB sources (Cursor, Goose) and OpenCode recorded their `index_state`/fingerprint *before* the upserts succeeded, so a thread that failed mid-pass on a transient write-lock could be marked "done" and skipped on the watcher's retry. State is now recorded only after the work succeeds.
- **Correct source labels.** Roo and Kilo task files are now recorded in `index_state` under their own source kind instead of `cline`.
