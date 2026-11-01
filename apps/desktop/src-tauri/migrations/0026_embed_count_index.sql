-- 0026 — covering indexes for the analytics counts (embedding progress, stats, spend).
--
-- embed_progress counts embeddable rows (`role IN ('user','assistant') AND tool_name IS
-- NULL`) and the embedded subset (`embedded = 1`); search::stats groups by role. None of
-- those could use an index (the only role index is the partial `embedded = 0` one from
-- 0008), so every progress poll and Stats open read the whole messages table, which is
-- gigabytes on a large history. A covering (role, tool_name, embedded) index answers all
-- of them from the index alone.
CREATE INDEX IF NOT EXISTS idx_messages_embed_counts ON messages (role, tool_name, embedded);

-- cost::spend sums token usage per (thread, model). The usage columns (0022) sit after
-- `text` in every row, so reading them walked each long message's overflow pages. This
-- partial index holds exactly what spend reads, in GROUP BY order, for the rows that
-- carry usage at all.
CREATE INDEX IF NOT EXISTS idx_messages_usage ON messages (
    thread_id, model, ts, input_tokens, output_tokens, cache_write_tokens, cache_read_tokens
) WHERE model IS NOT NULL;

-- One-time cost: building these reads the messages table once on upgrade.
