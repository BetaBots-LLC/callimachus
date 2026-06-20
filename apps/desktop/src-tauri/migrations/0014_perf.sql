-- 0014 — performance: composite indexes for the hot list paths, a facts(thread_id,kind)
-- index for project-memory aggregation, and a trigram FTS over file_mentions.path so
-- code-aware search uses an index instead of a full `LIKE '%x%'` scan.

-- Recent / Projects / pending-distill lists all filter is_subagent then ORDER BY
-- updated_at DESC; this composite removes the temp B-tree sort (EXPLAIN-verified).
CREATE INDEX idx_threads_subagent_updated ON threads (is_subagent, updated_at DESC);

-- Project Memory aggregates facts by thread within a project; (thread_id, kind) lets it
-- be driven from the project's thread set instead of scanning a whole kind-partition.
CREATE INDEX idx_facts_thread_kind ON facts (thread_id, kind);

-- Trigram FTS over file_mentions.path → substring path search via MATCH. The b-tree
-- idx_file_mentions_path cannot serve a leading-wildcard LIKE. External-content table
-- mirrors file_mentions by rowid, kept in sync by triggers; backfilled for existing rows.
CREATE VIRTUAL TABLE fm_fts USING fts5(
    path,
    content = 'file_mentions',
    content_rowid = 'rowid',
    tokenize = 'trigram'
);

CREATE TRIGGER fm_fts_ai AFTER INSERT ON file_mentions BEGIN
    INSERT INTO fm_fts (rowid, path) VALUES (new.rowid, new.path);
END;
CREATE TRIGGER fm_fts_ad AFTER DELETE ON file_mentions BEGIN
    INSERT INTO fm_fts (fm_fts, rowid, path) VALUES ('delete', old.rowid, old.path);
END;
CREATE TRIGGER fm_fts_au AFTER UPDATE ON file_mentions BEGIN
    INSERT INTO fm_fts (fm_fts, rowid, path) VALUES ('delete', old.rowid, old.path);
    INSERT INTO fm_fts (rowid, path) VALUES (new.rowid, new.path);
END;

INSERT INTO fm_fts (rowid, path) SELECT rowid, path FROM file_mentions;
