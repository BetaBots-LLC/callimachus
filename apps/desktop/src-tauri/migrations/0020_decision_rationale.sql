-- 0020 — ADR-style decisions: an optional rationale ("why") on a fact. Decisions carry it so
-- the contradiction guard can surface "you decided X BECAUSE Y" before an agent re-litigates a
-- settled choice. Nullable + lazily populated; gotchas/todos leave it NULL.
ALTER TABLE facts ADD COLUMN rationale TEXT;
