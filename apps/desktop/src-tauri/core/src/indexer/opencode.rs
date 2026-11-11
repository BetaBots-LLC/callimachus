//! OpenCode indexer.
//!
//! OpenCode (sst/opencode, since moved to anomalyco/opencode) keeps sessions in one of three
//! layouts, depending on its version and on how far its own migration got:
//!
//! | Format     | Location                                                     | When read                     |
//! |------------|--------------------------------------------------------------|-------------------------------|
//! | JSON files | `storage/{session,message,part}/*.json`                      | sessions the DB does not hold |
//! | V1 SQLite  | `session` + `message` + `part` in `opencode.db`              | ids absent from `session_v2`  |
//! | V2 SQLite  | `session_v2` + `session_message` (+ `session_part`), same DB | whenever its tables exist     |
//!
//! When both the JSON tree and the DB exist (OpenCode ran before and after its SQLite
//! migration), `scan_combined` reads both and deduplicates by `external_id`: DB entries take
//! precedence.
//!
//! V1 schema verified against opencode `packages/sdk/js/src/gen/types.gen.ts`. An assistant
//! message carries `modelID` and `tokens: {input, output, reasoning, cache: {read, write}}`,
//! where `input` already excludes the cached tokens; read for spend. A message with no text
//! part (a turn of tool calls only) still spent tokens: its usage lands on the last message.
//!
//! V2 follows OpenCode's source at tag v2.2.7 (2026-11-10); it is not yet run against a live
//! v2 install, only against fixtures built from these definitions:
//! - Tables (`packages/core/src/session/sql.ts`; DDL in the migrations
//!   `20260804233008_loose_psylocke` and `20261029002625_session_part`): `session_v2` (`id`,
//!   `title`, `directory`, `parent_id`, `time_created`, `time_updated` in ms),
//!   `session_message` (`id`, `session_id`, `type`, `seq`, `time_created`, `data` JSON) and,
//!   since v2.2.2, `session_part` (`message_id`, `ordinal`, `type`, `data` JSON: one assistant
//!   content item). v2 writes the same `opencode.db` as v1 (`packages/util/src/global-roots.ts`,
//!   `packages/cli/src/database-path.ts`) and upgrades it in place.
//! - Each table is probed in `sqlite_master` before it is read: a fresh v2 install has no
//!   `session` table at all (bootstrapped from `schema.gen.ts`), and v1.x (npm `latest` is
//!   still 1.19.5) has no `session_v2`. The v1 to v2 copy (`v1-migration.bun.ts`) keeps the
//!   session and message ids, leaves the v1 rows in place and runs in the background, so one
//!   DB can hold a session in v1 only, in both, or in v2 only: `session_v2` wins per id, and
//!   the external id is the same session id, so a migrated session updates its thread. V1 is
//!   read only for the ids the copy has not reached by its own progress row, and for the
//!   sessions updated after that row last moved, which OpenCode 1.x wrote since ([`V1Copy`]):
//!   a session deleted in v2 after the copy keeps its V1 rows, which must not replace its
//!   thread.
//! - A subagent is a session with `parent_id` set; a fork stores `parent_id` NULL and
//!   `fork_session_id` set (`projector.ts`), so it is not a subagent. A fork starts with copies
//!   of its parent's rows, tokens included: they keep their text and bill nothing while the
//!   parent is in `session_v2` or the index, and bill when it is in neither ([`is_fork_copy`]).
//! - Messages are `session_message` rows in `seq` order. A `user` row indexes `data.text`.
//!   An `assistant` row is one model step: its text is the `text` items of its `session_part`
//!   rows by `ordinal` (typed by the column `type`) followed by the `text` items of the
//!   inline `data.content`, the merge `history.ts` `withParts` does (v2.2.0 to v2.2.1 wrote
//!   content inline, v2.2.2+ writes parts and an empty array, and one row can hold both).
//!   Every other row type and part type is counted as skipped ([`SKIPPED_V2_MESSAGES`],
//!   [`SKIPPED_V2_PARTS`]). Rows written before v2.1 name provider data `state` /
//!   `providerState` (`packages/schema/src/session-legacy.ts`); none of those keys is read.
//!   A cell of another storage class than OpenCode writes counts as malformed ([`lenient`]):
//!   it skips its message or part row, and a session row reads it as NULL.
//! - Usage is per assistant row, from `data.tokens` (`input`, `output`, `reasoning`,
//!   `cache.read`, `cache.write`; `input` excludes cached tokens, `core/src/session/usage.ts`)
//!   and `data.model.id` (`packages/schema/src/session-message.ts`, `model.ts`). A step with
//!   no text still carries its usage, a fork's copied step none. `session_v2`'s `tokens_*`
//!   totals are never added on top.
//! - OpenCode runs the DB in WAL mode (`packages/core/src/database/database.ts`), so new turns
//!   sit in `opencode.db-wal` until a checkpoint: the change key covers the `-wal` too (by a
//!   sub-second mtime, [`wal_change_state`]), and a pass that could only open the DB
//!   `immutable=1` (blind to the `-wal`) while the `-wal` holds frames reads nothing.
//!
//! Not read: tool output and reasoning text, blobs and media, shell / skill / compaction
//! text (and the usage of compaction requests), channel databases (`opencode-<channel>.db`),
//! `OPENCODE_DB`, and Kilo CLI's `kilo.db`.

use super::common::{
    begin_file, count_file, count_line, count_malformed, count_skipped, read_json, truncate_title,
};
use super::{
    file_change_state, file_state, open_external_readonly_mode, set_file_state, source_id,
    upsert_thread, IndexReport, MsgUsage, ParsedMessage, ParsedThread,
};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

pub const KIND: &str = "opencode";

/// One row of the V1 `session` table or the V2 `session_v2` table, which share these columns:
/// (id, title, directory, parent_id, time_created, time_updated).
type SessionRow = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<i64>,
);

/// A message assembled from its text parts:
/// (message id, role, time_created, text parts).
type GroupedMessage = (String, Option<String>, Option<i64>, Vec<String>);

/// OpenCode data root. Honors OPENCODE_DATA_DIR (first entry of its list),
/// then XDG_DATA_HOME, else ~/.local/share/opencode.
///
/// Both `storage_root()` and `db_path()` derive from this.
fn data_root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("OPENCODE_DATA_DIR") {
        let first = dir.to_string_lossy();
        let first = first.split(',').next().unwrap_or("").trim();
        if !first.is_empty() {
            return Some(PathBuf::from(first));
        }
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/share")))?;
    Some(base.join("opencode"))
}

/// OpenCode JSON storage root (legacy path). Returns `<data_root>/storage`.
pub fn storage_root() -> Option<PathBuf> {
    data_root().map(|r| r.join("storage"))
}

/// Path to OpenCode's SQLite session DB, V1 and V2 alike (v2.2.7 keeps the file name and
/// upgrades it in place).
///
/// Returns `<data_root>/opencode.db`, the sibling of `storage/`.
///
/// Only the stable channel (`opencode.db`) is supported. Dev/nightly builds
/// may use `opencode-<channel>.db`, which is not yet handled.
///
/// This function intentionally does NOT match `-wal`, `-shm`, or `.backup-*`
/// siblings — only the main DB file.
pub fn db_path() -> Option<PathBuf> {
    data_root().map(|r| r.join("opencode.db"))
}

/// Public scan entry point. When both SQLite and JSON sources exist (user ran
/// OpenCode before and after the SQLite migration), reads both and deduplicates
/// by `external_id` — SQLite entries take precedence.
pub fn scan(conn: &mut Connection, tick: &mut dyn FnMut()) -> Result<IndexReport> {
    let json_root = storage_root().filter(|r| r.join("session").is_dir());
    let sqlite_db = db_path().filter(|p| p.exists());

    match (json_root, sqlite_db) {
        (Some(root), Some(db)) => scan_combined(conn, &root, &db, tick),
        (Some(root), None) => scan_root(conn, &root, tick),
        (None, Some(db)) => scan_sqlite(conn, &db, tick),
        (None, None) => Ok(IndexReport::default()),
    }
}

/// Scan both SQLite and JSON sources. SQLite is authoritative: sessions from
/// SQLite are upserted first, then JSON sessions fill in any gaps (sessions
/// that exist only in the legacy JSON tree).
fn scan_combined(
    conn: &mut Connection,
    root: &Path,
    db: &Path,
    tick: &mut dyn FnMut(),
) -> Result<IndexReport> {
    // SQLite first — these sessions are authoritative.
    let mut report = scan_sqlite(conn, db, tick)?;

    // Collect the external_ids that SQLite just indexed, so we can skip them
    // during the JSON pass.
    let sid = source_id(conn, KIND)?;
    let mut stmt = conn.prepare("SELECT external_id FROM threads WHERE source_id = ?1")?;
    let sqlite_ids: std::collections::HashSet<String> = stmt
        .query_map([sid], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);

    // JSON pass — skip sessions already indexed by SQLite.
    let session_dir = root.join("session");
    if session_dir.is_dir() {
        let mut session_files = Vec::new();
        collect_session_files(&session_dir, &mut session_files);

        for sf in session_files {
            tick();
            // Peek at the session id to check for dedup before doing full work.
            let Some(session_json) = read_json(&sf) else {
                continue;
            };
            let session_id = session_json
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| sf.file_stem().and_then(|s| s.to_str()).map(str::to_string))
                .unwrap_or_default();
            if session_id.is_empty() || sqlite_ids.contains(&session_id) {
                continue;
            }

            report.record_at(&sf, index_session(conn, sid, root, &sf));
        }
    }

    Ok(report)
}

/// Which of OpenCode's session tables a DB holds, probed before any of them is read (a fresh
/// v2 install has no `session` table, and the V1 query failing there made every pass of the
/// whole source an error).
struct Layout {
    /// `session`, `message` and `part`: what v1.x writes.
    v1: bool,
    /// `session_v2` and `session_message`: what v2 writes (v1.19.5 also has a `session_message`
    /// of its own, keyed to `session`, so `session_v2` is what tells them apart).
    v2: bool,
    /// `session_part`, since v2.2.2.
    parts: bool,
}

impl Layout {
    fn probe(ro: &Connection) -> Self {
        let has = |table| crate::db::has_table(ro, table);
        Self {
            v1: has("session") && has("message") && has("part"),
            v2: has("session_v2") && has("session_message"),
            parts: has("session_part"),
        }
    }
}

/// `?1`: every V1 session ([`V1Copy::NotStarted`]); `?2`: the copy's cursor, compared as
/// OpenCode's own `WHERE id < cursor` does; `?3`: its watermark (both NULL when absent).
const V1_SESSIONS: &str =
    "SELECT id, title, directory, parent_id, time_created, time_updated FROM session
     WHERE ?1 OR id < ?2 OR time_updated > ?3";
const V2_SESSIONS: &str =
    "SELECT id, title, directory, parent_id, time_created, time_updated FROM session_v2";

/// How far OpenCode's own copy of the V1 sessions into `session_v2` has got, from its progress
/// row (`kv` key `migration.v1-v2`; v2.2.7 `packages/core/src/database/v1-migration.bun.ts`
/// `run`, `readState`, `parseState`). The copy leaves the V1 rows in place, and deleting a
/// session in v2 removes only its `session_v2` row (`projector.ts`, `SessionEvent.Deleted`;
/// `session_message` cascades). So "absent from `session_v2`" alone would read a session the
/// copy took and the user deleted since from its stale V1 rows, replacing the thread the index
/// holds and dropping every turn added under v2.
///
/// The watermark is the row's `time_updated` (ms, `Timestamps` in
/// `packages/core/src/database/schema.sql.ts`), which `run` sets to `Date.now()` in the
/// transaction that copies each session and again at completion. OpenCode 1.x (npm `latest`
/// is still 1.19.5) keeps writing the V1 tables of a DB v2 has copied, and v2 never copies
/// again once completed, so a V1 session absent from `session_v2` and updated after the
/// watermark was written by 1.x since and is read; one the copy took and v2 deleted since is
/// older and stays out.
#[derive(Clone, Debug, PartialEq)]
enum V1Copy {
    /// No progress row, or `{phase: "sessions"}` with no cursor (written before the first
    /// session is copied): no V1 session was taken yet.
    NotStarted,
    /// `{phase: "sessions", cursor}`: the copy walks V1 ids in descending order and commits the
    /// cursor in the transaction that copies that session, so only the ids below it are still
    /// V1's alone, besides the sessions written after the watermark.
    Below {
        cursor: String,
        watermark: Option<i64>,
    },
    /// `{phase: "completed"}`: every V1 session was copied, and OpenCode never copies again
    /// (`run` and `status` return early). Also a row present that `parseState` rejects (an
    /// unknown phase, a cursor that is not a string): `run` then inserts a fresh row with no
    /// `onConflict`, which fails on the existing key on every launch, so the copy never
    /// restarts and the sessions it took must not be read from V1 again.
    Completed { watermark: Option<i64> },
}

