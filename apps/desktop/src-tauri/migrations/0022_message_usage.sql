-- 0022 — per-message token usage + model, for the cost/spend layer. Captured for assistant
-- turns from the source's usage block (Claude Code reports input/output/cache tokens + model);
-- NULL for everything else. Re-index to backfill (the source files carry it; the index didn't).
ALTER TABLE messages ADD COLUMN model TEXT;
ALTER TABLE messages ADD COLUMN input_tokens INTEGER;
ALTER TABLE messages ADD COLUMN output_tokens INTEGER;
ALTER TABLE messages ADD COLUMN cache_write_tokens INTEGER;
ALTER TABLE messages ADD COLUMN cache_read_tokens INTEGER;
-- Sum tokens / list spend by model without scanning non-LLM rows.
CREATE INDEX idx_messages_model ON messages (model) WHERE model IS NOT NULL;
