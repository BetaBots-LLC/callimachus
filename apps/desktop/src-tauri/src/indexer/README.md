# Indexers — adding a coding-agent source

Each source (Claude Code, Codex, Cursor, Gemini, Qwen, Goose, OpenCode, Continue,
Cline, …) parses its own on-disk history into the **canonical store** via one
common shape. Add a source once and search (FTS5 keyword), semantic search
(embeddings), the source filter, per-source reindex, and the live file-watcher all
light up for free.

## The contract

Every indexer is a module exposing two things:

```rust
pub const KIND: &str = "gemini";                 // matches the `sources.kind` row
pub fn scan(conn: &mut Connection) -> Result<IndexReport>;
```

`scan` locates the store, parses each conversation into a `ParsedThread`, and calls
`upsert_thread`. That's it — `upsert_thread` is **idempotent** (re-running replaces
a thread's messages), and FTS triggers + the embedding job key off the canonical
`messages` table, so they need no per-source code.

```rust
pub struct ParsedThread {
    pub external_id: String,      // stable, unique within the source (path / session id)
    pub title: Option<String>,
    pub project_path: Option<String>,
    pub git_branch: Option<String>,
    pub created_at: Option<i64>,  // epoch seconds
    pub updated_at: Option<i64>,
    pub is_subagent: bool,
    pub messages: Vec<ParsedMessage>,
}
pub struct ParsedMessage {
    pub role: String,             // user | assistant | tool | system
    pub text: String,
    pub tool_name: Option<String>,
    pub ts: Option<i64>,
}
```

## Six steps to add a source

1. **Migration** — `migrations/00NN_*.sql`: `INSERT OR IGNORE INTO sources (kind) VALUES ('<kind>');` and register it in `db/migrations.rs`.
2. **Indexer** — `indexer/<kind>.rs` with `KIND` + `scan()`. Parse to `ParsedThread`, call `upsert_thread`. Skip unchanged work (see change-detection below).
3. **Register** — add `pub mod <kind>;` and `("<kind>", <kind>::scan)` to the `sources` array in `scan_all_with_progress` in `indexer/mod.rs`.
4. **Watcher** — `indexer/watcher.rs`: add the store dir to `watch_targets`, a path→kind branch in the classifier, and a dispatch arm in `reindex`.
5. **Manual reindex** — add a `"<kind>" =>` arm to `index_source` in `lib.rs`.
6. **Frontend** — extend `SourceKind`, `SOURCE_LABELS`, and `INDEXABLE_SOURCES` in `apps/desktop/src/lib/api.ts` (the filter chips and reindex buttons derive from the last one). Optionally add to `OPEN_TARGETS` / `cli_resume` if the CLI is launchable.

Tests: a parse test over a synthetic sample, a search round-trip, and an
`#[ignore]`d real-data smoke test (`cargo test -- --ignored real_<kind>_index`).

## Change detection (don't re-embed unchanged threads)

Re-upserting an unchanged thread churns message ids and forces re-embedding. Avoid:

- **One file per thread** (JSONL) — skip on `(mtime, size)` via `file_state` / `set_file_state` (see `claude.rs`, `gemini.rs`).
- **One DB for all threads** (SQLite) — skip the whole pass with `file_change_state` (read-only) on the DB file, then `set_file_state` only after the upserts succeed (see `cursor.rs`, `goose.rs`).
- **Many files per thread** (OpenCode) — fingerprint the message dir as `(max mtime, file count)` and store it in `file_state` keyed by the session file.

## Tolerant parsing

Tool output and message bodies vary wildly. Flatten defensively: pull text parts,
turn tool calls into `tool_name` + a compact string, skip unknown block types, and
tolerate malformed lines (continue, don't bail). Only `user`/`assistant` messages
are embedded; `tool`/`system` are searchable via FTS but skipped by the embedder.

## Verified storage map (macOS)

| Kind | Path | Format |
|------|------|--------|
| `claude_code` | `~/.claude/projects/**/*.jsonl` | JSONL, content blocks |
| `codex` | `~/.codex/` (sessions JSONL + state SQLite) | mixed |
| `cursor` | `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb` | SQLite (`cursorDiskKV`) |
| `gemini` | `~/.gemini/tmp/<id>/chats/*.jsonl` | JSONL, `type: user`/`gemini`, `content` PartListUnion |
| `qwen` | `~/.qwen/tmp/<hash>/chats/*.jsonl` | JSONL, `type: user`/`assistant`, `message.parts` |
| `goose` | `~/.local/share/goose/sessions/sessions.db` | SQLite (`sessions` + `messages`, `content_json`) |
| `opencode` | `~/.local/share/opencode/storage/{session,message,part}/*.json` | JSON tree, join msg+part by id |
| `opencode` | `~/.local/share/opencode/opencode.db` | SQLite V1 (`session` + `message` + `part`, `data` JSON) |
| `continue` | `~/.continue/sessions/*.json` | JSON `history[]`, no per-msg ts (use mtime) |
| `cline` | `<editor>/User/globalStorage/saoudrizwan.claude-dev/tasks/<id>/api_conversation_history.json` | Anthropic Messages array (multi-editor; index-only, no CLI) |
| `roo` | `<editor>/User/globalStorage/rooveterinaryinc.roo-cline/tasks/<id>/…` | Cline-architecture fork — shares `cline::scan_ext` |
| `kilo` | `<editor>/User/globalStorage/kilocode.kilo-code/tasks/<id>/…` | Cline-architecture fork — shares `cline::scan_ext` |

Honor `XDG_DATA_HOME` (Goose/OpenCode) and `OPENCODE_DATA_DIR` where applicable.

## Candidates not yet integrated

- **Charm Crush** (`~/.crush/crush.db`, SQLite), **Factory Droid**
  (`~/.factory/sessions/`, JSONL), **Copilot CLI** (`~/.copilot/session-store.db`,
  schema undocumented) — peek at a real file first to confirm the schema.
- **Amp** — server-side (cloud Postgres); needs an authed API, not file indexing.
- **Zed** — thread bodies are LMDB binary; only the SQLite metadata is parseable.