impl V1Copy {
    fn read(ro: &Connection) -> Result<Self> {
        if !crate::db::has_table(ro, "kv") {
            return Ok(Self::NotStarted);
        }
        let Some((value, watermark)) = ro
            .query_row(
                "SELECT value, time_updated FROM kv WHERE key = 'migration.v1-v2'",
                [],
                |r| Ok((r.get::<_, String>(0).ok(), r.get::<_, i64>(1).ok())),
            )
            .optional()?
        else {
            return Ok(Self::NotStarted);
        };
        let state: Value = value
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or_default();
        Ok(
            match (
                state.get("phase").and_then(Value::as_str),
                state.get("cursor"),
            ) {
                (Some("sessions"), None) => Self::NotStarted,
                (Some("sessions"), Some(Value::String(cursor))) => Self::Below {
                    cursor: cursor.clone(),
                    watermark,
                },
                _ => Self::Completed { watermark },
            },
        )
    }

    /// The V1 sessions the copy has not taken yet, and those written after its watermark.
    fn pending_sessions(&self, ro: &Connection) -> Result<Vec<SessionRow>> {
        let (all, cursor, watermark) = match self {
            Self::NotStarted => (true, None, None),
            Self::Below { cursor, watermark } => (false, Some(cursor.as_str()), *watermark),
            Self::Completed { watermark } => (false, None, *watermark),
        };
        read_sessions(ro, V1_SESSIONS, params![all, cursor, watermark])
    }
}

/// Opens OpenCode's DB read-only, saying whether the connection is blind to the `-wal`
/// ([`super::open_external_readonly_mode`]); injected so a test can take the fallback.
type Opener = fn(&Path) -> Result<(Connection, bool)>;

/// `<db>-wal`: where a WAL-mode SQLite DB keeps writes until the next checkpoint.
fn wal_path(db: &Path) -> PathBuf {
    let mut s = db.as_os_str().to_owned();
    s.push("-wal");
    PathBuf::from(s)
}

/// The `-wal`'s change key, `(unchanged, mtime in ns, size)`, like [`file_change_state`] but
/// finer than its whole seconds: once a checkpoint has copied the whole `-wal` back, SQLite
/// restarts it from its first frame without truncating it (OpenCode sets no
/// `journal_size_limit`, `packages/core/src/database/database.ts`), so a write later in the
/// same second as a pass's stamp left both the second and the size as they were and waited
/// for OpenCode's next write.
fn wal_change_state(conn: &Connection, wal: &Path) -> Result<(bool, i64, i64)> {
    let meta = fs::metadata(wal)?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as i64);
    let size = meta.len() as i64;
    let unchanged = matches!(file_state(conn, &wal.to_string_lossy())?, Some((m, s)) if m == mtime && s == size);
    Ok((unchanged, mtime, size))
}

/// Index OpenCode sessions from the SQLite DB: the V2 tables when present, then the V1 tables
/// for the session ids V2 does not hold and its copy has not reached ([`V1Copy`]; see the
/// module doc). Skips if neither the DB nor its `-wal` changed since the last successful pass.
fn scan_sqlite(conn: &mut Connection, db: &Path, tick: &mut dyn FnMut()) -> Result<IndexReport> {
    scan_sqlite_with(conn, db, tick, open_external_readonly_mode)
}

fn scan_sqlite_with(
    conn: &mut Connection,
    db: &Path,
    tick: &mut dyn FnMut(),
    open: Opener,
) -> Result<IndexReport> {
    let mut report = IndexReport::default();
    let sid = source_id(conn, KIND)?;
    // The change key covers the main file AND its WAL: OpenCode writes in WAL mode, so a new
    // turn lands in opencode.db-wal and the main file only changes at a checkpoint.
    let wal = wal_path(db);
    let (main_unchanged, mtime, size) = file_change_state(conn, db)?;
    let wal_state = if wal.exists() {
        Some(wal_change_state(conn, &wal)?)
    } else {
        None
    };
    if main_unchanged
        && wal_state
            .as_ref()
            .is_none_or(|(unchanged, _, _)| *unchanged)
    {
        return Ok(report);
    }

    let (ro, wal_blind) = open(db)?;
    if wal_blind && wal_state.as_ref().is_some_and(|(_, _, size)| *size > 0) {
        // The `immutable=1` fallback reads the main file only, older than the frames in the
        // -wal that an earlier pass may have indexed: an upsert from it would cut those
        // threads back to the last checkpoint. Read nothing and record nothing, so the next
        // pass reads the DB. With no -wal or an empty one the main file is the whole DB.
        app_log!(
            "[index] {}: only an immutable open worked while its -wal holds frames; reading again next pass",
            db.display()
        );
        return Ok(report);
    }
    let layout = Layout::probe(&ro);
    let mut in_v2: HashSet<String> = HashSet::new();
    if layout.v2 {
        for row in &read_sessions(&ro, V2_SESSIONS, [])? {
            tick();
            in_v2.insert(row.0.clone());
            let thread = v2_thread(&ro, row, layout.parts, &|parent| indexed(conn, sid, parent))?;
            store(conn, sid, thread, &mut report)?;
        }
    }
    if layout.v1 {
        let copy = if layout.v2 {
            V1Copy::read(&ro)?
        } else {
            V1Copy::NotStarted
        };
        for row in copy
            .pending_sessions(&ro)?
            .iter()
            .filter(|row| !in_v2.contains(&row.0))
        {
            tick();
            let thread = sqlite_thread(&ro, row)?;
            store(conn, sid, thread, &mut report)?;
        }
    }

    // Record the DB file as indexed ONLY after every session upserted without
    // error: a mid-pass failure returns above via `?`, leaving the file
    // "changed" so the watcher's retry re-indexes it instead of skipping
    // dropped sessions.
    set_file_state(conn, &db.to_string_lossy(), KIND, mtime, size)?;
    if let Some((_, m, s)) = wal_state {
        set_file_state(conn, &wal.to_string_lossy(), KIND, m, s)?;
    }
    Ok(report)
}

fn read_sessions(ro: &Connection, sql: &str, p: impl rusqlite::Params) -> Result<Vec<SessionRow>> {
    let mut stmt = ro.prepare(sql)?;
    let mut rows = Vec::new();
    for row in stmt.query_map(p, session_row)? {
        rows.extend(row?);
    }
    Ok(rows)
}

/// Whether this source already holds a thread for `external_id`.
fn indexed(conn: &Connection, sid: i64, external_id: &str) -> Result<bool> {
    Ok(conn
        .prepare_cached("SELECT 1 FROM threads WHERE source_id = ?1 AND external_id = ?2")?
        .exists(params![sid, external_id])?)
}

