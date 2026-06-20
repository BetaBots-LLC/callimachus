-- 0015 — fact curation: pin / edit / hide distilled knowledge so users can trust the
-- auto-generated Project Memory. Curated facts survive re-distillation (they are not the
-- LLM's to overwrite anymore).
ALTER TABLE facts ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0; -- user-pinned: ranks first, never auto-removed
ALTER TABLE facts ADD COLUMN edited INTEGER NOT NULL DEFAULT 0; -- user-edited text: kept on re-distill
ALTER TABLE facts ADD COLUMN hidden INTEGER NOT NULL DEFAULT 0; -- user-deleted: hidden everywhere, kept as a tombstone so re-distill's DELETE skips it

-- Fast path for "does this thread have curated facts to preserve?" on re-distill.
CREATE INDEX idx_facts_curated ON facts(thread_id) WHERE pinned = 1 OR edited = 1 OR hidden = 1;
