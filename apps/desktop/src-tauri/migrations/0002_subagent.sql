-- Mark threads that are subagent transcripts (Claude Code `<uuid>/subagents/…`)
-- so the UI can hide them by default while still keeping them searchable.
ALTER TABLE threads ADD COLUMN is_subagent INTEGER NOT NULL DEFAULT 0;
CREATE INDEX idx_threads_subagent ON threads (is_subagent);