/// A decoded cell or row, None when a cell holds another storage class than the one asked for
/// (a REAL or TEXT where OpenCode writes an integer, a BLOB where it writes text: a hand edit
/// or another writer). The caller counts it malformed and skips that row: with `?`, one such
/// cell failed the whole source on every pass. Any other error still fails the pass.
fn lenient<T>(decoded: rusqlite::Result<T>) -> rusqlite::Result<Option<T>> {
    match decoded {
        Ok(v) => Ok(Some(v)),
        Err(
            rusqlite::Error::InvalidColumnType(..)
            | rusqlite::Error::IntegralValueOutOfRange(..)
            | rusqlite::Error::Utf8Error(..)
            | rusqlite::Error::FromSqlConversionFailure(..),
        ) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Upsert one parsed session (None: no text, skipped) into the tally.
fn store(
    conn: &mut Connection,
    sid: i64,
    thread: Option<ParsedThread>,
    report: &mut IndexReport,
) -> Result<()> {
    let Some(thread) = thread else {
        report.threads_skipped += 1;
        return Ok(());
    };
    let n = upsert_thread(conn, sid, &thread)?;
    report.threads_indexed += 1;
    report.messages_indexed += n;
    Ok(())
}

/// One session row; None (counted malformed) when its id is not text. Any other cell of the
/// wrong storage class reads as NULL and counts the row malformed, so the session is still
/// read and still shadows its V1 rows.
fn session_row(r: &rusqlite::Row) -> rusqlite::Result<Option<SessionRow>> {
    fn cell<T: rusqlite::types::FromSql>(
        r: &rusqlite::Row,
        i: usize,
        bad: &mut bool,
    ) -> rusqlite::Result<Option<T>> {
        let v = lenient(r.get::<_, Option<T>>(i))?;
        *bad |= v.is_none();
        Ok(v.flatten())
    }
    let Some(id) = lenient(r.get::<_, String>(0))? else {
        count_malformed();
        return Ok(None);
    };
    let mut bad = false;
    let row = (
        id,
        cell(r, 1, &mut bad)?, // title
        cell(r, 2, &mut bad)?, // directory
        cell(r, 3, &mut bad)?, // parent_id
        cell(r, 4, &mut bad)?, // time_created
        cell(r, 5, &mut bad)?, // time_updated
    );
    if bad {
        count_malformed();
    }
    Ok(Some(row))
}

/// One session of the SQLite DB, read for the usage backfill (`indexer::usage_backfill`):
/// the same thread [`scan_sqlite`] stores, from `session_v2` when it holds the id, else from
/// the V1 tables. None when the session is gone or has no text.
///
/// Unlike [`scan_sqlite`], it reads the V1 rows of a session the copy took ([`V1Copy`]) and
/// v2 deleted since: the backfill only fills usage onto a thread that has none, one an older
/// reader stored from those very rows, and it writes only when the message counts still match.
/// A fork's copies stay unbilled here as if its parent were indexed: when the scan billed
/// them instead (the parent in neither place), the fork's thread has usage and is no
/// candidate.
pub(super) fn sqlite_session_thread(
    ro: &Connection,
    session_id: &str,
) -> Result<Option<ParsedThread>> {
    let layout = Layout::probe(ro);
    if layout.v2 {
        let row = ro
            .prepare_cached(
                "SELECT id, title, directory, parent_id, time_created, time_updated
                 FROM session_v2 WHERE id = ?1",
            )?
            .query_row([session_id], session_row)
            .optional()?
            .flatten();
        if let Some(row) = row {
            return v2_thread(ro, &row, layout.parts, &|_| Ok(true));
        }
    }
    if !layout.v1 {
        return Ok(None);
    }
    let row = ro
        .prepare_cached(
            "SELECT id, title, directory, parent_id, time_created, time_updated
             FROM session WHERE id = ?1",
        )?
        .query_row([session_id], session_row)
        .optional()?
        .flatten();
    match row {
        Some(row) => sqlite_thread(ro, &row),
        None => Ok(None),
    }
}

/// `session_message.type` values other than `user` and `assistant`, skipped on purpose:
/// notices, skill and shell runs, compactions, idle markers and the switch markers (v2.2.7
/// `packages/schema/src/session-message.ts`, the `Info` union).
const SKIPPED_V2_MESSAGES: &[&str] = &[
    "synthetic",
    "system",
    "skill",
    "shell",
    "compaction",
    "idle",
    "agent-switched",
    "model-switched",
    "location-switched",
];

/// Assistant content types other than `text`, skipped on purpose (v2.2.7
/// `AssistantContent`: text, reasoning, tool).
const SKIPPED_V2_PARTS: &[&str] = &["reasoning", "tool"];

/// Whether the row `id` at `seq` of a fork is its copy of a parent row. `projectFork` starts a
/// fork with the parent's settled rows, copied with the same `seq`, `time_created` and `data`
/// (tokens included) under the id `<the Forked event's id as msg_...>_<seq>` (v2.2.7
/// `packages/core/src/session/projector.ts` `forkedID`, the same scheme since v2.0.0), and
/// starts the fork's own totals at 0. No other writer puts a `_` after the `msg_` prefix
/// (`Identifier.ascending` in `packages/schema/src/identifier.ts`, `syntheticID` in
/// `v1-migration.bun.ts`), so the copy is known from the fork's rows alone. The copies bill
/// nothing while the parent's thread counts them: the parent is in `session_v2`, or deleted
/// since and still in the index, which keeps its thread. A parent deleted before it was ever
/// indexed counts them nowhere, so then they bill ([`v2_thread`]). Not covered: a parent
/// reverted past the fork point (`RevertEvent.Committed` deletes its rows from the boundary
/// on) loses those rows' usage on its next read, and the fork's copies of them stay unbilled.
fn is_fork_copy(id: &str, seq: i64) -> bool {
    id.rsplit_once('_')
        .is_some_and(|(head, tail)| head.contains('_') && tail == seq.to_string())
}

/// Build one session's thread from the V2 tables (see the module doc). None when it has no
/// text messages. `parent_indexed` says whether the index holds a thread for a fork's parent.
fn v2_thread(
    ro: &Connection,
    row: &SessionRow,
    parts: bool,
    parent_indexed: &dyn Fn(&str) -> Result<bool>,
) -> Result<Option<ParsedThread>> {
    type MessageRow = (String, Option<String>, i64, Option<i64>, Option<String>);
    fn message_row(r: &rusqlite::Row) -> rusqlite::Result<MessageRow> {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
    }
    // A fork's parent, and whether `session_v2` still holds it (a parent id that is not text
    // reads as one never indexed).
    let fork: Option<(String, bool)> = ro
        .prepare_cached(
            "SELECT s.fork_session_id,
                    EXISTS (SELECT 1 FROM session_v2 p WHERE p.id = s.fork_session_id)
             FROM session_v2 s WHERE s.id = ?1 AND s.fork_session_id IS NOT NULL",
        )?
        .query_row([&row.0], |r| {
            Ok((lenient(r.get(0))?.unwrap_or_default(), r.get(1)?))
        })
        .optional()?;
    // Its copies bill nothing while the parent's thread counts them ([`is_fork_copy`]).
    let copies_counted = match &fork {
        Some((parent, in_v2)) => *in_v2 || parent_indexed(parent)?,
        None => false,
    };
    let rows: Vec<Option<MessageRow>> = ro
        .prepare_cached(
            "SELECT id, type, seq, time_created, data FROM session_message
             WHERE session_id = ?1 ORDER BY seq",
        )?
        .query_map([&row.0], |r| lenient(message_row(r)))?
        .collect::<rusqlite::Result<_>>()?;
    // Point reads on the (message_id, ordinal) primary key, the order `withParts` merges in.
    let mut part_stmt = if parts {
        Some(ro.prepare_cached(
            "SELECT type, data FROM session_part WHERE message_id = ?1 ORDER BY ordinal",
        )?)
    } else {
        None
    };

    let file = begin_file();
    let mut thread = ParsedThread::default();
    let mut first_user: Option<String> = None;
    for decoded in rows {
        let Some((msg_id, kind, seq, ts_ms, data)) = decoded else {
            count_malformed();
            continue;
        };
        let Some(data) = data.and_then(|d| serde_json::from_str::<Value>(&d).ok()) else {
            count_malformed();
            continue;
        };
        count_line();
        let ts = ts_ms.map(|ms| ms / 1000);
        let (role, text, usage) = match kind.as_deref() {
            Some("user") => (
                "user",
                data.get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
                None,
            ),
            Some("assistant") => {
                let mut texts = Vec::new();
                if let Some(stmt) = part_stmt.as_mut() {
                    let mut q = stmt.query([&msg_id])?;
                    while let Some(r) = q.next()? {
                        let Some(part_type) = lenient(r.get::<_, Option<String>>(0))? else {
                            count_malformed();
                            continue;
                        };
                        if part_type.as_deref() != Some("text") {
                            count_skipped("part:", part_type.as_deref(), SKIPPED_V2_PARTS);
                            continue;
                        }
                        match lenient(r.get::<_, Option<String>>(1))?
                            .flatten()
                            .and_then(|d| serde_json::from_str::<Value>(&d).ok())
                        {
                            Some(part) => push_text(&mut texts, &part),
                            None => count_malformed(),
                        }
                    }
                }
                for item in data
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let item_type = item.get("type").and_then(Value::as_str);
                    if item_type == Some("text") {
                        push_text(&mut texts, item);
                    } else {
                        count_skipped("part:", item_type, SKIPPED_V2_PARTS);
                    }
                }
                // A fork's copy keeps its text (the fork shows the whole conversation) but not
                // its usage while the parent's thread counts it.
                let usage = if copies_counted && is_fork_copy(&msg_id, seq) {
                    None
                } else {
                    v2_usage(&data)
                };
                ("assistant", texts.join("\n"), usage)
            }
            other => {
                count_skipped("message:", other, SKIPPED_V2_MESSAGES);
                continue;
            }
        };
        if text.is_empty() {
            // A step of tool calls only: its tokens still count.
            if let Some(u) = usage {
                thread.attach_usage(None, u);
            }
            continue;
        }
        if role == "user" && first_user.is_none() {
            first_user = Some(text.clone());
        }
        thread.messages.push(ParsedMessage {
            role: role.to_string(),
            text,
            tool_name: None,
            ts,
        });
        if let Some(u) = usage {
            thread.attach_usage(Some(thread.messages.len() - 1), u);
        }
    }
    let ParsedThread {
        messages, usage, ..
    } = thread;
    count_file(file, messages.len());
    Ok(db_thread(row, messages, usage, first_user))
}

/// Append a content item's trimmed `text`, when it has one.
fn push_text(texts: &mut Vec<String>, item: &Value) {
    if let Some(t) = item.get("text").and_then(Value::as_str) {
        let t = t.trim();
        if !t.is_empty() {
            texts.push(t.to_string());
        }
    }
}

/// The thread of one DB session row (V1 or V2) and its parsed messages. None when it has no
/// messages.
fn db_thread(
    row: &SessionRow,
    messages: Vec<ParsedMessage>,
    usage: Vec<(usize, MsgUsage)>,
    first_user: Option<String>,
) -> Option<ParsedThread> {
    if messages.is_empty() {
        return None;
    }
    let (session_id, title, directory, parent_id, time_created, time_updated) = row;
    let title = title
        .as_deref()
        .filter(|t| !t.trim().is_empty())
        .map(str::to_string)
        .or_else(|| first_user.as_deref().map(truncate_title));
    Some(ParsedThread {
        external_id: session_id.clone(),
        title,
        project_path: directory
            .as_deref()
            .filter(|d| !d.is_empty())
            .map(str::to_string),
        git_branch: None,
        created_at: time_created.map(|ms| ms / 1000),
        updated_at: time_updated.map(|ms| ms / 1000),
        is_subagent: parent_id.is_some(),
        usage,
        messages,
    })
}

/// Build one session's thread from the V1 SQLite DB. None when it has no text messages.
fn sqlite_thread(ro: &Connection, row: &SessionRow) -> Result<Option<ParsedThread>> {
    let session_id = &row.0;
    let mut msg_stmt = ro.prepare_cached(
        "SELECT m.id, json_extract(m.data, '$.role'), m.time_created,
                json_extract(p.data, '$.text')
         FROM message m
         JOIN part p ON p.message_id = m.id
         WHERE m.session_id = ?1 AND json_extract(p.data, '$.type') = 'text'
         ORDER BY m.time_created, m.id, p.id",
    )?;

    // Every assistant message's usage, in the text query's order: a message with no text part
    // is missing from that query but spent tokens all the same.
    let mut usage_stmt = ro.prepare_cached(
        "SELECT id, time_created, data FROM message
         WHERE session_id = ?1 AND json_extract(data, '$.role') = 'assistant'
           AND json_extract(data, '$.tokens') IS NOT NULL
         ORDER BY time_created, id",
    )?;
    let spent: Vec<(String, Option<i64>, MsgUsage)> = usage_stmt
        .query_map(params![session_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?
        .filter_map(|row| {
            let (id, ts, data) = row.ok()?;
            let data: Value = serde_json::from_str(&data?).ok()?;
            Some((id, ts, message_usage(&data)?))
        })
        .collect();
    let mut spent = spent.into_iter().peekable();

    // Group text parts by message id, concatenating with \n.
    let rows = msg_stmt.query_map(params![session_id], |r| {
        Ok((
            r.get::<_, String>(0)?,         // message id
            r.get::<_, Option<String>>(1)?, // role
            r.get::<_, Option<i64>>(2)?,    // time_created
            r.get::<_, Option<String>>(3)?, // text
        ))
    })?;

    let mut grouped: Vec<GroupedMessage> = Vec::new();
    for row in rows {
        let (msg_id, role, ts, text) = row?;
        if let Some(last) = grouped.last_mut() {
            if last.0 == msg_id {
                if let Some(t) = text {
                    let t = t.trim().to_string();
                    if !t.is_empty() {
                        last.3.push(t);
                    }
                }
                continue;
            }
        }
        let mut parts = Vec::new();
        if let Some(t) = text {
            let t = t.trim().to_string();
            if !t.is_empty() {
                parts.push(t);
            }
        }
        grouped.push((msg_id, role, ts, parts));
    }

    let file = begin_file();
    let mut thread = ParsedThread::default();
    let mut first_user: Option<String> = None;

    for (msg_id, role, ts_ms, parts) in grouped {
        // Usage of the text-less messages ordered before this one goes on the last message.
        while let Some((id, at, _)) = spent.peek() {
            if (*at, id.as_str()) >= (ts_ms, msg_id.as_str()) {
                break;
            }
            let (_, _, u) = spent.next().expect("peeked");
            thread.attach_usage(None, u);
        }
        let own = spent.next_if(|(id, _, _)| *id == msg_id).map(|(_, _, u)| u);
        count_line();
        let role = match role.as_deref() {
            Some("assistant") => "assistant",
            Some("user") => "user",
            other => {
                count_skipped("role:", other, &[]);
                continue;
            }
        };
        let text = parts.join("\n");
        let at = if text.is_empty() {
            None
        } else {
            if role == "user" && first_user.is_none() {
                first_user = Some(text.clone());
            }
            thread.messages.push(ParsedMessage {
                role: role.to_string(),
                text,
                tool_name: None,
                ts: ts_ms.map(|ms| ms / 1000),
            });
            Some(thread.messages.len() - 1)
        };
        if let Some(u) = own {
            thread.attach_usage(at, u);
        }
    }
    for (_, _, u) in spent {
        thread.attach_usage(None, u);
    }
    let ParsedThread {
        messages, usage, ..
    } = thread;

    count_file(file, messages.len());
    Ok(db_thread(row, messages, usage, first_user))
}

fn scan_root(conn: &mut Connection, root: &Path, tick: &mut dyn FnMut()) -> Result<IndexReport> {
    let mut report = IndexReport::default();
    let session_dir = root.join("session");
    if !session_dir.is_dir() {
        return Ok(report);
    }
    let sid = source_id(conn, KIND)?;

    let mut session_files = Vec::new();
    collect_session_files(&session_dir, &mut session_files);

    for sf in session_files {
        tick();
        report.record_at(&sf, index_session(conn, sid, root, &sf));
    }
    Ok(report)
}

/// Collect `ses_*.json` files under session/<projectID>/.
fn collect_session_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_session_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("json")
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("ses_"))
        {
            out.push(path);
        }
    }
}

fn index_session(conn: &mut Connection, sid: i64, root: &Path, sf: &Path) -> Result<Option<usize>> {
    let session = read_json(sf).with_context(|| format!("reading {}", sf.display()))?;
    let session_id = session
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| sf.file_stem().and_then(|s| s.to_str()).map(str::to_string))
        .unwrap_or_default();
    if session_id.is_empty() {
        return Ok(None);
    }

    let msg_dir = root.join("message").join(&session_id);
    // Change fingerprint for the session: (max message-file mtime, message count).
    // Avoids re-upserting (and re-embedding) unchanged sessions every pass.
    let (fp_mtime, fp_count) = dir_fingerprint(&msg_dir);
    let key = sf.to_string_lossy().to_string();
    if let Some((prev_mtime, prev_count)) = file_state(conn, &key)? {
        if prev_mtime == fp_mtime && prev_count == fp_count {
            return Ok(None);
        }
    }

    let Some(thread) = session_thread(root, &session, session_id) else {
        // An empty session has no upsert that could fail: safe to record now.
        set_file_state(conn, &key, KIND, fp_mtime, fp_count)?;
        return Ok(Some(0));
    };
    let n = upsert_thread(conn, sid, &thread)?;
    // Record the session fingerprint ONLY after the upsert succeeds, so a failed upsert
    // (e.g. a transient write-lock) leaves the fingerprint stale and the next pass retries
    // instead of silently skipping this session forever.
    set_file_state(conn, &key, KIND, fp_mtime, fp_count)?;
    Ok(Some(n))
}

/// The JSON tree's session file `sf`, read for the usage backfill
/// (`indexer::usage_backfill`): the same thread [`index_session`] stores. The storage root
/// is the directory above `session/`.
pub(super) fn json_session_thread(sf: &Path) -> Result<Option<ParsedThread>> {
    let Some(root) = sf
        .ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == "session"))
        .and_then(Path::parent)
    else {
        return Ok(None);
    };
    let session = read_json(sf).with_context(|| format!("reading {}", sf.display()))?;
    let session_id = session
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| sf.file_stem().and_then(|s| s.to_str()).map(str::to_string))
        .unwrap_or_default();
    if session_id.is_empty() {
        return Ok(None);
    }
    Ok(session_thread(root, &session, session_id))
}

/// Build one JSON-tree session's thread from its message and part files. None when it has
/// no text messages.
fn session_thread(root: &Path, session: &Value, session_id: String) -> Option<ParsedThread> {
    let msg_dir = root.join("message").join(&session_id);
    let file = begin_file();
    let mut thread = ParsedThread::default();
    let mut first_user: Option<String> = None;
    let mut max_ts: Option<i64> = None;

    let mut msg_files = read_dir_sorted(&msg_dir);
    msg_files.sort();
    for mf in msg_files {
        let Some(msg) = read_json(&mf) else {
            count_malformed();
            continue;
        };
        count_line();
        let role = msg.get("role").and_then(Value::as_str);
        let role = match role {
            Some("assistant") => "assistant",
            Some("user") => "user",
            _ => {
                count_skipped("role:", role, &[]);
                continue;
            }
        };
        let ts = msg
            .get("time")
            .and_then(|t| t.get("created"))
            .and_then(Value::as_i64)
            .map(|ms| ms / 1000);
        if let Some(ts) = ts {
            max_ts = Some(max_ts.map_or(ts, |m: i64| m.max(ts)));
        }
        let message_id = msg.get("id").and_then(Value::as_str).unwrap_or_default();
        let text = read_parts_text(root, message_id);
        let text = text.trim().to_string();
        let usage = message_usage(&msg);
        if text.is_empty() {
            // A turn of tool calls only: its tokens still count.
            if let Some(u) = usage {
                thread.attach_usage(None, u);
            }
            continue;
        }
        if role == "user" && first_user.is_none() {
            first_user = Some(text.clone());
        }
        thread.messages.push(ParsedMessage {
            role: role.to_string(),
            text,
            tool_name: None,
            ts,
        });
        if let Some(u) = usage {
            thread.attach_usage(Some(thread.messages.len() - 1), u);
        }
    }
    let ParsedThread {
        messages, usage, ..
    } = thread;

    count_file(file, messages.len());
    if messages.is_empty() {
        return None;
    }

    let created_at = session
        .get("time")
        .and_then(|t| t.get("created"))
        .and_then(Value::as_i64)
        .map(|ms| ms / 1000);
    let title = session
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|t| !t.trim().is_empty())
        .or_else(|| first_user.as_deref().map(truncate_title));
    let project_path = session
        .get("directory")
        .and_then(Value::as_str)
        .filter(|d| !d.is_empty())
        .map(str::to_string);

    // NOTE: The JSON path hardcodes `is_subagent: false` because the JSON file
    // tree has no parent_id concept. The SQLite path sets `is_subagent` from
    // `parent_id IS NOT NULL`. When both sources exist, SQLite entries (with
    // correct is_subagent) take precedence via dedup in scan_combined.
    let thread = ParsedThread {
        external_id: session_id,
        title,
        project_path,
        git_branch: None,
        created_at,
        updated_at: max_ts.or(created_at),
        is_subagent: false,
        usage,
        messages,
    };
    Some(thread)
}

