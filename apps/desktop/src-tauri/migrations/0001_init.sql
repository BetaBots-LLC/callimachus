-- Callimachus canonical store. One local SQLite DB for every indexed thread
-- across Claude Code / Codex / Cursor, plus in-app chats. Timestamps are epoch
-- seconds (UTC). FTS5 is an external-content index over messages.text so we do
-- not duplicate the (large) message bodies.

CREATE TABLE sources (
    id        INTEGER PRIMARY KEY,
    kind      TEXT NOT NULL UNIQUE,   -- claude_code | codex | cursor | in_app
    root_path TEXT
);

-- Seed the known source kinds; root_path filled in by the indexers at runtime.
INSERT OR IGNORE INTO sources (kind) VALUES
    ('claude_code'), ('codex'), ('cursor'), ('in_app');

CREATE TABLE threads (
    id              INTEGER PRIMARY KEY,
    source_id       INTEGER NOT NULL REFERENCES sources(id),
    external_id     TEXT NOT NULL,         -- sessionId / composerId / codex thread id
    title           TEXT,
    project_path    TEXT,
    git_branch      TEXT,
    created_at      INTEGER,
    updated_at      INTEGER,
    message_count   INTEGER NOT NULL DEFAULT 0,
    content_hash    TEXT,                  -- detect "did this thread change" cheaply
    last_indexed_at INTEGER,
    UNIQUE (source_id, external_id)
);
CREATE INDEX idx_threads_project ON threads (project_path);
CREATE INDEX idx_threads_updated ON threads (updated_at);

CREATE TABLE messages (
    id        INTEGER PRIMARY KEY,
    thread_id INTEGER NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    seq       INTEGER NOT NULL,            -- order within the thread
    role      TEXT NOT NULL,               -- user | assistant | tool | system
    text      TEXT NOT NULL,
    tool_name TEXT,                         -- set when role = tool / a tool_use block
    ts        INTEGER,
    raw_json  TEXT,                         -- original line/blob for re-parsing later
    UNIQUE (thread_id, seq)
);
CREATE INDEX idx_messages_thread ON messages (thread_id);

-- External-content FTS5 over messages.text. snippet()/highlight() read the body
-- back from `messages`, so no text duplication. Kept in sync by triggers below.
CREATE VIRTUAL TABLE messages_fts USING fts5 (
    text,
    content      = 'messages',
    content_rowid = 'id',
    tokenize     = "porter unicode61 remove_diacritics 2"
);

CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts (rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts (messages_fts, rowid, text) VALUES ('delete', old.id, old.text);
END;
CREATE TRIGGER messages_au AFTER UPDATE ON messages BEGIN
    INSERT INTO messages_fts (messages_fts, rowid, text) VALUES ('delete', old.id, old.text);
    INSERT INTO messages_fts (rowid, text) VALUES (new.id, new.text);
END;

-- Incremental indexing cursor: one row per source file we have ingested.
-- last_offset = byte offset for append-only JSONL; mtime/size/hash for SQLite sources.
CREATE TABLE index_state (
    path        TEXT PRIMARY KEY,
    source_kind TEXT NOT NULL,
    mtime       INTEGER,
    size        INTEGER,
    last_offset INTEGER NOT NULL DEFAULT 0,
    hash        TEXT,
    updated_at  INTEGER
);

-- LLM provider config (non-secret). API keys live in the OS keychain, never here.
CREATE TABLE providers (
    name     TEXT PRIMARY KEY,   -- anthropic | openai | ollama | custom
    base_url TEXT,
    model    TEXT,
    enabled  INTEGER NOT NULL DEFAULT 0
);
