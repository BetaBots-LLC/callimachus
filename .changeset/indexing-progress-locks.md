---
"callimachus": patch
---

**Indexing: no more "database is locked", and real progress.**

- **Concurrency fix.** Every write transaction now uses `BEGIN IMMEDIATE` instead of a DEFERRED transaction. With multiple writer connections (the app's shared connection, the reindex's own connection, and the file watcher's), a DEFERRED transaction that read-then-upgrades could hit `SQLITE_BUSY` immediately, bypassing `busy_timeout` — which surfaced as intermittent "database is locked" failures that stalled a reindex. `busy_timeout` was also raised (5s to 15s) so concurrent writers queue instead of erroring.
- **Live, thread-granular progress.** Reindex progress is now reported per thread (not per source), so the bar keeps moving with a running "N scanned" count even while one large source (usually Claude Code) works through thousands of files, instead of sitting at 0%. The total is estimated from the existing thread count (accurate on a re-index, indeterminate on a first run).
- **Consistent DB path.** The desktop app now resolves its index location through the same `CALLIMACHUS_DB`-aware resolver as the indexer, watcher, and sidecars, instead of hardcoding the app-data path. Setting `CALLIMACHUS_DB` to a throwaway path now correctly drives the whole app (handy for exercising a clean first-run).
