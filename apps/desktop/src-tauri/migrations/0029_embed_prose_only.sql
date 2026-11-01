-- 0029: embed user/assistant prose only, and repair corpora the old embed job marked
-- embedded without vectors.
--
-- The embedder now selects `role IN ('user','assistant') AND tool_name IS NULL`
-- (embed::EMBEDDABLE_SQL): tool calls are JSON, and their vectors crowded real prose out of
-- semantic results.

-- 0. Re-index FTS only when `text` changes. 0001's update trigger fired on EVERY update,
--    so flipping the `embedded` flag (each embed batch, the re-queues below) deleted and
--    re-tokenized the whole message in FTS for nothing.
DROP TRIGGER IF EXISTS messages_au;
CREATE TRIGGER messages_au AFTER UPDATE OF text ON messages BEGIN
    INSERT INTO messages_fts (messages_fts, rowid, text) VALUES ('delete', old.id, old.text);
    INSERT INTO messages_fts (rowid, text) VALUES (new.id, new.text);
END;

-- 1. Drop vectors older builds stored for non-prose rows. Through the owner map (0025), so
--    each vector goes by rowid and no owner row is left pointing at a deleted vector...
DELETE FROM vec_chunk_owner WHERE message_id IN (
    SELECT id FROM messages WHERE tool_name IS NOT NULL OR role NOT IN ('user','assistant')
);
-- ...then the same messages' vectors that have no owner row (written by a build without
-- the map), with one metadata scan of the vector table.
DELETE FROM vec_chunks WHERE message_id IN (
    SELECT id FROM messages WHERE tool_name IS NOT NULL OR role NOT IN ('user','assistant')
);

-- ...and clear those rows' embedded flag: they are never embedded again, and a flagged row
-- with no owner rows would otherwise look like a legacy unmapped vector to the delete
-- triggers (0025). Driven by an id subquery so the scan reads the covering
-- (role, tool_name, embedded) index from 0026 instead of every message body.
UPDATE messages SET embedded = 0 WHERE id IN (
    SELECT m.id FROM messages m
    WHERE m.embedded = 1 AND NOT (m.role IN ('user','assistant') AND m.tool_name IS NULL)
);

-- 2. Re-queue prose rows flagged embedded with no vector. When the embedding model failed
--    to load (offline first run, an unwritable CWD-relative cache), the old job marked
--    whole batches embedded anyway, which silently disabled semantic search for them.
--    One-time on purpose: genuinely failing inputs are not retried on every launch.
UPDATE messages SET embedded = 0
WHERE embedded = 1 AND role IN ('user','assistant') AND tool_name IS NULL
  AND id NOT IN (SELECT message_id FROM vec_chunks);

-- No new index: 0026's covering (role, tool_name, embedded) index already serves the
-- embedder's pending queue and both progress counts with exactly this predicate.
