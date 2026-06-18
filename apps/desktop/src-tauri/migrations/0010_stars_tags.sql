-- 0010 — user organization: star threads + free-form tags ("collections").
-- `starred` lives on threads but is intentionally NOT written by the indexer's
-- upsert_thread (not in its column list / UPDATE SET), so re-indexing a thread
-- never resets a star. Tags are a separate table keyed by thread_id.

ALTER TABLE threads ADD COLUMN starred INTEGER NOT NULL DEFAULT 0;

CREATE TABLE thread_tags (
    id        INTEGER PRIMARY KEY,
    thread_id INTEGER NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    tag       TEXT NOT NULL,
    added_at  INTEGER NOT NULL,
    UNIQUE (thread_id, tag)
);

-- "threads with tag X" (collection filter) and "tags on thread Y" (thread view).
CREATE INDEX idx_thread_tags_tag ON thread_tags(tag);
CREATE INDEX idx_thread_tags_thread ON thread_tags(thread_id);
