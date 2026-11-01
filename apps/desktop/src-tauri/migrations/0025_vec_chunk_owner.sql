-- 0025 — O(1) vector cleanup when a message is deleted.
--
-- 0004's trigger deleted vectors with `DELETE FROM vec_chunks WHERE message_id = old.id`.
-- `message_id` is a vec0 METADATA column, and sqlite-vec only plans KNN (MATCH) or point
-- (`rowid =`) lookups; every other filter is a full scan of the whole vector table. The
-- trigger fires once per deleted message, so a thread delete or a full re-index of a long
-- thread scanned every vector once per message while holding the write lock (~0.2s per
-- message at 200k chunks: minutes for a bulk delete).
--
-- Fix: a plain b-tree side table maps each chunk rowid to its owning message. Deleting a
-- message deletes its owner rows through an index probe, and each owner row deletes its
-- vector by rowid, which vec0 serves as a point lookup. The embedder writes one owner row
-- per chunk it inserts (embed::store_batch); cleanup::reconcile_vec_owners repairs any
-- vectors written without one, and a fallback trigger (below) still cleans those up the
-- old, slow way so a missing owner row can never leave an orphaned vector behind.

CREATE TABLE vec_chunk_owner (
    chunk_rowid INTEGER PRIMARY KEY,  -- = vec_chunks.rowid
    message_id  INTEGER NOT NULL
);
CREATE INDEX idx_vec_chunk_owner_message ON vec_chunk_owner (message_id);

-- Backfill: one full vector scan now, instead of one per deleted message from here on.
INSERT INTO vec_chunk_owner (chunk_rowid, message_id)
SELECT rowid, message_id FROM vec_chunks;

-- Removing an owner row removes its vector (vec0 point delete by rowid). Note: `rowid IN
-- (...)` would NOT work here; vec0 only uses IN on rowid inside KNN queries and full-scans
-- otherwise, so this must stay a per-row equality.
CREATE TRIGGER vec_chunk_owner_ad AFTER DELETE ON vec_chunk_owner BEGIN
    DELETE FROM vec_chunks WHERE rowid = old.chunk_rowid;
END;

-- Replace the full-scan trigger. Only embedded messages can own vectors (store_batch sets
-- the flag in the same transaction that inserts them, and nothing stores vectors for a row
-- with embedded = 0), so rows that were never embedded (tool output, most history) skip
-- even the index probe.
DROP TRIGGER IF EXISTS messages_vec_ad;
CREATE TRIGGER messages_vec_ad AFTER DELETE ON messages WHEN old.embedded = 1 BEGIN
    DELETE FROM vec_chunk_owner WHERE message_id = old.id;
END;

-- Fallback for an embedded message whose vectors have no owner rows (written by a build
-- without the owner map): the legacy metadata scan, but only for those messages. The
-- guard must be a trigger WHEN clause; as a WHERE term SQLite re-evaluates it per scanned
-- vector instead of skipping the scan. Created AFTER messages_vec_ad on purpose: SQLite
-- runs a table's triggers newest-first, so this guard is checked before messages_vec_ad
-- deletes the owner rows (the migration tests pin this with a VM step count).
-- Prose only (embed::EMBEDDABLE_SQL): older builds flagged tool-call rows embedded too,
-- and 0029 drops those vectors. Without the role guard every delete of such a row would
-- pay a full vector scan for vectors that no longer exist.
CREATE TRIGGER messages_vec_ad_unmapped AFTER DELETE ON messages
WHEN old.embedded = 1
 AND old.role IN ('user','assistant') AND old.tool_name IS NULL
 AND NOT EXISTS (SELECT 1 FROM vec_chunk_owner WHERE message_id = old.id)
BEGIN
    DELETE FROM vec_chunks WHERE message_id = old.id;
END;
