-- Track embedding state on the message row so the embedder can find pending work
-- with an index instead of a `NOT EXISTS` scan against the vec0 table (whose
-- metadata columns aren't b-tree indexed). Without this, each batch re-scans a
-- growing share of already-embedded messages — O(total²/batch) over a full build.

ALTER TABLE messages ADD COLUMN embedded INTEGER NOT NULL DEFAULT 0;

-- Backfill: anything that already has a vector chunk is embedded (no re-embed on upgrade).
UPDATE messages SET embedded = 1 WHERE id IN (SELECT DISTINCT message_id FROM vec_chunks);

-- Partial index over ONLY the not-yet-embedded rows: it shrinks toward empty as
-- indexing completes, so "find the next batch" stays cheap to the very end.
CREATE INDEX idx_messages_pending_embed ON messages(role) WHERE embedded = 0;
