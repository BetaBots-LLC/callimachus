-- Move semantic search to sqlite-vec (vec0): KNN runs IN SQL instead of loading
-- every vector into Rust. Embeddings are now CHUNK-level (turn-aware chunking)
-- rather than per-message-truncated, so long messages are fully searchable.
-- Each chunk row maps back to its message via the message_id metadata column.

DROP TABLE IF EXISTS embeddings;

CREATE VIRTUAL TABLE vec_chunks USING vec0(
    message_id integer,
    embedding float[384] distance_metric=cosine
);

-- Keep the vector index in sync when messages are removed (e.g. thread re-index).
CREATE TRIGGER messages_vec_ad AFTER DELETE ON messages BEGIN
    DELETE FROM vec_chunks WHERE message_id = old.id;
END;
