-- 0019 — agent session snapshots: durable, resumable checkpoints of a thread (its packed
-- transcript + a carry-forward block of the project's distilled decisions/gotchas/TODOs) that
-- any agent CLI can load to continue across context windows or across tools.
CREATE TABLE snapshots (
    id INTEGER PRIMARY KEY,
    thread_id INTEGER REFERENCES threads(id) ON DELETE SET NULL,
    project_path TEXT,
    source_kind TEXT,
    label TEXT NOT NULL,
    body TEXT NOT NULL,
    token_estimate INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);
CREATE INDEX idx_snapshots_project ON snapshots (project_path, created_at DESC);
CREATE INDEX idx_snapshots_created ON snapshots (created_at DESC);
