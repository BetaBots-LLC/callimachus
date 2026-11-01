-- 0031: index-only reads for the FTS join, the Coach heatmap and the commit linker, and
-- drop two indexes nothing reads.
--
-- `ts` sits after `text` in every messages row, so any query that needs a message's
-- thread, role or ts walked that row's overflow pages: one random read of the multi-GB
-- table per FTS hit before a single filter ran. Both new indexes carry exactly those
-- columns. One-time cost: each build reads the messages table once on upgrade (db::migrate
-- pre-reads a large file sequentially first, so the scans hit the page cache).

-- Dropped first so the builds below reuse their pages instead of growing the file.

-- 0008's partial index: the embedder's pending queue and both progress counts are served
-- by 0026's covering (role, tool_name, embedded) index, and every insert, delete and
-- embedded-flag flip still paid to maintain this one (~20 MB on a large index).
DROP INDEX IF EXISTS idx_messages_pending_embed;

-- 0001's (thread_id) index is a prefix of the UNIQUE (thread_id, seq) autoindex, which
-- serves thread_detail, the indexer's per-thread deletes and the FK cascade on its own.
DROP INDEX IF EXISTS idx_messages_thread;

-- Replaces it with (thread_id, ts, role): the Coach heatmap (thread -> ts range, counting
-- user/assistant rows) and gitlink's per-thread MIN/MAX(ts) become index-only. The
-- planner picks it without a hint.
CREATE INDEX IF NOT EXISTS idx_messages_thread_ts ON messages (thread_id, ts, role);

-- Covering lookup by id for FTS hits: messages_fts yields rowids, and the subagent /
-- source / project / date filters need m.thread_id, m.role and m.ts for every hit before
-- ranking. The planner always prefers the rowid lookup on its own, so the FTS queries
-- name this index with INDEXED BY (search::fts_message_join, which falls back to the
-- plain join on an index that predates this migration).
CREATE INDEX IF NOT EXISTS idx_messages_cover ON messages (id, thread_id, role, ts);
