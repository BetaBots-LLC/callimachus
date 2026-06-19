-- 0013 — file-path mentions extracted from message text, for code-aware search:
-- "find every thread that touched embed/mod.rs". Re-derived on every index (delete +
-- rescan), so it never goes stale; cascades when a thread is deleted.
CREATE TABLE file_mentions (
    thread_id INTEGER NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    path      TEXT NOT NULL,
    PRIMARY KEY (thread_id, path)
);

-- "which threads touched <path>" — the search path.
CREATE INDEX idx_file_mentions_path ON file_mentions(path);
