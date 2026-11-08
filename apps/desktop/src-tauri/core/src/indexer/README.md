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
    pub usage: Vec<(usize, MsgUsage)>, // optional: token usage per model call, keyed by message index (Claude Code, Codex, Gemini, Qwen, OpenCode and Copilot fill it; `ParsedThread::attach_usage` places a call with no message of its own)
}
pub struct ParsedMessage {
    pub role: String,             // user | assistant | tool | system
    pub text: String,
    pub tool_name: Option<String>,
    pub ts: Option<i64>,
}
```

`ParsedThread` derives `Default`, so a source with no token usage ends the literal with `..Default::default()`.

## Six steps to add a source

1. **Migration**: `migrations/00NN_*.sql` (in `apps/desktop/src-tauri`): `INSERT OR IGNORE INTO sources (kind) VALUES ('<kind>');` and register it in `db/migrations.rs`.
2. **Indexer**: `indexer/<kind>.rs` with `KIND` + `scan()`. Parse to `ParsedThread`, call `upsert_thread`. Skip unchanged work (see change-detection below). The small shared helpers (titles, timestamps, text parts, `push_message`) are in `indexer/common.rs`, and `IndexReport::record` tallies each file.
3. **Register**: add `pub mod <kind>;` and a `Source { kind, scan, caps_tool_output }` entry to `SOURCES` in `indexer/mod.rs`. The full reindex, the per-source reindex (`jobs::index_source`, behind the app's `index_source` command), the watcher's dispatch and tool-output cleanup all read this one list; tests check it against the `sources` table, `telemetry::SOURCES` and whether the indexer calls `cap_tool_text`. Add your indexer's file to the `match` in `caps_tool_output_matches_the_indexers` as well; until you do, that test panics with "add <kind>'s indexer source here".
4. **Watcher**: `indexer/watcher.rs`: add the store dir to `watch_targets` (routing is by root, so no classifier branch is needed).
5. **Telemetry**: add the kind to `telemetry::SOURCES` in the app crate (`src-tauri/src/telemetry.rs`; its `every_source_is_on_the_telemetry_allowlist` test checks it against `SOURCES`). Nothing in `apps/desktop/src/lib/telemetry.ts` needs to change: its `thread_opened` event takes `SourceKind` from `api.ts`, which step 6 extends.
6. **Frontend**: extend `SourceKind`, `SOURCE_LABELS`, and `INDEXABLE_SOURCES` in `apps/desktop/src/lib/api.ts` (the filter chips and reindex buttons derive from the last one). Optionally add to `OPEN_TARGETS` / `cli_resume` if the CLI is launchable.

Tests: a parse test over a synthetic sample, a search round-trip, and an
`#[ignore]`d real-data smoke test (`cargo test -p callimachus-core -- --ignored real_<kind>_index`).

## Change detection (don't re-embed unchanged threads)

Re-upserting an unchanged thread churns message ids and forces re-embedding. Avoid:

- **One file per thread** (JSONL) — skip on `(mtime, size)` via `file_change_state` / `set_file_state` (see `gemini.rs`).
- **Append-only JSONL** (Claude Code) — `claude.rs` also keeps a byte cursor per file in `index_state` (`last_offset` = end of the last complete line, `hash` = FNV-1a of the 4 KB at each end of the indexed region). When a file only grew, only the new lines are read and parsed and folded onto the stored prefix; a shrink, a fingerprint mismatch or a missing thread falls back to a full parse. A half-written last line is left for the next pass. A full scan groups its writes into short batched transactions (one commit per thread spent a quarter of a cold index in COMMIT); `upsert_thread` runs as a savepoint when the caller already has a transaction open.
- **One DB for all threads** (SQLite) — skip the whole pass with `file_change_state` (read-only) on the DB file, then `set_file_state` only after the upserts succeed (see `goose.rs`). If the store runs in WAL mode, include the `-wal` file in the change key, since new rows land there first (see `cursor.rs`).
- **One DB, many threads that change independently** (Cursor) — fingerprint each thread from cheap metadata (`lastUpdatedAt`, row count, `octet_length` sums, which read only record headers) in `index_state` under a synthetic `<db>#composer:<id>` key, and only extract threads whose fingerprint moved.
- **One conversation in several stores** (Cursor: a composer in state.vscdb, an agent
  transcript, a CLI chat store, all under the same agent id): one thread per id, and the
  store that fills it is chosen by what the stores held the last time each was read
  (composer with text > CLI store with messages > transcript). That outcome is stored per
  file state (the composer fingerprint's `:text` / `:empty` mark, the store's
  `<store>#parsed` row), so the same files always pick the same winner and two stores
  never take turns rewriting a thread. A covered transcript is skipped without recording
  its file state, so it is picked up if the richer store ever stops covering it. A
  composer fingerprint with no mark was written by an older build: its coverage is checked
  against state.vscdb once and the mark stored.
