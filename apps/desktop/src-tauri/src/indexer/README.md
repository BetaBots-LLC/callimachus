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
pub fn scan(conn: &mut Connection, tick: &mut dyn FnMut()) -> Result<IndexReport>; // tick once per thread
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
4. **Watcher** — `indexer/watcher.rs`: add the store dir to `watch_targets` and an arm in `scanner_for` (routing is by root, so no classifier branch is needed).
5. **Manual reindex** — add a `"<kind>" =>` arm to `index_source` in `lib.rs`.
6. **Frontend** — extend `SourceKind`, `SOURCE_LABELS`, and `INDEXABLE_SOURCES` in `apps/desktop/src/lib/api.ts` (the filter chips and reindex buttons derive from the last one). Optionally add to `OPEN_TARGETS` / `cli_resume` if the CLI is launchable.

Tests: a parse test over a synthetic sample, a search round-trip, and an
`#[ignore]`d real-data smoke test (`cargo test -- --ignored real_<kind>_index`).

## Change detection (don't re-embed unchanged threads)

Re-upserting an unchanged thread churns message ids and forces re-embedding. Avoid:

- **One file per thread** (JSONL) — skip on `(mtime, size)` via `file_state` / `set_file_state` (see `gemini.rs`).
- **Append-only JSONL** (Claude Code) — `claude.rs` also keeps a byte cursor per file in `index_state` (`last_offset` = end of the last complete line, `hash` = FNV-1a of the 4 KB at each end of the indexed region). When a file only grew, only the new lines are read and parsed and folded onto the stored prefix; a shrink, a fingerprint mismatch or a missing thread falls back to a full parse. A half-written last line is left for the next pass.
- **One DB for all threads** (SQLite) — skip the whole pass with `file_change_state` (read-only) on the DB file, then `set_file_state` only after the upserts succeed (see `goose.rs`). If the store runs in WAL mode, include the `-wal` file in the change key, since new rows land there first (see `cursor.rs`).
- **One DB, many threads that change independently** (Cursor) — fingerprint each thread from cheap metadata (`lastUpdatedAt`, row count, `octet_length` sums, which read only record headers) in `index_state` under a synthetic `<db>#composer:<id>` key, and only extract threads whose fingerprint moved.
- **Many files per thread** (OpenCode) — fingerprint the message dir as `(max mtime, file count)` and store it in `file_state` keyed by the session file.

## Tool output cap

Store tool results through `claude::cap_tool_text` (16,000 chars: the head plus the last
4,000 chars, with an elision marker in between). Tool output was ~70% of all stored text
but is never embedded or distilled; the tail is kept because that is where build and test
errors (what `issues.rs` mines) usually are. Cap the **trimmed** text so the result is
deterministic and the incremental prefix check stays stable. Rows indexed before the cap
are not a mismatch either: `upsert_thread` treats a stored `tool` row whose capped form is
the new text as matching and rewrites it in place, so the first re-parse after an upgrade
doesn't full-replace the thread. `cleanup::compact_tool_output` rewrites such rows in bulk
with the same function; add a source's kind to `cleanup::CAPPED_TOOL_SOURCES` once its
indexer caps.

## Things every indexer must skip

- **Deleted threads.** `cleanup::delete_threads` writes a tombstone (`deleted_threads`).
  Check `cleanup::is_tombstoned(conn, source_id, external_id)` before parsing, and record
  the file state anyway so an unchanged file isn't re-checked every scan. (`upsert_thread`
  checks again inside its write transaction, so a delete racing the parse still wins.)
- **The app's own CLI-engine sessions.** Distill / Ask through `claude -p` or `codex exec`
  can leave transcripts behind. Skip a thread when
  `agent::is_app_cli_session(cwd, first_user_texts)` is true.

## Data roots

Resolve stores from the agent's own env override, not a hardcoded home path:
`claude::config_dir()` honors `CLAUDE_CONFIG_DIR`, `codex::codex_root()` honors
`CODEX_HOME`, and both accept a Settings value stored in `app_config`
(`paths.claude_config_dir`, `paths.codex_home`), because a GUI launched from Finder/Dock
never sees shell env vars.

## Watcher

`watcher.rs` routes each event to the most specific watched root that contains it (both
the configured and the symlink-resolved spelling), with an optional file-name filter for
directories shared with unrelated files. Every kind it can route must have an arm in
`scanner_for` (a test enforces this). Claude Code events are indexed per changed file via
`claude::index_paths`; other sources re-run their fingerprint-skipping `scan`.

## Tolerant parsing

Tool output and message bodies vary wildly. Flatten defensively: pull text parts,
turn tool calls into `tool_name` + a compact string, skip unknown block types, and
tolerate malformed lines (continue, don't bail). Only `user`/`assistant` messages
are embedded; `tool`/`system` are searchable via FTS but skipped by the embedder.

## Verified storage map (macOS)

| Kind | Path | Format |
|------|------|--------|
| `claude_code` | `$CLAUDE_CONFIG_DIR` or `~/.claude`, then `projects/**/*.jsonl` | JSONL, content blocks |
| `codex` | `$CODEX_HOME` or `~/.codex` (sessions JSONL + state SQLite) | mixed |
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
| `copilot` | `<editor>/User/workspaceStorage/<hash>/chatSessions/<uuid>.{json,jsonl}` and `<editor>/User/globalStorage/emptyWindowChatSessions/<uuid>.{json,jsonl}` (Code, Code - Insiders, Cursor, VSCodium, Windsurf) | one JSON object `{ kind, v }` per file, turns in `v.requests[]`; project from the workspace's `workspace.json` |

Honor `XDG_DATA_HOME` (Goose/OpenCode) and `OPENCODE_DATA_DIR` where applicable.

## Candidates not yet integrated

- **Charm Crush** (`~/.crush/crush.db`, SQLite), **Factory Droid**
  (`~/.factory/sessions/`, JSONL), **Copilot CLI** (the standalone terminal agent;
  Copilot Chat in VS Code-family editors is already indexed as `copilot`;
  `~/.copilot/session-store.db`, schema undocumented). Peek at a real file first to
  confirm the schema.
- **Amp** — server-side (cloud Postgres); needs an authed API, not file indexing.
- **Zed** — thread bodies are LMDB binary; only the SQLite metadata is parseable.
