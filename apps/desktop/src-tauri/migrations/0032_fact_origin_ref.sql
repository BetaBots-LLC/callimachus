-- 0032: where a fact came from outside a thread's own messages. `source_message_id` points
-- at a message in the fact's thread; facts imported from what an agent already wrote
-- (a Claude Code auto-memory topic file, a PreCompact compaction summary) have no such
-- message, so they carry a stable reference instead: 'claude-memory:<slug>/<file>' or
-- 'claude-compact:<session_id>'. The importers replace a file's or session's facts by this
-- key on every change, so it gets an index; most facts leave it NULL, hence partial.
ALTER TABLE facts ADD COLUMN origin_ref TEXT;
CREATE INDEX idx_facts_origin_ref ON facts(origin_ref) WHERE origin_ref IS NOT NULL;