- **Many files per thread** (OpenCode) — fingerprint the message dir as `(max mtime, file count)` and store it in `file_state` keyed by the session file.

## Tool output cap

Store tool results through `claude::cap_tool_text` (16,000 chars: the head plus the last
4,000 chars, with an elision marker in between). Tool output was ~70% of all stored text
but is never embedded or distilled; the tail is kept because that is where build and test
errors (what `issues.rs` mines) usually are. Cap the **trimmed** text so the result is
deterministic and the incremental prefix check stays stable. Claude Code also caps its tool
calls (assistant rows with a `tool_name`, `"Write: {input json}"`), since a Write carries the
whole file, and so does Codex for its freeform `apply_patch` calls; the other sources store
calls in full. Rows indexed before the cap are not a
mismatch either: `upsert_thread` treats a stored `tool` row, or a stored tool call, whose
capped form is the new text as matching and rewrites it in place, so the first re-parse
after an upgrade doesn't full-replace the thread. `cleanup::compact_tool_output` rewrites
such rows in bulk with the same function: `tool` rows for the sources registered with
`caps_tool_output: true`, tool calls for Claude Code only.

## Tool calls and `file_edits`

Store each tool call as an assistant row with `tool_name` set and the text `"<name>: <input>"`
(the input as JSON when the source has it). `file_edits::edits_of` reads the paths from that
text: it knows the file tools by name (read / edit / write verbs, `apply_patch`, shell tools),
so a new source whose tools use other names adds them there, with a test. Anything else
with a path argument counts as a mention.

The indexer writes `file_edits` for the messages it inserts. Threads indexed before 0034 get
theirs once from the stored tool-call rows (`file_edits::backfill_file_edits_once`, behind
the `file_edits.backfill_pending` flag 0034 sets): a Reindex never re-parses an unchanged
transcript, and resetting the cursors would rewrite every message id and re-embed the whole
history. The watcher runs it after its launch catch-up, pausing between batches while a
writer job runs; `cal index` runs it after its pass.

