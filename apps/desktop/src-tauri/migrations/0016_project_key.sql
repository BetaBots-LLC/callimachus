-- 0016 — canonical project key: a normalized, stable grouping key per thread (git root /
-- resolved path) so one repo doesn't fragment into separate memories across worktrees,
-- symlinks, ~ vs absolute, or trailing slashes. Populated at index time + backfilled at
-- startup (the value is computed in Rust, so it starts NULL and queries COALESCE to
-- project_path until backfill fills it in).
ALTER TABLE threads ADD COLUMN project_key TEXT;

CREATE INDEX idx_threads_project_key ON threads (project_key);
