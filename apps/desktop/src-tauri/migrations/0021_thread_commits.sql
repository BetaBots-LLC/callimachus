-- 0021 — git linkage: which commits a thread produced. Inferred on-device by overlapping a
-- thread's file_mentions with `git log` changed-files inside the thread's time window, so we can
-- answer "which AI conversation led to this commit?". `overlap` (count of shared files) doubles
-- as a confidence proxy. CASCADE so deleting a thread drops its links.
CREATE TABLE thread_commits (
    thread_id    INTEGER NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    sha          TEXT NOT NULL,
    short_sha    TEXT NOT NULL,
    subject      TEXT,
    committed_at INTEGER NOT NULL,
    overlap      INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL,
    PRIMARY KEY (thread_id, sha)
);
CREATE INDEX idx_thread_commits_thread ON thread_commits (thread_id);
CREATE INDEX idx_thread_commits_committed ON thread_commits (committed_at DESC);
