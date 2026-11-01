-- 0027 — tombstones for threads the user deleted.
--
-- Deleting a thread used to remove its rows but leave nothing behind, so the next scan
-- re-created it: Cursor re-extracts every composer whenever state.vscdb changes, and a
-- Claude/Codex transcript comes back as soon as its file is touched again (then gets
-- re-distilled, which costs money with auto-distill on). cleanup::delete_threads now
-- records (source, external id) here and the indexers skip tombstoned ids.
-- In-app chats are never tombstoned: the app owns them and nothing re-imports them.
CREATE TABLE deleted_threads (
    source_id   INTEGER NOT NULL REFERENCES sources(id),
    external_id TEXT NOT NULL,
    deleted_at  INTEGER NOT NULL,
    PRIMARY KEY (source_id, external_id)
) WITHOUT ROWID;
