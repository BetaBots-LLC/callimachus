-- 0035: what a fact's truth is now, beside the fact itself. A later session can reverse a
-- decision (`supersedes`), disagree with it without settling it (`contradicts`), confirm it
-- again (`reaffirmed`) or show it no longer holds (`stale` / `wrong`). Both tables are keyed
-- by `facts.uid` (0034), not the row id: a re-distill that keeps a fact's text keeps its uid,
-- and the uid is what repo memory files and other machines share. Nothing is deleted to
-- express a reversal: the older fact stays as history and readers leave it out of injection.
-- Only new, empty tables and their indexes, so `db::migrate` skips its pre-read.

-- A relation between two facts. `from_uid` is the NEWER fact for 'supersedes'. `source`
-- says who declared it: the distiller (from a session that states it), the user, an agent
-- (`record_decision` `replaces`), a repo memory import, or a conflict review. A link whose
-- uid matches no fact (the fact was replaced or deleted) is ignored on read.
-- `dismissed_at`: the user undid the link. The row stays, inactive, so the distiller, an
-- agent or a repo import asserting the same pair again is a no-op (else a re-distill of the
-- session would bring back what the user just undid); only a later user link clears it.
CREATE TABLE fact_links (
    id           INTEGER PRIMARY KEY,
    from_uid     TEXT NOT NULL,
    to_uid       TEXT NOT NULL,
    kind         TEXT NOT NULL CHECK (kind IN ('supersedes', 'contradicts')),
    source       TEXT NOT NULL CHECK (source IN ('distill', 'user', 'agent', 'repo', 'review')),
    thread_id    INTEGER REFERENCES threads(id) ON DELETE SET NULL, -- session that asserted it
    created_at   INTEGER NOT NULL,
    dismissed_at INTEGER,
    UNIQUE (from_uid, to_uid, kind)
);
-- "Is this fact superseded?" for every fact a memory reader returns. The UNIQUE above
-- already serves lookups by `from_uid`.
CREATE INDEX idx_fact_links_to ON fact_links (to_uid, kind);
-- A re-distill replaces the thread's own links, and a thread delete sets their thread_id
-- to NULL: both look links up by thread. Partial, since agent and user links carry none
-- (an equality lookup implies NOT NULL, so SQLite uses it for both).
CREATE INDEX idx_fact_links_thread ON fact_links (thread_id) WHERE thread_id IS NOT NULL;

-- A per-fact assertion with no second fact: a later session reaffirmed it, or it was flagged
-- stale / wrong (by an agent, the distiller via "retires", or the user). `resolved_at`: the
-- user kept the fact, which clears the flag without deleting the record of it.
CREATE TABLE fact_signals (
    id          INTEGER PRIMARY KEY,
    fact_uid    TEXT NOT NULL,
    kind        TEXT NOT NULL CHECK (kind IN ('reaffirmed', 'stale', 'wrong')),
    source      TEXT NOT NULL CHECK (source IN ('distill', 'agent', 'user')),
    thread_id   INTEGER REFERENCES threads(id) ON DELETE SET NULL,
    reason      TEXT,
    created_at  INTEGER NOT NULL,
    resolved_at INTEGER
);
CREATE INDEX idx_fact_signals_fact ON fact_signals (fact_uid, kind);
-- One reaffirmation per fact and session, however often the session is re-distilled.
CREATE UNIQUE INDEX idx_fact_signals_reaffirm ON fact_signals (fact_uid, thread_id)
    WHERE kind = 'reaffirmed';
-- Same reason as idx_fact_links_thread: re-distill and thread deletes find signals by thread.
CREATE INDEX idx_fact_signals_thread ON fact_signals (thread_id) WHERE thread_id IS NOT NULL;