/// An assistant message's model and tokens, or None for a user message or one that has
/// not reported usage yet (all zero while it streams).
fn message_usage(msg: &Value) -> Option<MsgUsage> {
    if msg.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let model = msg.get("modelID").and_then(Value::as_str)?;
    token_usage(model, msg.get("tokens")?)
}

/// A V2 assistant step's model (`data.model.id`, a `Model.Ref`) and tokens (`data.tokens`, a
/// `TokenUsage.Info` of the same shape as V1's), or None while it has reported nothing.
fn v2_usage(data: &Value) -> Option<MsgUsage> {
    let model = data
        .get("model")
        .and_then(|m| m.get("id"))
        .and_then(Value::as_str)?;
    token_usage(model, data.get("tokens")?)
}

/// `{input, output, reasoning, cache: {read, write}}` as usage: reasoning is billed as output.
fn token_usage(model: &str, t: &Value) -> Option<MsgUsage> {
    let n = |v: Option<&Value>| v.and_then(Value::as_i64).unwrap_or(0).max(0);
    let cache = t.get("cache");
    let u = MsgUsage {
        model: model.to_string(),
        input: n(t.get("input")),
        output: n(t.get("output")) + n(t.get("reasoning")),
        cache_write: n(cache.and_then(|c| c.get("write"))),
        cache_read: n(cache.and_then(|c| c.get("read"))),
    };
    (u.input + u.output + u.cache_write + u.cache_read > 0).then_some(u)
}

/// Concatenate the text of all `type:"text"` parts of a message, ordered by file.
fn read_parts_text(root: &Path, message_id: &str) -> String {
    if message_id.is_empty() {
        return String::new();
    }
    let part_dir = root.join("part").join(message_id);
    let mut files = read_dir_sorted(&part_dir);
    files.sort();
    let mut out = Vec::new();
    for pf in files {
        let Some(part) = read_json(&pf) else {
            count_malformed();
            continue;
        };
        let kind = part.get("type").and_then(Value::as_str);
        if kind == Some("text") {
            if let Some(t) = part.get("text").and_then(Value::as_str) {
                let t = t.trim();
                if !t.is_empty() {
                    out.push(t.to_string());
                }
            }
        } else {
            count_skipped("part:", kind, SKIPPED_PARTS);
        }
    }
    out.join("\n")
}

/// Part types of the JSON tree skipped on purpose (the parse canary, `parse_health`): tool
/// calls and their output, step markers, reasoning, attached files, patches and snapshots.
/// Only `text` parts are indexed; the SQLite path selects them in SQL.
const SKIPPED_PARTS: &[&str] = &[
    "tool",
    "step-start",
    "step-finish",
    "reasoning",
    "file",
    "patch",
    "snapshot",
    "agent",
    "subtask",
    "retry",
    "compaction",
];

fn read_dir_sorted(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect()
}

