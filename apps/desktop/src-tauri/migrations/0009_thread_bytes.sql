-- Precompute each thread's text size so the cleanup list doesn't SUM(LENGTH(text))
-- over every message live (that read megabytes and, on the single DB connection,
-- froze the whole Settings page). Maintained by upsert_thread going forward.

ALTER TABLE threads ADD COLUMN bytes INTEGER NOT NULL DEFAULT 0;

-- One-time backfill for already-indexed threads.
UPDATE threads
SET bytes = COALESCE(
    (SELECT SUM(LENGTH(CAST(m.text AS BLOB))) FROM messages m WHERE m.thread_id = threads.id),
    0
);
