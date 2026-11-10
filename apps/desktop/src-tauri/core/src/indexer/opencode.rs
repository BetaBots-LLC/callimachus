//! OpenCode indexer.
//!
//! OpenCode (sst/opencode) stores sessions in one of two formats depending on
//! version and migration state:
//!
//! | Format      | Location                                               | When read     |
//! |-------------|--------------------------------------------------------|---------------|
//! | JSON files  | `storage/{session,message,part}/*.json`                | DB absent     |
//! | V1 SQLite   | `message` + `part` tables in `opencode.db`             | DB exists     |
//! | V2 SQLite   | `session_message` table in `opencode.db`               | Deferred      |
//!
//! When both sources exist (user ran OpenCode before and after the SQLite
//! migration), `scan_combined` reads both and deduplicates by `external_id`:
//! SQLite entries take precedence. V2 is deferred until the runtime writes there.
//!
//! Schema verified against opencode `packages/sdk/js/src/gen/types.gen.ts`. An assistant
//! message carries `modelID` and `tokens: {input, output, reasoning, cache: {read, write}}`,
//! where `input` already excludes the cached tokens; read for spend. A message with no text
//! part (a turn of tool calls only) still spent tokens: its usage lands on the last message.

use super::common::{
    begin_file, count_file, count_line, count_malformed, count_skipped, read_json, truncate_title,
};
use super::{
    file_change_state, file_state, open_external_readonly, set_file_state, source_id,
    upsert_thread, IndexReport, MsgUsage, ParsedMessage, ParsedThread,
};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

pub const KIND: &str = "opencode";

/// One row of the V1 `session` table:
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

/// Path to OpenCode's SQLite session DB (V1).
///
/// Returns `<data_root>/opencode.db` — the sibling of `storage/`.
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

/// Index OpenCode sessions from the V1 SQLite DB (`session`, `message`, `part`
/// tables). Skips if the file is unchanged since the last successful pass.
fn scan_sqlite(conn: &mut Connection, db: &Path, tick: &mut dyn FnMut()) -> Result<IndexReport> {
    let mut report = IndexReport::default();
    let sid = source_id(conn, KIND)?;
    let (unchanged, mtime, size) = file_change_state(conn, db)?;
    if unchanged {
        return Ok(report);
    }

    let ro = open_external_readonly(db)?;

    let mut session_stmt = ro.prepare(
        "SELECT id, title, directory, parent_id, time_created, time_updated FROM session",
    )?;
    let sessions: Vec<SessionRow> = session_stmt
        .query_map([], session_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    for row in &sessions {
        tick();
        let Some(thread) = sqlite_thread(&ro, row)? else {
            report.threads_skipped += 1;
            continue;
        };
        let n = upsert_thread(conn, sid, &thread)?;
        report.threads_indexed += 1;
        report.messages_indexed += n;
    }

    // Record the DB file as indexed ONLY after every session upserted without
    // error: a mid-pass failure returns above via `?`, leaving the file
    // "changed" so the watcher's retry re-indexes it instead of skipping
    // dropped sessions.
    set_file_state(conn, &db.to_string_lossy(), KIND, mtime, size)?;
    Ok(report)
}

fn session_row(r: &rusqlite::Row) -> rusqlite::Result<SessionRow> {
    Ok((
        r.get::<_, String>(0)?,         // id
        r.get::<_, Option<String>>(1)?, // title
        r.get::<_, Option<String>>(2)?, // directory
        r.get::<_, Option<String>>(3)?, // parent_id
        r.get::<_, Option<i64>>(4)?,    // time_created
        r.get::<_, Option<i64>>(5)?,    // time_updated
    ))
}

/// One session of the V1 SQLite DB, read for the usage backfill
/// (`indexer::usage_backfill`): the same thread [`scan_sqlite`] stores. None when the
/// session is gone or has no text.
pub(super) fn sqlite_session_thread(
    ro: &Connection,
    session_id: &str,
) -> Result<Option<ParsedThread>> {
    let row = ro
        .prepare_cached(
            "SELECT id, title, directory, parent_id, time_created, time_updated
             FROM session WHERE id = ?1",
        )?
        .query_row([session_id], session_row)
        .optional()?;
    match row {
        Some(row) => sqlite_thread(ro, &row),
        None => Ok(None),
    }
}

/// Build one session's thread from the V1 SQLite DB. None when it has no text messages.
fn sqlite_thread(ro: &Connection, row: &SessionRow) -> Result<Option<ParsedThread>> {
    let (session_id, title, directory, parent_id, time_created, time_updated) = row;
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
    if messages.is_empty() {
        return Ok(None);
    }

    let created_at = time_created.map(|ms| ms / 1000);
    let updated_at = time_updated.map(|ms| ms / 1000);
    let title = title
        .as_deref()
        .filter(|t| !t.trim().is_empty())
        .map(str::to_string)
        .or_else(|| first_user.as_deref().map(truncate_title));
    let is_subagent = parent_id.is_some();

    let thread = ParsedThread {
        external_id: session_id.clone(),
        title,
        project_path: directory
            .as_deref()
            .filter(|d| !d.is_empty())
            .map(str::to_string),
        git_branch: None,
        created_at,
        updated_at,
        is_subagent,
        usage,
        messages,
    };
    Ok(Some(thread))
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
    let t = msg.get("tokens")?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::create_dir_all;

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
