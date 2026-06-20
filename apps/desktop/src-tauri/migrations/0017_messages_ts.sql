-- Index messages by timestamp so the Coach activity heatmap (a GROUP BY day over a
-- recent ts range) range-scans instead of full-scanning the largest table in the DB.
CREATE INDEX IF NOT EXISTS idx_messages_ts ON messages(ts) WHERE ts IS NOT NULL;
