---
"callimachus": patch
---

Reindex is now a background job with a per-source progress bar.

- **Non-blocking:** re-indexing your sources runs in the background and reports a per-source progress bar, so the UI stays responsive while it works.
- **No write-lock fights:** the reindex and the semantic-index build are now mutually exclusive; each defers to the other so they never contend for the SQLite write lock.
- **Resilient embedding:** when the embed job hits a locked batch it re-queues that batch and retries instead of aborting the whole job.