/// (max mtime secs, file count) of a directory's immediate files.
fn dir_fingerprint(dir: &Path) -> (i64, i64) {
    let files = read_dir_sorted(dir);
    let count = files.len() as i64;
    let max_mtime = files
        .iter()
        .filter_map(|p| fs::metadata(p).ok())
        .filter_map(|m| m.modified().ok())
        .filter_map(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .max()
        .unwrap_or(0);
    (max_mtime, count)
}

/// OpenCode DB fixtures for the tests here and in `usage_backfill`. The V2 DDL is OpenCode's
/// own at tag v2.2.7 (`packages/core/src/database/migration/20260804233008_loose_psylocke.ts`
/// for `session_v2`, `session_message` and `kv`, `20261029002625_session_part.ts` for
/// `session_part`), cut to the columns the reader reads plus the NOT NULL ones; row data
/// follows `packages/schema/src/session-message.ts`. The V1 DDL is the cut the V1 tests use.
#[cfg(test)]
pub(super) mod fixture {
    use rusqlite::{params, Connection};
    use serde_json::Value;

    pub const V1_DDL: &str = "
        CREATE TABLE session (id TEXT PRIMARY KEY, title TEXT, directory TEXT, parent_id TEXT,
                              time_created INTEGER, time_updated INTEGER);
        CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
        CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, data TEXT);";

    pub const V2_DDL: &str = "
        CREATE TABLE `session_v2` (
          `id` text PRIMARY KEY,
          `project_id` text NOT NULL,
          `parent_id` text,
          `fork_session_id` text,
          `slug` text NOT NULL,
          `directory` text NOT NULL,
          `title` text,
          `version` text NOT NULL,
          `cost` real DEFAULT 0 NOT NULL,
          `tokens_input` integer DEFAULT 0 NOT NULL,
          `tokens_output` integer DEFAULT 0 NOT NULL,
          `tokens_reasoning` integer DEFAULT 0 NOT NULL,
          `tokens_cache_read` integer DEFAULT 0 NOT NULL,
          `tokens_cache_write` integer DEFAULT 0 NOT NULL,
          `time_created` integer NOT NULL,
          `time_updated` integer NOT NULL
        );
        CREATE TABLE `session_message` (
          `id` text PRIMARY KEY,
          `session_id` text NOT NULL,
          `type` text NOT NULL,
          `seq` integer NOT NULL,
          `time_created` integer NOT NULL,
          `time_updated` integer NOT NULL,
          `data` text NOT NULL
        );
        CREATE UNIQUE INDEX `session_message_session_seq_idx` ON `session_message` (`session_id`,`seq`);
        CREATE TABLE IF NOT EXISTS `kv` (
          `key` text PRIMARY KEY,
          `value` text NOT NULL,
          `time_created` integer NOT NULL,
          `time_updated` integer NOT NULL
        );";

    /// Since v2.2.2.
    pub const PARTS_DDL: &str = "
        CREATE TABLE `session_part` (
          `message_id` text NOT NULL,
          `session_id` text NOT NULL,
          `ordinal` integer NOT NULL,
          `type` text NOT NULL,
          `call_id` text,
          `time_created` integer NOT NULL,
          `time_updated` integer NOT NULL,
          `data` text NOT NULL,
          CONSTRAINT `session_part_pk` PRIMARY KEY(`message_id`, `ordinal`)
        );
        CREATE INDEX `session_part_session_message_idx` ON `session_part` (`session_id`,`message_id`,`ordinal`);";

    /// Milliseconds, as OpenCode stores every `time_*` column.
    pub const T0: i64 = 1_780_000_000_000;

    pub fn session(
        c: &Connection,
        id: &str,
        title: Option<&str>,
        parent: Option<&str>,
        fork: Option<&str>,
    ) {
        c.execute(
            "INSERT INTO session_v2 (id, project_id, parent_id, fork_session_id, slug, directory,
                                     title, version, time_created, time_updated)
             VALUES (?1, 'prj_1', ?2, ?3, 'quiet-otter', '/v2/proj', ?4, '2.2.7', ?5, ?6)",
            params![id, parent, fork, title, T0, T0 + 60_000],
        )
        .unwrap();
    }

    pub fn message(c: &Connection, id: &str, session: &str, seq: i64, kind: &str, data: Value) {
        let at = T0 + seq * 1000;
        c.execute(
            "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)",
            params![id, session, kind, seq, at, data.to_string()],
        )
        .unwrap();
    }

    pub fn part(c: &Connection, message: &str, session: &str, ordinal: i64, data: Value) {
        let kind = data["type"].as_str().unwrap().to_string();
        let call_id = (kind == "tool").then(|| data["id"].as_str().unwrap().to_string());
        c.execute(
            "INSERT INTO session_part (message_id, session_id, ordinal, type, call_id,
                                       time_created, time_updated, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7)",
            params![
                message,
                session,
                ordinal,
                kind,
                call_id,
                T0,
                data.to_string()
            ],
        )
        .unwrap();
    }

    /// `data.model`, a `Model.Ref`.
    pub fn model() -> Value {
        serde_json::json!({"id": "claude-sonnet-4-5", "providerID": "anthropic"})
    }

    /// `data.tokens`, a `TokenUsage.Info`.
    pub fn tokens(input: i64, output: i64, reasoning: i64, read: i64, write: i64) -> Value {
        serde_json::json!({
            "input": input, "output": output, "reasoning": reasoning,
            "cache": {"read": read, "write": write}
        })
    }

    pub fn user(text: &str) -> Value {
        serde_json::json!({"text": text, "time": {"created": T0}})
    }

    /// An assistant step with inline `content` and, when given, `tokens`.
    pub fn assistant(content: Value, tokens: Option<Value>) -> Value {
        let mut data = serde_json::json!({
            "agent": "build", "model": model(), "content": content,
            "time": {"created": T0, "completed": T0 + 500}
        });
        if let Some(t) = tokens {
            data["tokens"] = t;
        }
        data
    }

    /// A completed tool call as an assistant content item (`AssistantTool`).
    pub fn tool(id: &str, output: &str) -> Value {
        serde_json::json!({
            "type": "tool", "id": id, "name": "bash",
            "state": {"status": "completed", "input": {"command": "ls"},
                      "content": [{"type": "text", "text": output}]},
            "time": {"created": T0, "completed": T0 + 100}
        })
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::*;
    use super::*;
    use crate::db::test_support::TestDir;
    use crate::indexer::{file_stamp, open_external_readonly};
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::fs::create_dir_all;

    /// (role, text, model, [input, output, cache_write, cache_read]) of a thread's rows.
    type UsageRow = (String, String, Option<String>, [Option<i64>; 4]);

    fn thread_rows(conn: &Connection, external_id: &str) -> Vec<UsageRow> {
        conn.prepare(
            "SELECT m.role, m.text, m.model, m.input_tokens, m.output_tokens,
                    m.cache_write_tokens, m.cache_read_tokens
             FROM messages m JOIN threads t ON t.id = m.thread_id
             WHERE t.external_id = ?1 ORDER BY m.seq",
        )
        .unwrap()
        .query_map([external_id], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                [r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?],
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    }

    fn thread_meta(conn: &Connection, external_id: &str) -> (i64, Option<String>, bool) {
        conn.query_row(
            "SELECT id, title, is_subagent FROM threads WHERE external_id = ?1",
            [external_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    }

    /// A fresh v2 install (no V1 tables at all, where the V1 query used to fail the whole
    /// scan): every content shape `withParts` merges, usage per step, a child and a fork, the
    /// skipped row and part types, and pre-v2.1 keys.
    #[test]
    fn reads_a_fresh_v2_database() {
        let dir = TestDir::new("oc_v2_fresh");
        let src = dir.with_file_name("opencode.db");
        {
            let c = Connection::open(&src).unwrap();
            c.execute_batch(V2_DDL).unwrap();
            c.execute_batch(PARTS_DDL).unwrap();
            session(&c, "ses_main", Some("V2 main"), None, None);
            // The session totals OpenCode keeps: never added on top of the per-step usage.
            c.execute(
                "UPDATE session_v2 SET tokens_input = 999999, tokens_output = 999999
                 WHERE id = 'ses_main'",
                [],
            )
            .unwrap();
            let msg = |id, seq, kind, data| message(&c, id, "ses_main", seq, kind, data);
            let prt = |id, ordinal, data| part(&c, id, "ses_main", ordinal, data);
            let text = |t: &str| json!({"type": "text", "text": t});
            msg("msg_01", 0, "user", user("index the v2 store"));
            // v2.2.0 to v2.2.1: content inline, no part rows.
            let inline = json!([
                text("inline answer"),
                {"type": "reasoning", "text": "private reasoning"},
                tool("call_1", "tool output text"),
            ]);
            msg(
                "msg_02",
                1,
                "assistant",
                assistant(inline, Some(tokens(10, 5, 1, 100, 20))),
            );
            let notice = json!({"text": "a synthetic notice", "time": {"created": T0}});
            msg("msg_03", 2, "synthetic", notice);
            // v2.2.2+: part rows and an empty inline array.
            msg(
                "msg_04",
                3,
                "assistant",
                assistant(json!([]), Some(tokens(20, 6, 0, 0, 0))),
            );
            prt("msg_04", 0, text("parts one"));
            prt(
                "msg_04",
                1,
                json!({"type": "reasoning", "text": "private reasoning"}),
            );
            prt("msg_04", 2, text("parts two"));
            // Both: the parts by ordinal (inserted out of order), then the inline content.
            let inline = json!([text("then inline")]);
            msg(
                "msg_05",
                4,
                "assistant",
                assistant(inline, Some(tokens(30, 7, 0, 0, 0))),
            );
            prt("msg_05", 1, text("part second"));
            prt("msg_05", 0, text("part first"));
            // A step of tool calls only: its tokens land on the message before it.
            msg(
                "msg_06",
                5,
                "assistant",
                assistant(json!([]), Some(tokens(40, 8, 0, 0, 0))),
            );
            prt("msg_06", 0, tool("call_2", "more tool output"));
            let compaction = json!({
                "status": "completed", "reason": "auto", "summary": "compaction summary",
                "tokens": tokens(500, 50, 0, 0, 0), "time": {"created": T0}
            });
            msg("msg_07", 6, "compaction", compaction);
            msg(
                "msg_08",
                7,
                "idle",
                json!({"outcome": "succeeded", "time": {"created": T0}}),
            );
            // Written before v2.1: provider data under `providerState` / `state`.
            let mut legacy = assistant(
                json!([{"type": "text", "text": "legacy keyed text", "state": {"k": "v"}}]),
                Some(tokens(50, 9, 0, 0, 0)),
            );
            legacy["providerState"] = json!({"k": "v"});
            msg("msg_09", 8, "assistant", legacy);
            // An invented row type and an invented content type: the parse canary names them.
            msg("msg_10", 9, "hologram", json!({}));
            msg(
                "msg_11",
                10,
                "assistant",
                assistant(json!([{"type": "sculpture"}]), None),
            );

            session(&c, "ses_child", None, Some("ses_main"), None);
            message(&c, "msg_c1", "ses_child", 0, "user", user("child task"));
            let step = assistant(json!([text("child answer")]), None);
            message(&c, "msg_c2", "ses_child", 1, "assistant", step);
            session(&c, "ses_fork", Some("Fork of main"), None, Some("ses_main"));
            message(&c, "msg_f1", "ses_fork", 0, "user", user("fork prompt"));
        }
        let mut conn = crate::db::open(&dir).unwrap();
        let (report, stats) = crate::indexer::common::collect_parse_stats(|| {
            scan_sqlite(&mut conn, &src, &mut || {}).unwrap()
        });
        assert_eq!((report.threads_indexed, report.errors), (3, 0));
        assert_eq!(report.messages_indexed, 5 + 2 + 1);
        assert_eq!((stats.lines, stats.malformed_lines), (14, 0));
        assert_eq!((stats.files, stats.files_with_zero_messages), (3, 0));
        assert_eq!(
            stats.unknown_kinds,
            BTreeMap::from([
                ("message:hologram".to_string(), 1),
                ("part:sculpture".to_string(), 1),
            ])
        );

        // [input, output (reasoning included), cache_write, cache_read] per stored row.
        let row = |role: &str, text: &str, u: Option<[i64; 4]>| -> UsageRow {
            let model = u.map(|_| "claude-sonnet-4-5".to_string());
            (
                role.into(),
                text.into(),
                model,
                u.map_or([None; 4], |u| u.map(Some)),
            )
        };
        assert_eq!(
            thread_rows(&conn, "ses_main"),
            vec![
                row("user", "index the v2 store", None),
                row("assistant", "inline answer", Some([10, 6, 20, 100])),
                row("assistant", "parts one\nparts two", Some([20, 6, 0, 0])),
                row(
                    "assistant",
                    "part first\npart second\nthen inline",
                    Some([30 + 40, 7 + 8, 0, 0])
                ),
                row("assistant", "legacy keyed text", Some([50, 9, 0, 0])),
            ]
        );
        let all_text: String = conn
            .query_row("SELECT group_concat(text, ' | ') FROM messages", [], |r| {
                r.get(0)
            })
            .unwrap();
        for left_out in ["tool output", "reasoning", "synthetic", "compaction"] {
            assert!(!all_text.contains(left_out), "{left_out} in {all_text}");
        }

        let (_, title, sub) = thread_meta(&conn, "ses_main");
        assert_eq!((title.as_deref(), sub), (Some("V2 main"), false));
        let (_, title, sub) = thread_meta(&conn, "ses_child");
        assert_eq!((title.as_deref(), sub), (Some("child task"), true));
        let (_, _, sub) = thread_meta(&conn, "ses_fork");
        assert!(!sub, "a fork has no parent_id, so it is not a subagent");
        let project: String = conn
            .query_row(
                "SELECT project_path FROM threads WHERE external_id = 'ses_main'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(project, "/v2/proj");

        // The usage backfill's reader takes the same branch, and a missing id is no error.
        let ro = open_external_readonly(&src).unwrap();
        let t = sqlite_session_thread(&ro, "ses_main").unwrap().unwrap();
        assert_eq!((t.messages.len(), t.usage.len()), (5, 4));
        assert!(sqlite_session_thread(&ro, "ses_gone").unwrap().is_none());

        let again = scan_sqlite(&mut conn, &src, &mut || {}).unwrap();
        assert_eq!((again.threads_indexed, again.threads_skipped), (0, 0));
    }

    /// A cell of another storage class than OpenCode writes (a hand edit, another writer)
    /// skips its message or part row as malformed, and a session row reads it as NULL: the
    /// strict decode failed the whole source on every pass.
    #[test]
    fn a_cell_of_the_wrong_storage_class_skips_its_row() {
        let dir = TestDir::new("oc_v2_bad_cell");
        let src = dir.with_file_name("opencode.db");
        {
            let c = Connection::open(&src).unwrap();
            c.execute_batch(V2_DDL).unwrap();
            c.execute_batch(PARTS_DDL).unwrap();
            let set = |sql: &str| c.execute(sql, []).unwrap();
            session(&c, "ses_odd", Some("Odd cells"), None, None);
            // Text that is not a number stays TEXT in an `integer` column; a BLOB stays BLOB.
            set("UPDATE session_v2 SET time_created = 'soon'");
            let msg = |id, seq, kind, data| message(&c, id, "ses_odd", seq, kind, data);
            msg("msg_1", 0, "user", user("a readable prompt"));
            msg("msg_2", 1, "user", user("a blob data cell"));
            set("UPDATE session_message SET data = X'7B7D' WHERE id = 'msg_2'");
            msg("msg_3", 2, "user", user("a text time cell"));
            set("UPDATE session_message SET time_created = 'later' WHERE id = 'msg_3'");
            msg("msg_4", 3, "assistant", assistant(json!([]), None));
            let text = |t: &str| json!({"type": "text", "text": t});
            part(&c, "msg_4", "ses_odd", 0, text("a readable part"));
            part(&c, "msg_4", "ses_odd", 1, text("a blob part"));
            set("UPDATE session_part SET data = X'7B7D' WHERE ordinal = 1");
        }
        let mut conn = crate::db::open(&dir).unwrap();
        let (report, stats) = crate::indexer::common::collect_parse_stats(|| {
            scan_sqlite(&mut conn, &src, &mut || {}).unwrap()
        });
        assert_eq!((report.threads_indexed, report.errors), (1, 0));
        assert_eq!(
            texts(&conn, "ses_odd"),
            ["a readable prompt", "a readable part"]
        );
        assert_eq!(
            stats.malformed_lines, 4,
            "the session, two messages, a part"
        );
        let (_, title, _) = thread_meta(&conn, "ses_odd");
        assert_eq!(title.as_deref(), Some("Odd cells"));
    }

    /// An upgraded DB: OpenCode v2 keeps the V1 tables beside the new ones and copies sessions
    /// over in the background under the same ids. A copied session updates its thread (V2
    /// wins), one not copied yet is still read from V1, and a v2-only one is read too.
    #[test]
    fn an_upgraded_database_prefers_v2_and_keeps_v1_only_sessions() {
        let dir = TestDir::new("oc_v2_upgrade");
        let src = dir.with_file_name("opencode.db");
        {
            let c = Connection::open(&src).unwrap();
            c.execute_batch(V1_DDL).unwrap();
            c.execute_batch(
                "INSERT INTO session VALUES
                   ('ses_both', 'Both, v1 title', '/v1/proj', NULL, 1780000000000, 1780000001000),
                   ('ses_a_v1only', 'V1 only', '/v1/proj', NULL, 1780000000000, 1780000001000);
                 INSERT INTO message VALUES
                   ('msg_b1', 'ses_both', 1780000000000, json('{\"role\":\"user\"}')),
                   ('msg_o1', 'ses_a_v1only', 1780000000000, json('{\"role\":\"user\"}'));
                 INSERT INTO part VALUES
                   ('prt_b1', 'msg_b1', json('{\"type\":\"text\",\"text\":\"asked under v1\"}')),
                   ('prt_o1', 'msg_o1', json('{\"type\":\"text\",\"text\":\"not copied yet\"}'));",
            )
            .unwrap();
        }
        let mut conn = crate::db::open(&dir).unwrap();
        let first = scan_sqlite(&mut conn, &src, &mut || {}).unwrap();
        assert_eq!(first.threads_indexed, 2);
        let (both_id, title, _) = thread_meta(&conn, "ses_both");
        assert_eq!(title.as_deref(), Some("Both, v1 title"));

        // v2 upgrades the file in place, and its copy has reached ses_both only: it walks the
        // ids in descending order, so ses_a_v1only, below the cursor, is still to come.
        {
            let c = Connection::open(&src).unwrap();
            c.execute_batch(V2_DDL).unwrap();
            c.execute_batch(PARTS_DDL).unwrap();
            c.execute(
                "INSERT INTO kv VALUES ('migration.v1-v2', ?1, ?2, ?2)",
                params![r#"{"phase":"sessions","cursor":"ses_both"}"#, T0],
            )
            .unwrap();
            session(&c, "ses_both", Some("Both, v2 title"), None, None);
            message(&c, "msg_b1", "ses_both", 0, "user", user("asked under v2"));
            let step = assistant(json!([{"type": "text", "text": "answered under v2"}]), None);
            message(&c, "msg_b2", "ses_both", 1, "assistant", step);
            session(&c, "ses_new", Some("Started in v2"), None, None);
            message(&c, "msg_n1", "ses_new", 0, "user", user("v2 only"));
        }
        let second = scan_sqlite(&mut conn, &src, &mut || {}).unwrap();
        assert_eq!((second.threads_indexed, second.errors), (3, 0));
        let threads: i64 = conn
            .query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0))
            .unwrap();
        assert_eq!(threads, 3, "the migrated session is not duplicated");
        let (id, title, _) = thread_meta(&conn, "ses_both");
        assert_eq!((id, title.as_deref()), (both_id, Some("Both, v2 title")));
        let texts: Vec<String> = thread_rows(&conn, "ses_both")
            .into_iter()
            .map(|r| r.1)
            .collect();
        assert_eq!(texts, ["asked under v2", "answered under v2"]);
        let texts: Vec<String> = thread_rows(&conn, "ses_a_v1only")
            .into_iter()
            .map(|r| r.1)
            .collect();
        assert_eq!(texts, ["not copied yet"]);
        assert_eq!(thread_rows(&conn, "ses_new").len(), 1);

        let ro = open_external_readonly(&src).unwrap();
        let t = sqlite_session_thread(&ro, "ses_both").unwrap().unwrap();
        assert_eq!(t.title.as_deref(), Some("Both, v2 title"));
        let t = sqlite_session_thread(&ro, "ses_a_v1only").unwrap().unwrap();
        assert_eq!(t.title.as_deref(), Some("V1 only"));
    }

    /// Forget the DB's stamps so the next pass reads it: a delete inside one second changes
    /// neither the file's mtime (seconds) nor its size.
    fn forget_stamps(conn: &Connection) {
        conn.execute("DELETE FROM index_state WHERE source_kind = ?1", [KIND])
            .unwrap();
    }

    fn texts(conn: &Connection, external_id: &str) -> Vec<String> {
        thread_rows(conn, external_id)
            .into_iter()
            .map(|r| r.1)
            .collect()
    }

    /// `V1Copy::read` reads the progress row as v2.2.7 `parseState` does, with its
    /// `time_updated` as the watermark. A row present that `parseState` rejects reads as a
    /// completed copy: `run` then fails on every launch inserting a row whose key exists.
    #[test]
    fn the_copy_progress_reads_as_opencode_reads_it() {
        let dir = TestDir::new("oc_v2_kv");
        let c = Connection::open(dir.with_file_name("opencode.db")).unwrap();
        let read = || V1Copy::read(&c).unwrap();
        assert_eq!(read(), V1Copy::NotStarted, "no kv table");
        c.execute_batch(V2_DDL).unwrap();
        assert_eq!(read(), V1Copy::NotStarted, "no progress row");
        let done = V1Copy::Completed {
            watermark: Some(T0),
        };
        for (value, want) in [
            (r#"{"phase":"sessions"}"#, V1Copy::NotStarted),
            (
                r#"{"phase":"sessions","cursor":"ses_x"}"#,
                V1Copy::Below {
                    cursor: "ses_x".into(),
                    watermark: Some(T0),
                },
            ),
            (r#"{"phase":"completed"}"#, done.clone()),
            (r#"{"phase":"sessions","cursor":7}"#, done.clone()),
            (r#"{"phase":"sessions","cursor":null}"#, done.clone()),
            (r#"{"phase":"messages"}"#, done.clone()),
            ("not json", done.clone()),
        ] {
            c.execute(
                "INSERT OR REPLACE INTO kv VALUES ('migration.v1-v2', ?1, 0, ?2)",
                params![value, T0],
            )
            .unwrap();
            assert_eq!(read(), want, "{value}");
        }
        c.execute(
            "UPDATE kv SET time_updated = 'soon' WHERE key = 'migration.v1-v2'",
            [],
        )
        .unwrap();
        assert_eq!(read(), V1Copy::Completed { watermark: None });
    }

    /// OpenCode's delete removes the `session_v2` row (`session_message` cascades), and the
    /// copy leaves the V1 rows behind. Once the copy has taken a session (by its cursor, or by
    /// completing), V1 is not read for it again, so its thread keeps the turns added under v2.
    #[test]
    fn a_session_deleted_in_v2_after_the_copy_keeps_its_thread() {
        let dir = TestDir::new("oc_v2_deleted");
        let src = dir.with_file_name("opencode.db");
        let c = Connection::open(&src).unwrap();
        c.execute_batch(V1_DDL).unwrap();
        c.execute_batch(V2_DDL).unwrap();
        c.execute_batch(
            "INSERT INTO session VALUES
               ('ses_1_pending', 'Pending', '/v1/proj', NULL, 1780000000000, 1780000001000),
               ('ses_2_copied', 'Copied', '/v1/proj', NULL, 1780000000000, 1780000001000);
             INSERT INTO message VALUES
               ('msg_p1', 'ses_1_pending', 1780000000000, json('{\"role\":\"user\"}')),
               ('msg_c1', 'ses_2_copied', 1780000000000, json('{\"role\":\"user\"}'));
             INSERT INTO part VALUES
               ('prt_p1', 'msg_p1', json('{\"type\":\"text\",\"text\":\"first prompt under v1\"}')),
               ('prt_c1', 'msg_c1', json('{\"type\":\"text\",\"text\":\"first prompt under v1\"}'));",
        )
        .unwrap();
        // The copy runs after 1.x last wrote these sessions: its watermark is later.
        let progress = |state: &str| {
            c.execute(
                "INSERT OR REPLACE INTO kv VALUES ('migration.v1-v2', ?1, ?2, ?2)",
                params![state, T0 + 3_600_000],
            )
            .unwrap();
        };
        // The copy keeps the session and message ids; two more turns follow under v2.
        let copy_and_continue = |id: &str, m: &str| {
            session(&c, id, Some("Copied"), None, None);
            message(
                &c,
                &format!("msg_{m}1"),
                id,
                0,
                "user",
                user("first prompt under v1"),
            );
            message(
                &c,
                &format!("msg_{m}2"),
                id,
                1,
                "user",
                user("then a quokka question"),
            );
            let step = assistant(json!([{"type": "text", "text": "a quokka answer"}]), None);
            message(&c, &format!("msg_{m}3"), id, 2, "assistant", step);
        };
        let delete_in_v2 = |id: &str| {
            c.execute("DELETE FROM session_message WHERE session_id = ?1", [id])
                .unwrap();
            c.execute("DELETE FROM session_v2 WHERE id = ?1", [id])
                .unwrap();
        };
        let v2_turns = [
            "first prompt under v1",
            "then a quokka question",
            "a quokka answer",
        ];

        // The copy walks the ids downwards: it has taken ses_2_copied, not ses_1_pending.
        progress(r#"{"phase":"sessions","cursor":"ses_2_copied"}"#);
        copy_and_continue("ses_2_copied", "c");
        let mut conn = crate::db::open(&dir).unwrap();
        let r = scan_sqlite(&mut conn, &src, &mut || {}).unwrap();
        assert_eq!(r.threads_indexed, 2);
        assert_eq!(texts(&conn, "ses_2_copied"), v2_turns);
        assert_eq!(texts(&conn, "ses_1_pending"), ["first prompt under v1"]);

        delete_in_v2("ses_2_copied");
        forget_stamps(&conn);
        let r = scan_sqlite(&mut conn, &src, &mut || {}).unwrap();
        assert_eq!(r.threads_indexed, 1, "ses_1_pending only");
        assert_eq!(texts(&conn, "ses_2_copied"), v2_turns);

        // The copy completes; then that session is deleted in v2 too.
        copy_and_continue("ses_1_pending", "p");
        progress(r#"{"phase":"completed"}"#);
        forget_stamps(&conn);
        scan_sqlite(&mut conn, &src, &mut || {}).unwrap();
        assert_eq!(texts(&conn, "ses_1_pending"), v2_turns);
        delete_in_v2("ses_1_pending");
        forget_stamps(&conn);
        let r = scan_sqlite(&mut conn, &src, &mut || {}).unwrap();
        assert_eq!((r.threads_indexed, r.errors), (0, 0));
        assert_eq!(texts(&conn, "ses_1_pending"), v2_turns);
        assert_eq!(texts(&conn, "ses_2_copied"), v2_turns);

        // The usage backfill still reads the V1 rows (see `sqlite_session_thread`).
        let ro = open_external_readonly(&src).unwrap();
        let t = sqlite_session_thread(&ro, "ses_2_copied").unwrap().unwrap();
        assert_eq!(t.messages.len(), 1);
    }

    /// OpenCode 1.x keeps writing the V1 tables of a DB whose copy v2 completed, and v2 never
    /// copies again: a V1 session updated after the progress row's watermark is read, one the
    /// copy took and v2 deleted since is older and is not. A row `parseState` rejects reads
    /// the same way.
    #[test]
    fn a_v1_session_written_after_the_copy_is_read() {
        for state in [r#"{"phase":"completed"}"#, r#"{"phase":"messages"}"#] {
            let dir = TestDir::new("oc_v2_late_v1");
            let src = dir.with_file_name("opencode.db");
            let c = Connection::open(&src).unwrap();
            c.execute_batch(V1_DDL).unwrap();
            c.execute_batch(V2_DDL).unwrap();
            let copied_at = T0 + 3_600_000;
            c.execute(
                "INSERT INTO kv VALUES ('migration.v1-v2', ?1, ?2, ?2)",
                params![state, copied_at],
            )
            .unwrap();
            let v1_session = |id: &str, updated: i64, text: &str| {
                let m = format!("msg_{id}");
                c.execute(
                    "INSERT INTO session VALUES (?1, 'V1', '/v1/proj', NULL, ?2, ?2)",
                    params![id, updated],
                )
                .unwrap();
                c.execute(
                    "INSERT INTO message VALUES (?1, ?2, ?3, json('{\"role\":\"user\"}'))",
                    params![m, id, updated],
                )
                .unwrap();
                c.execute(
                    "INSERT INTO part VALUES (?1, ?2, json_object('type', 'text', 'text', ?3))",
                    params![format!("prt_{id}"), m, text],
                )
                .unwrap();
            };
            // Copied, then deleted in v2: its `session_v2` row is gone, its V1 rows stay.
            v1_session("ses_b_copied", T0, "copied, then deleted in v2");
            v1_session(
                "ses_a_late",
                copied_at + 60_000,
                "written by 1.x after the copy",
            );
            let mut conn = crate::db::open(&dir).unwrap();
            let r = scan_sqlite(&mut conn, &src, &mut || {}).unwrap();
            assert_eq!((r.threads_indexed, r.errors), (1, 0), "{state}");
            assert_eq!(
                texts(&conn, "ses_a_late"),
                ["written by 1.x after the copy"],
                "{state}"
            );
            assert!(texts(&conn, "ses_b_copied").is_empty(), "{state}");
        }
    }

    /// A fork as `projectFork` makes it (v2.2.7 `projector.ts`): the parent's rows before the
    /// boundary copied with the same `seq`, `time_created` and `data` (tokens included) and
    /// their parts, under `<the Forked event's id as msg_...>_<seq>`, then the fork's own rows
    /// from the next seq. The copies keep their text and bill nothing, so one model call is
    /// counted once, also after the parent is deleted (its thread stays in the index).
    #[test]
    fn a_fork_bills_only_its_own_steps() {
        assert!(is_fork_copy("msg_0a1b2c3d4e5fXyZ_3", 3));
        assert!(!is_fork_copy("msg_0a1b2c3d4e5fXyZ_3", 4), "its own seq");
        assert!(!is_fork_copy("msg_3", 3), "no `_` after the prefix");

        let dir = TestDir::new("oc_v2_fork");
        let src = dir.with_file_name("opencode.db");
        let c = Connection::open(&src).unwrap();
        c.execute_batch(V2_DDL).unwrap();
        c.execute_batch(PARTS_DDL).unwrap();
        let text = |t: &str| json!({"type": "text", "text": t});
        let step = |content: Value, input: i64| {
            assistant(content, Some(tokens(input, input / 100, 0, 0, 0)))
        };
        // The parent's seqs have gaps, as the event sequence they come from does.
        session(&c, "ses_parent", Some("Cache policy"), None, None);
        let parent = [
            ("msg_p1", 0, "user", user("pick a cache policy")),
            (
                "msg_p2",
                1,
                "assistant",
                step(json!([text("an LRU fits")]), 100_000),
            ),
            ("msg_p3", 3, "assistant", step(json!([]), 2_000)),
            ("msg_p4", 5, "user", user("now add an expiry")),
            (
                "msg_p5",
                6,
                "assistant",
                step(json!([text("expiry added")]), 4_000),
            ),
        ];
        for (id, seq, kind, data) in &parent {
            message(&c, id, "ses_parent", *seq, kind, data.clone());
        }
        part(&c, "msg_p3", "ses_parent", 0, text("the hit rate is high"));
        // Forked before msg_p4: the copies end at seq 3, and the fork's own rows start at 4.
        let fork = Some("ses_parent");
        session(&c, "ses_fork", Some("Cache policy (fork #1)"), None, fork);
        for (_, seq, kind, data) in &parent[..3] {
            let id = format!("msg_0a1b2c3d4e5fXyZ_{seq}");
            message(&c, &id, "ses_fork", *seq, kind, data.clone());
        }
        part(
            &c,
            "msg_0a1b2c3d4e5fXyZ_3",
            "ses_fork",
            0,
            text("the hit rate is high"),
        );
        message(
            &c,
            "msg_f1",
            "ses_fork",
            4,
            "user",
            user("try a TTL instead"),
        );
        let own = step(json!([text("a TTL of ten minutes")]), 3_000);
        message(&c, "msg_f2", "ses_fork", 5, "assistant", own);

        let input = |conn: &Connection, id: &str| -> Vec<(String, Option<i64>)> {
            thread_rows(conn, id)
                .into_iter()
                .map(|r| (r.1, r.3[0]))
                .collect()
        };
        let row = |t: &str, i: Option<i64>| (t.to_string(), i);
        let fork_rows = vec![
            row("pick a cache policy", None),
            row("an LRU fits", None),
            row("the hit rate is high", None),
            row("try a TTL instead", None),
            row("a TTL of ten minutes", Some(3_000)),
        ];
        let total = |conn: &Connection| -> i64 {
            conn.query_row("SELECT SUM(input_tokens) FROM messages", [], |r| r.get(0))
                .unwrap()
        };
        let mut conn = crate::db::open(&dir).unwrap();
        scan_sqlite(&mut conn, &src, &mut || {}).unwrap();
        assert_eq!(input(&conn, "ses_fork"), fork_rows);
        assert_eq!(
            input(&conn, "ses_parent"),
            vec![
                row("pick a cache policy", None),
                row("an LRU fits", Some(100_000)),
                row("the hit rate is high", Some(2_000)),
                row("now add an expiry", None),
                row("expiry added", Some(4_000)),
            ]
        );
        assert_eq!(total(&conn), 100_000 + 2_000 + 4_000 + 3_000);

        c.execute_batch(
            "DELETE FROM session_part WHERE session_id = 'ses_parent';
             DELETE FROM session_message WHERE session_id = 'ses_parent';
             DELETE FROM session_v2 WHERE id = 'ses_parent';",
        )
        .unwrap();
        forget_stamps(&conn);
        let r = scan_sqlite(&mut conn, &src, &mut || {}).unwrap();
        assert_eq!(r.threads_indexed, 1);
        assert_eq!(input(&conn, "ses_fork"), fork_rows);
        assert_eq!(total(&conn), 100_000 + 2_000 + 4_000 + 3_000);
    }

    /// A parent deleted in OpenCode before Callimachus ever read it (any v2 session before
    /// this reader) is in neither `session_v2` nor the index: the fork's copies are the only
    /// record of those steps, so they bill.
    #[test]
    fn a_fork_whose_parent_was_never_indexed_bills_its_copies() {
        let dir = TestDir::new("oc_v2_fork_orphan");
        let src = dir.with_file_name("opencode.db");
        let c = Connection::open(&src).unwrap();
        c.execute_batch(V2_DDL).unwrap();
        let step = |t: &str, input: i64| {
            let content = json!([{"type": "text", "text": t}]);
            assistant(content, Some(tokens(input, input / 100, 0, 0, 0)))
        };
        let fork = Some("ses_gone");
        session(&c, "ses_fork", Some("Cache policy (fork #1)"), None, fork);
        let copy = |seq: i64| format!("msg_0a1b2c3d4e5fXyZ_{seq}");
        let ask = user("pick a cache policy");
        message(&c, &copy(0), "ses_fork", 0, "user", ask);
        let answer = step("an LRU fits", 100_000);
        message(&c, &copy(1), "ses_fork", 1, "assistant", answer);
        message(&c, "msg_f1", "ses_fork", 2, "user", user("try a TTL"));
        let own = step("a TTL of ten minutes", 3_000);
        message(&c, "msg_f2", "ses_fork", 3, "assistant", own);

        let mut conn = crate::db::open(&dir).unwrap();
        scan_sqlite(&mut conn, &src, &mut || {}).unwrap();
        let input: Vec<Option<i64>> = thread_rows(&conn, "ses_fork")
            .into_iter()
            .map(|r| r.3[0])
            .collect();
        assert_eq!(input, [None, Some(100_000), None, Some(3_000)]);
    }

    /// A WAL-mode DB with OpenCode's connection still open, as v2.2.7 runs it: the first
    /// session is checkpointed into the main file, the rest stay in `opencode.db-wal`.
    fn wal_fixture(src: &Path) -> Connection {
        let w = Connection::open(src).unwrap();
        w.pragma_update(None, "journal_mode", "WAL").unwrap();
        w.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        w.execute_batch(V2_DDL).unwrap();
        w.execute_batch(PARTS_DDL).unwrap();
        session(&w, "ses_a", Some("Checkpointed"), None, None);
        message(&w, "msg_a1", "ses_a", 0, "user", user("in the main file"));
        w.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
        w
    }

    /// New turns sit in the -wal until a checkpoint, with the main file untouched: the change
    /// key covers the -wal, so the next pass reads them.
    #[test]
    fn a_turn_still_in_the_wal_is_read() {
        let dir = TestDir::new("oc_v2_wal");
        let src = dir.with_file_name("opencode.db");
        let w = wal_fixture(&src);
        let (_, wal_blind) = open_external_readonly_mode(&src).unwrap();
        assert!(!wal_blind, "a plain read-only open sees the -wal");
        let mut conn = crate::db::open(&dir).unwrap();
        assert_eq!(
            scan_sqlite(&mut conn, &src, &mut || {})
                .unwrap()
                .threads_indexed,
            1
        );

        let main_before = file_stamp(&src).unwrap();
        let step = assistant(json!([{"type": "text", "text": "a step in the wal"}]), None);
        message(&w, "msg_a2", "ses_a", 1, "assistant", step);
        assert_eq!(file_stamp(&src).unwrap(), main_before);
        let r = scan_sqlite(&mut conn, &src, &mut || {}).unwrap();
        assert_eq!((r.threads_indexed, r.messages_indexed), (1, 1));
        assert_eq!(thread_rows(&conn, "ses_a")[1].1, "a step in the wal");

        let r = scan_sqlite(&mut conn, &src, &mut || {}).unwrap();
        assert_eq!((r.threads_indexed, r.threads_skipped), (0, 0), "unchanged");
        drop(w);
    }

    /// A -wal that a checkpoint fully copied back is restarted from its first frame at the
    /// same size, so a write in the same second as a pass's stamp matched a whole-second key;
    /// the -wal's key has a sub-second mtime.
    #[test]
    fn a_same_size_wal_write_in_the_same_second_is_read() {
        let dir = TestDir::new("oc_v2_wal_ns");
        let src = dir.with_file_name("opencode.db");
        let wal = wal_path(&src);
        let w = wal_fixture(&src);
        for (seq, t) in [(1, "second turn"), (2, "third turn")] {
            message(
                &w,
                &format!("msg_a{}", seq + 1),
                "ses_a",
                seq,
                "user",
                user(t),
            );
        }
        w.execute_batch("PRAGMA wal_checkpoint(PASSIVE);").unwrap();
        let at = |nanos| {
            let t = std::time::UNIX_EPOCH + std::time::Duration::new(1_780_000_000, nanos);
            let f = fs::OpenOptions::new().write(true).open(&wal).unwrap();
            f.set_modified(t).unwrap();
        };
        at(250_000_000);
        let mut conn = crate::db::open(&dir).unwrap();
        scan_sqlite(&mut conn, &src, &mut || {}).unwrap();
        assert_eq!(texts(&conn, "ses_a").len(), 3);

        let (main_before, wal_before) = (file_stamp(&src).unwrap(), file_stamp(&wal).unwrap());
        message(
            &w,
            "msg_a4",
            "ses_a",
            3,
            "user",
            user("same second, same size"),
        );
        at(750_000_000);
        assert_eq!(file_stamp(&src).unwrap(), main_before, "no checkpoint");
        assert_eq!(file_stamp(&wal).unwrap(), wal_before, "restarted in place");
        let r = scan_sqlite(&mut conn, &src, &mut || {}).unwrap();
        assert_eq!(r.messages_indexed, 1);
        assert_eq!(texts(&conn, "ses_a")[3], "same second, same size");
        drop(w);
    }

    /// The `immutable=1` fallback reads the main file only. While the -wal holds frames, that
    /// view is older than what an earlier pass may have indexed from them, so the pass reads
    /// nothing (an upsert would cut a thread back to the last checkpoint) and records
    /// nothing. With an empty -wal the main file is the whole DB: the pass reads and stamps.
    #[test]
    fn a_pass_blind_to_a_nonempty_wal_reads_nothing() {
        let dir = TestDir::new("oc_v2_immutable");
        let src = dir.with_file_name("opencode.db");
        let wal = wal_path(&src);
        let w = wal_fixture(&src);
        let immutable: Opener = |p| {
            let uri = format!("file:{}?immutable=1", p.display());
            let flags =
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI;
            Ok((Connection::open_with_flags(uri, flags)?, true))
        };
        let stamps = |conn: &Connection| {
            let state = |p: &Path| file_state(conn, &p.to_string_lossy()).unwrap();
            (state(&src), state(&wal))
        };
        let mut conn = crate::db::open(&dir).unwrap();
        assert_eq!(file_stamp(&wal).unwrap().1, 0, "checkpointed and truncated");
        let r = scan_sqlite_with(&mut conn, &src, &mut || {}, immutable).unwrap();
        assert_eq!(r.threads_indexed, 1);
        assert!(matches!(stamps(&conn), (Some(_), Some(_))));

        message(&w, "msg_a2", "ses_a", 1, "user", user("a turn in the wal"));
        scan_sqlite(&mut conn, &src, &mut || {}).unwrap();
        let both = ["in the main file", "a turn in the wal"];
        assert_eq!(texts(&conn, "ses_a"), both);
        let recorded = stamps(&conn);

        session(&w, "ses_b", Some("Only in the wal"), None, None);
        message(&w, "msg_b1", "ses_b", 0, "user", user("not checkpointed"));
        let r = scan_sqlite_with(&mut conn, &src, &mut || {}, immutable).unwrap();
        assert_eq!((r.threads_indexed, r.messages_indexed), (0, 0));
        assert_eq!(texts(&conn, "ses_a"), both, "not cut back to the main file");
        assert_eq!(stamps(&conn), recorded);

        let r = scan_sqlite(&mut conn, &src, &mut || {}).unwrap();
        assert_eq!(r.threads_indexed, 2);
        assert_eq!(texts(&conn, "ses_b"), ["not checkpointed"]);
        drop(w);
    }

    fn temp_dir(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("callimachus_{name}_{}_{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    /// The parse canary over the JSON tree: an invented role, an invented part type and a
    /// broken message file are counted; tool and step parts are skipped on purpose.
    #[test]
    fn parse_counts_name_unknown_types_and_broken_files() {
        let dir = crate::db::test_support::TestDir::new("oc_canary");
        let root = dir.with_file_name("storage");
        for d in ["session/p", "message/ses_1", "part/m1", "part/m2"] {
            create_dir_all(root.join(d)).unwrap();
        }
        let write = |rel: &str, text: &str| fs::write(root.join(rel), text).unwrap();
        write(
            "session/p/ses_1.json",
            r#"{"id":"ses_1","directory":"/p","time":{"created":1}}"#,
        );
        write("message/ses_1/a_m1.json", r#"{"id":"m1","role":"user"}"#);
        write(
            "message/ses_1/b_m2.json",
            r#"{"id":"m2","role":"narrator"}"#,
        );
        write("message/ses_1/c_m3.json", r#"{"id":"m3","role":"#);
        write("part/m1/0.json", r#"{"type":"text","text":"hi"}"#);
        write("part/m1/1.json", r#"{"type":"step-start"}"#);
        write("part/m1/2.json", r#"{"type":"hologram"}"#);
        let mut conn = crate::db::open(&dir).unwrap();
        let (report, stats) = crate::indexer::common::collect_parse_stats(|| {
            scan_root(&mut conn, &root, &mut || {}).unwrap()
        });
        assert_eq!(report.messages_indexed, 1);
        assert_eq!((stats.lines, stats.malformed_lines), (3, 1));
        assert_eq!(
            stats.unknown_kinds,
            std::collections::BTreeMap::from([
                ("part:hologram".to_string(), 1),
                ("role:narrator".to_string(), 1),
            ])
        );
        assert_eq!((stats.files, stats.files_with_zero_messages), (1, 0));
    }

    /// Build an OpenCode-shaped storage tree and verify reconstruction.
    #[test]
    fn extracts_from_opencode_tree() {
        let root = temp_dir("oc_store");
        create_dir_all(root.join("session/proj1")).unwrap();
        create_dir_all(root.join("message/ses_abc")).unwrap();
        create_dir_all(root.join("part/msg_1")).unwrap();
        create_dir_all(root.join("part/msg_2")).unwrap();

        fs::write(
            root.join("session/proj1/ses_abc.json"),
            r#"{"id":"ses_abc","title":"FTS5 work","directory":"/Users/me/proj","time":{"created":1780000000000}}"#,
        )
        .unwrap();
        fs::write(
            root.join("message/ses_abc/msg_001_msg_1.json"),
            r#"{"id":"msg_1","sessionID":"ses_abc","role":"user","time":{"created":1780000000000}}"#,
        )
        .unwrap();
        fs::write(
            root.join("message/ses_abc/msg_002_msg_2.json"),
            r#"{"id":"msg_2","sessionID":"ses_abc","role":"assistant","time":{"created":1780000005000}}"#,
        )
        .unwrap();
        fs::write(
            root.join("part/msg_1/000_p1.json"),
            r#"{"id":"p1","messageID":"msg_1","type":"text","text":"index opencode threads with sqlite fts5"}"#,
        )
        .unwrap();
        fs::write(
            root.join("part/msg_2/000_p2.json"),
            r#"{"id":"p2","messageID":"msg_2","type":"text","text":"Sure, using FTS5"}"#,
        )
        .unwrap();

        let dst = temp_dir("oc_dst");
        create_dir_all(&dst).unwrap();
        let mut conn = crate::db::open(&dst.join("db.sqlite")).unwrap();
        let report = scan_root(&mut conn, &root, &mut || {}).unwrap();
        assert_eq!(report.threads_indexed, 1);
        assert_eq!(report.messages_indexed, 2);

        let hits =
            crate::search::search(&conn, "fts5", &crate::search::SearchFilters::default()).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.source == "opencode"));

        let tid: i64 = conn
            .query_row("SELECT id FROM threads", [], |r| r.get(0))
            .unwrap();
        let detail = crate::search::thread_detail(&conn, tid).unwrap().unwrap();
        assert_eq!(detail.title.as_deref(), Some("FTS5 work"));
        assert_eq!(detail.project_path.as_deref(), Some("/Users/me/proj"));

        // Second scan with no changes is skipped (fingerprint match).
        let report2 = scan_root(&mut conn, &root, &mut || {}).unwrap();
        assert_eq!(report2.threads_indexed, 0);
        assert_eq!(report2.threads_skipped, 1);
    }

    /// Build a synthetic V1 SQLite DB and verify scan_sqlite output.
    #[test]
    fn extracts_from_opencode_sqlite_v1() {
        let src = temp_dir("oc_sqlite_src.db");
        let _ = std::fs::remove_file(&src);
        {
            let c = Connection::open(&src).unwrap();
            c.execute_batch(
                "CREATE TABLE session (
                    id TEXT PRIMARY KEY,
                    title TEXT,
                    directory TEXT,
                    parent_id TEXT,
                    time_created INTEGER,
                    time_updated INTEGER
                );
                CREATE TABLE message (
                    id TEXT PRIMARY KEY,
                    session_id TEXT,
                    time_created INTEGER,
                    data TEXT
                );
                CREATE TABLE part (
                    id TEXT PRIMARY KEY,
                    message_id TEXT,
                    data TEXT
                );
                INSERT INTO session VALUES
                    ('ses_abc', 'FTS5 work', '/Users/me/proj', NULL, 1780000000000, 1780000010000),
                    ('ses_sub', NULL, '/Users/me/proj', 'ses_abc', 1780000020000, 1780000030000);
                INSERT INTO message VALUES
                    ('msg_1', 'ses_abc', 1780000000000, json('{\"role\":\"user\"}')),
                    ('msg_2', 'ses_abc', 1780000005000, json('{\"role\":\"assistant\"}')),
                    ('msg_3', 'ses_sub', 1780000020000, json('{\"role\":\"user\"}')),
                    ('msg_4', 'ses_sub', 1780000025000, json('{\"role\":\"assistant\"}'));
                INSERT INTO part VALUES
                    ('p1', 'msg_1', json('{\"type\":\"text\",\"text\":\"index opencode threads with sqlite fts5\"}')),
                    ('p2', 'msg_2', json('{\"type\":\"text\",\"text\":\"Sure, using FTS5\"}')),
                    ('p3', 'msg_3', json('{\"type\":\"text\",\"text\":\"subagent task\"}')),
                    ('p4', 'msg_4', json('{\"type\":\"text\",\"text\":\"subagent response\"}'));",
            )
            .unwrap();
        }

        let dst = temp_dir("oc_sqlite_dst.db");
        let _ = std::fs::remove_file(&dst);
        let mut conn = crate::db::open(&dst).unwrap();
        let report = scan_sqlite(&mut conn, &src, &mut || {}).unwrap();
        assert_eq!(report.threads_indexed, 2);
        assert_eq!(report.messages_indexed, 4);

        let hits =
            crate::search::search(&conn, "fts5", &crate::search::SearchFilters::default()).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.source == "opencode"));

        // Verify the main thread
        let (tid, is_sub): (i64, bool) = conn
            .query_row(
                "SELECT id, is_subagent FROM threads WHERE external_id = 'ses_abc'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let detail = crate::search::thread_detail(&conn, tid).unwrap().unwrap();
        assert_eq!(detail.title.as_deref(), Some("FTS5 work"));
        assert_eq!(detail.project_path.as_deref(), Some("/Users/me/proj"));
        assert!(!is_sub);

        // Verify the subagent thread
        let (tid_sub, is_sub_sub): (i64, bool) = conn
            .query_row(
                "SELECT id, is_subagent FROM threads WHERE external_id = 'ses_sub'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let detail_sub = crate::search::thread_detail(&conn, tid_sub)
            .unwrap()
            .unwrap();
        // Title should fall back to truncated first user message
        assert_eq!(detail_sub.title.as_deref(), Some("subagent task"));
        assert!(is_sub_sub);

        // Second scan with no changes is skipped.
        let report2 = scan_sqlite(&mut conn, &src, &mut || {}).unwrap();
        assert_eq!(report2.threads_indexed, 0);
    }

    /// Assistant messages carry `modelID` and `tokens`; a tool-only turn (no text part)
    /// adds its usage to the last message instead of losing it.
    #[test]
    fn sqlite_messages_carry_usage() {
        let src = temp_dir("oc_usage_src.db");
        {
            let c = Connection::open(&src).unwrap();
            c.execute_batch(
                "CREATE TABLE session (id TEXT PRIMARY KEY, title TEXT, directory TEXT, parent_id TEXT, time_created INTEGER, time_updated INTEGER);
                 CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
                 CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, data TEXT);
                 INSERT INTO session VALUES ('s', 'Usage', '/p', NULL, 1, 2);
                 INSERT INTO message VALUES
                   ('m1', 's', 1000, json('{\"role\":\"user\"}')),
                   ('m2', 's', 2000, json('{\"role\":\"assistant\",\"modelID\":\"claude-sonnet-4-5\",\"tokens\":{\"input\":10,\"output\":5,\"reasoning\":1,\"cache\":{\"read\":100,\"write\":20}}}')),
                   ('m3', 's', 3000, json('{\"role\":\"assistant\",\"modelID\":\"claude-sonnet-4-5\",\"tokens\":{\"input\":7,\"output\":3,\"reasoning\":0,\"cache\":{\"read\":50,\"write\":0}}}'));
                 INSERT INTO part VALUES
                   ('p1', 'm1', json('{\"type\":\"text\",\"text\":\"go\"}')),
                   ('p2', 'm2', json('{\"type\":\"text\",\"text\":\"working\"}')),
                   ('p3', 'm3', json('{\"type\":\"tool\",\"tool\":\"bash\"}'));",
            )
            .unwrap();
        }
        let dst = temp_dir("oc_usage_dst.db");
        let mut conn = crate::db::open(&dst).unwrap();
        scan_sqlite(&mut conn, &src, &mut || {}).unwrap();
        type Row = (i64, Option<String>, [Option<i64>; 4]);
        let rows: Vec<Row> = conn
            .prepare(
                "SELECT seq, model, input_tokens, output_tokens, cache_write_tokens,
                        cache_read_tokens
                 FROM messages ORDER BY seq",
            )
            .unwrap()
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    [r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?],
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                (0, None, [None; 4]),
                (
                    1,
                    Some("claude-sonnet-4-5".into()),
                    [Some(17), Some(9), Some(20), Some(150)]
                ),
            ]
        );
    }

    /// Verify scan() dispatches: combined when both exist, JSON-only when no DB.
    #[test]
    fn scan_dispatches_correctly() {
        // --- Part 1: Both sources exist → scan_combined indexes both (no overlap) ---
        let root = temp_dir("oc_dispatch");
        let data_dir = root.join("opencode");
        create_dir_all(&data_dir).unwrap();

        let db_file = data_dir.join("opencode.db");
        {
            let c = Connection::open(&db_file).unwrap();
            c.execute_batch(
                "CREATE TABLE session (id TEXT PRIMARY KEY, title TEXT, directory TEXT, parent_id TEXT, time_created INTEGER, time_updated INTEGER);
                 CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
                 CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, data TEXT);
                 INSERT INTO session VALUES ('ses_1', 'DB Session', '/proj', NULL, 1780000000000, 1780000000000);
                 INSERT INTO message VALUES ('msg_1', 'ses_1', 1780000000000, json('{\"role\":\"user\"}'));
                 INSERT INTO part VALUES ('p1', 'msg_1', json('{\"type\":\"text\",\"text\":\"hello from db\"}'));",
            )
            .unwrap();
        }

        let storage = data_dir.join("storage");
        create_dir_all(storage.join("session/proj")).unwrap();
        create_dir_all(storage.join("message/ses_json")).unwrap();
        create_dir_all(storage.join("part/msg_j1")).unwrap();
        fs::write(
            storage.join("session/proj/ses_json.json"),
            r#"{"id":"ses_json","title":"JSON Session","time":{"created":1780000000000}}"#,
        )
        .unwrap();
        fs::write(
            storage.join("message/ses_json/msg_j1.json"),
            r#"{"id":"msg_j1","role":"user","time":{"created":1780000000000}}"#,
        )
        .unwrap();
        fs::write(
            storage.join("part/msg_j1/000.json"),
            r#"{"type":"text","text":"hello from json"}"#,
        )
        .unwrap();

        std::env::set_var(
            "OPENCODE_DATA_DIR",
            root.join("opencode").to_string_lossy().to_string(),
        );

        let dst = temp_dir("oc_dispatch_dst.db");
        let _ = std::fs::remove_file(&dst);
        let mut conn = crate::db::open(&dst).unwrap();
        let report = scan(&mut conn, &mut || {}).unwrap();

        // Both sources, no overlap → 2 threads
        assert_eq!(report.threads_indexed, 2);
        let hits = crate::search::search(&conn, "hello", &crate::search::SearchFilters::default())
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.source == "opencode"));

        // --- Part 2: No DB → scan falls back to JSON tree ---
        let root2 = temp_dir("oc_fallback");
        let data_dir2 = root2.join("opencode");
        let storage2 = data_dir2.join("storage");
        create_dir_all(storage2.join("session/proj")).unwrap();
        create_dir_all(storage2.join("message/ses_2")).unwrap();
        create_dir_all(storage2.join("part/msg_2")).unwrap();

        fs::write(
            storage2.join("session/proj/ses_2.json"),
            r#"{"id":"ses_2","title":"JSON Only","time":{"created":1780000000000}}"#,
        )
        .unwrap();
        fs::write(
            storage2.join("message/ses_2/msg_2.json"),
            r#"{"id":"msg_2","role":"user","time":{"created":1780000000000}}"#,
        )
        .unwrap();
        fs::write(
            storage2.join("part/msg_2/000.json"),
            r#"{"type":"text","text":"hello from json fallback"}"#,
        )
        .unwrap();

        std::env::set_var(
            "OPENCODE_DATA_DIR",
            root2.join("opencode").to_string_lossy().to_string(),
        );

        let dst2 = temp_dir("oc_fallback_dst.db");
        let _ = std::fs::remove_file(&dst2);
        let mut conn2 = crate::db::open(&dst2).unwrap();
        let report2 = scan(&mut conn2, &mut || {}).unwrap();

        assert_eq!(report2.threads_indexed, 1);
        let hits2 =
            crate::search::search(&conn2, "fallback", &crate::search::SearchFilters::default())
                .unwrap();
        assert_eq!(hits2.len(), 1);
        assert_eq!(hits2[0].source, "opencode");

        // --- Part 3: Both sources with overlap → SQLite wins on conflict ---
        let root3 = temp_dir("oc_combined");
        let data_dir3 = root3.join("opencode");
        create_dir_all(&data_dir3).unwrap();

        // SQLite: ses_overlap (conflicts with JSON) and ses_db_only
        let db_file3 = data_dir3.join("opencode.db");
        {
            let c = Connection::open(&db_file3).unwrap();
            c.execute_batch(
                "CREATE TABLE session (id TEXT PRIMARY KEY, title TEXT, directory TEXT, parent_id TEXT, time_created INTEGER, time_updated INTEGER);
                 CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
                 CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, data TEXT);
                 INSERT INTO session VALUES
                     ('ses_overlap', 'SQLite Title', '/db/proj', NULL, 1780000000000, 1780000000000),
                     ('ses_db_only', 'DB Only', '/db/proj', NULL, 1780000000000, 1780000000000);
                 INSERT INTO message VALUES
                     ('msg_o1', 'ses_overlap', 1780000000000, json('{\"role\":\"user\"}')),
                     ('msg_d1', 'ses_db_only', 1780000000000, json('{\"role\":\"user\"}'));
                 INSERT INTO part VALUES
                     ('po1', 'msg_o1', json('{\"type\":\"text\",\"text\":\"overlap from sqlite\"}')),
                     ('pd1', 'msg_d1', json('{\"type\":\"text\",\"text\":\"db only session\"}'));",
            )
            .unwrap();
        }

        // JSON: ses_overlap (same id, should be skipped) and ses_json_only
        let storage3 = data_dir3.join("storage");
        create_dir_all(storage3.join("session/proj")).unwrap();
        create_dir_all(storage3.join("message/ses_overlap")).unwrap();
        create_dir_all(storage3.join("part/msg_oj1")).unwrap();
        create_dir_all(storage3.join("message/ses_json_only")).unwrap();
        create_dir_all(storage3.join("part/msg_j3")).unwrap();
        fs::write(
            storage3.join("session/proj/ses_overlap.json"),
            r#"{"id":"ses_overlap","title":"JSON Title","directory":"/json/proj","time":{"created":1780000000000}}"#,
        )
        .unwrap();
        fs::write(
            storage3.join("message/ses_overlap/msg_oj1.json"),
            r#"{"id":"msg_oj1","role":"user","time":{"created":1780000000000}}"#,
        )
        .unwrap();
        fs::write(
            storage3.join("part/msg_oj1/000.json"),
            r#"{"type":"text","text":"overlap from json"}"#,
        )
        .unwrap();
        fs::write(
            storage3.join("session/proj/ses_json_only.json"),
            r#"{"id":"ses_json_only","title":"JSON Only","directory":"/json/proj","time":{"created":1780000000000}}"#,
        )
        .unwrap();
        fs::write(
            storage3.join("message/ses_json_only/msg_j3.json"),
            r#"{"id":"msg_j3","role":"user","time":{"created":1780000000000}}"#,
        )
        .unwrap();
        fs::write(
            storage3.join("part/msg_j3/000.json"),
            r#"{"type":"text","text":"json only session"}"#,
        )
        .unwrap();

        std::env::set_var(
            "OPENCODE_DATA_DIR",
            root3.join("opencode").to_string_lossy().to_string(),
        );

        let dst3 = temp_dir("oc_combined_dst.db");
        let _ = std::fs::remove_file(&dst3);
        let mut conn3 = crate::db::open(&dst3).unwrap();
        let report3 = scan(&mut conn3, &mut || {}).unwrap();

        // 3 sessions: ses_overlap + ses_db_only (from SQLite), ses_json_only (from JSON)
        assert_eq!(report3.threads_indexed, 3);

        // ses_overlap should have the SQLite title, not the JSON title
        let overlap_tid: i64 = conn3
            .query_row(
                "SELECT id FROM threads WHERE external_id = 'ses_overlap'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let detail_overlap = crate::search::thread_detail(&conn3, overlap_tid)
            .unwrap()
            .unwrap();
        assert_eq!(detail_overlap.title.as_deref(), Some("SQLite Title"));

        // ses_json_only should be indexed from JSON
        let json_tid: i64 = conn3
            .query_row(
                "SELECT id FROM threads WHERE external_id = 'ses_json_only'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let detail_json = crate::search::thread_detail(&conn3, json_tid)
            .unwrap()
            .unwrap();
        assert_eq!(detail_json.title.as_deref(), Some("JSON Only"));

        std::env::remove_var("OPENCODE_DATA_DIR");
    }

    #[test]
    #[ignore]
    fn real_opencode_index() {
        let mut conn = crate::db::open(&temp_dir("oc_real").join("db.sqlite")).unwrap();
        eprintln!("{:?}", scan(&mut conn, &mut || {}).unwrap());
    }
}
