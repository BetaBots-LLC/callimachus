-- Local semantic-search vectors. One row per embedded message; `vec` is the raw
-- little-endian f32 array (384 dims for all-MiniLM-L6-v2). Cosine similarity is
-- computed in Rust — at this corpus size a brute-force scan is fast enough and
-- avoids a loadable vector extension.
CREATE TABLE embeddings (
    message_id INTEGER PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    vec        BLOB NOT NULL
);
