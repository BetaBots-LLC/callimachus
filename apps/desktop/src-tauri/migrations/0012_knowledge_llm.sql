-- 0012 — LLM-distilled knowledge tier on top of 0011's `facts` table.
-- Opt-in: nothing here runs until the user enables distillation and picks an engine
-- (local Ollama or a cloud key). Adds per-thread extraction state, a fact-embedding
-- flag + vector table for later semantic recall, and a cross-process config table.

-- Per-thread extraction state. Like `starred`, these columns are OMITTED from
-- upsert_thread's UPDATE SET so a re-index never wipes distilled knowledge; the
-- indexer explicitly resets knowledge_extracted=0 when the message count changes.
ALTER TABLE threads ADD COLUMN knowledge_extracted    INTEGER NOT NULL DEFAULT 0;
ALTER TABLE threads ADD COLUMN knowledge_extracted_at INTEGER;
ALTER TABLE threads ADD COLUMN knowledge_msg_count    INTEGER;  -- message_count at extraction
ALTER TABLE threads ADD COLUMN knowledge_error        TEXT;     -- last failure, NULL on success

-- Embedding flag on facts (mirrors messages.embedded) for the future recall drain.
ALTER TABLE facts ADD COLUMN embedded INTEGER NOT NULL DEFAULT 0;
CREATE INDEX idx_facts_pending_embed ON facts(id) WHERE embedded = 0;

-- Fact vectors — same 384-dim bge-small + vec0 as vec_chunks (0004), for semantic
-- recall of decisions/gotchas. Populated lazily; empty until recall ships.
CREATE VIRTUAL TABLE vec_facts USING vec0(
    fact_id   integer,
    embedding float[384] distance_metric=cosine
);
CREATE TRIGGER facts_vec_ad AFTER DELETE ON facts BEGIN
    DELETE FROM vec_facts WHERE fact_id = old.id;
END;

-- Cross-process key/value config: the desktop app, `cal`, and the MCP server all open
-- the same DB, so distillation consent + engine choice must be shared here (not in the
-- frontend's localStorage). Keys: knowledge.enabled, knowledge.provider, knowledge.model.
CREATE TABLE app_config (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
