-- Index thread_commits by sha so the inverse lookup (commits_by_sha: "which thread produced this
-- commit?") is a direct index probe, not a scan. Powers `cal audit-pr` over a PR's commit SHAs.
CREATE INDEX IF NOT EXISTS idx_thread_commits_sha ON thread_commits(sha);