Token usage has the same problem for the sources that started recording it after their
sessions were indexed (Codex, Gemini, Qwen, OpenCode). `usage_backfill::backfill_usage_once`
re-parses those threads' transcripts once, right after the `file_edits` backfill and from the
same callers, and writes only the usage columns onto the stored rows by `seq` (no replace, no
id churn, no re-embedding); `usage.backfill_done` in `app_config` marks it done, and a fresh
index is marked done at once. A source you add that records usage from day one needs none of
this; one that starts recording it later joins `usage_backfill::SOURCES`.

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
`CODEX_HOME` (and the state DB follows Codex's own `sqlite_home` / `CODEX_SQLITE_HOME`),
`copilot_cli::copilot_home()` honors `COPILOT_HOME`, and all three accept a Settings
value stored in `app_config` (`paths.claude_config_dir`, `paths.codex_home`,
`paths.copilot_home`), because a GUI launched from Finder/Dock never sees shell env vars.
Project slugs (`-Users-me-x`, Cursor's `Users-me-x`) resolve through
`common::dir_for_project_slug`, which starts the walk at the drive a Windows slug names
(`C--Users-me-x`) and follows symlinked directories.

## Watcher

`watcher.rs` routes each event to the most specific watched root that contains it (both
the configured and the symlink-resolved spelling), with an optional file-name filter for
directories shared with unrelated files. Every kind it can route must be in `SOURCES` (a
test enforces this). Claude Code events are indexed per changed file via
`claude::index_paths`; other sources re-run their fingerprint-skipping `scan`. Watch
events only start when the watch does, so at launch (once anything has been indexed) the
watcher also runs one catch-up pass over every source for what changed while the app was
closed, holding the reindex flag meanwhile.

After each batch the watcher links the commits of the threads it wrote (`jobs::CommitLinker`:
each checkout at most once a minute, Today's 2 s budget per pass, a checkout whose `git log`
runs out of time retried with backoff), and on its minute tick, once the catch-up is done,
refreshes `issue_clusters` when the index changed since the stored refresh (mined on a
read-only connection on its own thread, yielding to writer jobs). `cal index` does the same
for the pass it ran (`headless::graph_upkeep`), so a machine kept fresh only by the plugin's
`cal index --if-stale` gets commit links and served clusters too.

## Tolerant parsing

Tool output and message bodies vary wildly. Flatten defensively: pull text parts,
turn tool calls into `tool_name` + a compact string, skip unknown block types, and
tolerate malformed lines (continue, don't bail). Only `user`/`assistant` messages
are embedded; `tool`/`system` are searchable via FTS but skipped by the embedder.

## Verified storage map (macOS)

| Kind | Path | Format |
|------|------|--------|
| `claude_code` | `$CLAUDE_CONFIG_DIR` or `~/.claude`, then `projects/**/*.jsonl` | JSONL, content blocks |
| `codex` | `$CODEX_HOME` or `~/.codex`: `sessions/` + `archived_sessions/` rollout JSONL, plus the newest `state_<N>.sqlite` under `sqlite_home` (config.toml), else `$CODEX_SQLITE_HOME`, else the Codex home | mixed; usage from `event_msg` `token_count` (running totals, differenced per call) with the model from `turn_context` |
| `cursor` | `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb` | SQLite (`cursorDiskKV`) |
| `cursor` | `~/.cursor/projects/<slug>/agent-transcripts/<id>/<id>.jsonl`, plus `<id>/subagents/*.jsonl`; `<slug>` is the workspace path with non-alphanumerics as `-`, resolved against the disk. Observed on one Mac: 195 nested transcripts (the CLI session's too) and 147 subagents. A flat `agent-transcripts/<id>.jsonl` is also read but was never seen | JSONL `{role, message.content[]}`, `text` / `tool_use` parts, no clock but a user turn's `<timestamp>` |
| `cursor` | `~/.cursor/chats/<md5 of cwd>/<agent id>/store.db` (+ `meta.json`). Observed: one store, a 4 KB store.db with every table still in its `-wal`, read from a temp copy when a plain read-only open can't see the WAL | SQLite `blobs` + `meta`: the root blob (protobuf) lists the JSON message blobs in order, field 9 the workspace URI |
| `gemini` | `~/.gemini/tmp/<id>/chats/*.jsonl` | JSONL, `type: user`/`gemini`, `content` PartListUnion; a message re-recorded under the same `id` replaces the first copy; usage from `tokens` + `model` |
| `qwen` | `~/.qwen/tmp/<hash>/chats/*.jsonl` | JSONL, `type: user`/`assistant`, `message.parts`; usage from `usageMetadata` + `model` |
| `goose` | `~/.local/share/goose/sessions/sessions.db` | SQLite (`sessions` + `messages`, `content_json`) |
| `opencode` | `~/.local/share/opencode/storage/{session,message,part}/*.json` | JSON tree, join msg+part by id; usage from the message's `tokens` + `modelID` |
| `opencode` | `~/.local/share/opencode/opencode.db` | SQLite V1 (`session` + `message` + `part`, `data` JSON); usage from `message.data` `tokens` + `modelID`, text-less turns included |
| `continue` | `~/.continue/sessions/*.json` | JSON `history[]`, no per-msg ts (use mtime) |
| `cline` | `<editor>/User/globalStorage/saoudrizwan.claude-dev/tasks/<id>/api_conversation_history.json` | Anthropic Messages array (multi-editor; index-only, no CLI) |
| `roo` | `<editor>/User/globalStorage/rooveterinaryinc.roo-cline/tasks/<id>/…` | Cline-architecture fork — shares `cline::scan_ext` |
| `kilo` | `<editor>/User/globalStorage/kilocode.kilo-code/tasks/<id>/…` | Cline-architecture fork — shares `cline::scan_ext` |
| `copilot_cli` | Settings `paths.copilot_home`, `$COPILOT_HOME` or `~/.copilot`, then `session-state/<id>/events.jsonl` and `workspace.yaml`. Observed: 63 sessions, every one a GitHub Desktop commit-message run (`client_name: sdk`, the diff as the prompt), which are skipped; only interactive conversations are indexed. A flat `<id>.jsonl` and numbered `events.<n>.jsonl` archives are also read but were never seen | JSONL Copilot SDK session events, with a byte cursor on the live journal; usage from `session.shutdown` `modelMetrics`, calls from `requests.count` |
| `copilot` | `<editor>/User/workspaceStorage/<hash>/chatSessions/<uuid>.{json,jsonl}` and `<editor>/User/globalStorage/emptyWindowChatSessions/<uuid>.{json,jsonl}` (Code, Code - Insiders, Cursor, VSCodium, Windsurf) | one JSON object `{ kind, v }` per file, turns in `v.requests[]`; project from the workspace's `workspace.json` |

Honor `XDG_DATA_HOME` (Goose/OpenCode) and `OPENCODE_DATA_DIR` where applicable.

## Candidates not yet integrated

- **Charm Crush** (`~/.crush/crush.db`, SQLite), **Factory Droid**
  (`~/.factory/sessions/`, JSONL). Peek at a real file first to confirm the schema.
- **Amp** — server-side (cloud Postgres); needs an authed API, not file indexing.
- **Zed** — thread bodies are LMDB binary; only the SQLite metadata is parseable.
