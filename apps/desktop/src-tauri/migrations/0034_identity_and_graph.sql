-- 0034: fact identity, and the graph tables the phase 2 features read (cal why, cost per
-- commit, failure playbooks). Every statement touches `facts` (about 2k rows on a 21k-thread
-- index) or creates an empty table, plus one index over `threads`; nothing reads `messages`
-- beyond the point lookups that backfill `source_seq`, so `db::migrate` skips its pre-read.

-- A random UUID per fact: the merge key across machines and repo memory files. Rust insert
-- paths pass `knowledge::new_fact_uid()`; the trigger below fills any insert that does not
-- (test fixtures, an older code path), so the column is non-null for every row, and a
-- second trigger refuses an update that would clear it. ALTER ADD COLUMN takes no
-- non-constant default, hence the trigger rather than a DEFAULT. `random() & 3` rather than
-- `abs(random()) % 4`: abs() of the one i64::MIN draw raises "integer overflow".
ALTER TABLE facts ADD COLUMN uid TEXT;
-- The source message's `seq` in its thread. A full re-index replaces the thread's messages
-- (new ids), which nulls `source_message_id` (0011's ON DELETE SET NULL); the seq survives
-- and `knowledge::rebind_fact_sources` points the fact back at the new row.
ALTER TABLE facts ADD COLUMN source_seq INTEGER;

UPDATE facts SET uid =
    lower(hex(randomblob(4))) || '-' || lower(hex(randomblob(2))) || '-4' ||
    substr(lower(hex(randomblob(2))), 2) || '-' ||
    substr('89ab', 1 + (random() & 3), 1) || substr(lower(hex(randomblob(2))), 2) || '-' ||
    lower(hex(randomblob(6)))
WHERE uid IS NULL;
UPDATE facts SET source_seq = (SELECT seq FROM messages WHERE id = facts.source_message_id)
WHERE source_message_id IS NOT NULL;
CREATE UNIQUE INDEX idx_facts_uid ON facts(uid);

-- An empty uid counts as missing: a second '' would otherwise hit the unique index.
CREATE TRIGGER facts_uid_ai AFTER INSERT ON facts WHEN NEW.uid IS NULL OR NEW.uid = '' BEGIN
    UPDATE facts SET uid =
        lower(hex(randomblob(4))) || '-' || lower(hex(randomblob(2))) || '-4' ||
        substr(lower(hex(randomblob(2))), 2) || '-' ||
        substr('89ab', 1 + (random() & 3), 1) || substr(lower(hex(randomblob(2))), 2) || '-' ||
        lower(hex(randomblob(6)))
    WHERE id = NEW.id;
END;
CREATE TRIGGER facts_uid_bu BEFORE UPDATE OF uid ON facts
WHEN NEW.uid IS NULL OR NEW.uid = '' BEGIN
    SELECT RAISE(ABORT, 'facts.uid cannot be cleared: it is the fact''s identity');
END;

-- A machine-independent name per project key: the normalized `origin` remote
-- (`github.com/a/b`, plus `#<subpath>` below the toplevel), cached by
-- `knowledge::portable_project_id` and refreshed after 24 h. NULL portable_id: no remote.
-- `threads.project_key` stays the canonical local path; nothing is rekeyed.
CREATE TABLE projects (
    key          TEXT PRIMARY KEY,
    remote       TEXT,
    subpath      TEXT,
    portable_id  TEXT,
    refreshed_at INTEGER
);
CREATE INDEX idx_projects_portable ON projects(portable_id) WHERE portable_id IS NOT NULL;

-- Per tool call: which file a thread read, edited or wrote, and when. `file_mentions` stays
-- the per-thread summary of paths named in prose. Paths are relative to the thread's
-- checkout when they fall inside it. `message_id` is provenance without a foreign key: a
-- full re-index deletes the thread's messages, and a cascade would probe this table once per
-- deleted message; the indexer deletes the thread's rows itself (thread delete cascades).
-- It is NULL for a call the backfill read from a Codex rollout that the index never stored.
CREATE TABLE file_edits (
    thread_id  INTEGER NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    message_id INTEGER,
    path       TEXT NOT NULL,
    op         TEXT NOT NULL CHECK (op IN ('read', 'edit', 'write', 'mention')),
    ts         INTEGER
);
CREATE INDEX idx_file_edits_path_ts ON file_edits(path, ts);
CREATE INDEX idx_file_edits_thread ON file_edits(thread_id);
-- History indexed before this migration has no rows, and a Reindex never re-parses an
-- unchanged transcript: flag the one-time fill from the stored messages
-- (`indexer::file_edits::backfill_file_edits_once`, run by the watcher and `cal index`). A
-- new, empty index needs none: every row comes from indexing. One probe of `threads`.
INSERT INTO app_config (key, value)
SELECT 'file_edits.backfill_pending', '1' WHERE EXISTS (SELECT 1 FROM threads);

-- Recurring error clusters, refreshed by `issues::refresh_clusters` (the watcher's minute
-- tick) so readers skip the mining. `resolved_*` are filled by the playbook resolver.
CREATE TABLE issue_clusters (
    signature          TEXT PRIMARY KEY,
    example            TEXT NOT NULL,
    count              INTEGER NOT NULL,
    threads            INTEGER NOT NULL,
    first_seen         INTEGER NOT NULL,
    last_seen          INTEGER NOT NULL,
    last_thread_id     INTEGER,
    resolved_thread_id INTEGER,
    resolved_sha       TEXT,
    refreshed_at       INTEGER NOT NULL
);

-- The watcher links commits for the threads a batch just wrote (`last_indexed_at` at or
-- after the batch start): a range probe instead of a scan of every thread. Also serves the
-- MCP server's MAX(last_indexed_at).
CREATE INDEX idx_threads_last_indexed ON threads(last_indexed_at);
