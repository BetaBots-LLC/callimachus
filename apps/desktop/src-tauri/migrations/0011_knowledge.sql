-- 0011 — distilled knowledge layer.
-- One row per distilled fact about a thread (decision / gotcha / todo / summary).
-- Slice 1 populates ONLY kind='todo' via the indexer's free heuristic scan; the LLM
-- tier (decisions/gotchas/summary, lazy on-demand) lands in a later slice and reuses
-- this same table. Facts hang off thread_id (ON DELETE CASCADE) so deleting a thread
-- drops its facts; source_message_id is provenance and goes NULL if that message is
-- replaced on re-index. Heuristic todos are re-derived (delete+rescan) every index, so
-- they never go stale.
CREATE TABLE facts (
    id                INTEGER PRIMARY KEY,
    thread_id         INTEGER NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    kind              TEXT NOT NULL CHECK (kind IN ('summary', 'decision', 'gotcha', 'todo')),
    text              TEXT NOT NULL,
    source_message_id INTEGER REFERENCES messages(id) ON DELETE SET NULL,
    status            TEXT NOT NULL DEFAULT 'open',   -- 'open' | 'done' (todos)
    extractor         TEXT NOT NULL DEFAULT 'llm',    -- 'llm' | 'heuristic'
    seq               INTEGER,
    created_at        INTEGER NOT NULL
);

CREATE INDEX idx_facts_thread ON facts(thread_id);
CREATE INDEX idx_facts_kind ON facts(kind);
-- Serves list_open_todos without scanning non-todo facts.
CREATE INDEX idx_facts_open_todo ON facts(created_at) WHERE kind = 'todo' AND status = 'open';
