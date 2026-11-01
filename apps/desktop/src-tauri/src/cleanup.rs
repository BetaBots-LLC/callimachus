//! Storage cleanup: list threads oldest-first with their on-disk footprint, delete
//! selected ones, and reclaim freed pages. Deleting a thread cascades to its
//! messages (FK `ON DELETE CASCADE`), which fires the triggers that clear the FTS
//! index and vector chunks — so one `DELETE FROM threads` removes everything.
//!
//! Deleted threads leave a tombstone (`deleted_threads`, migration 0027) so the next
//! scan doesn't re-import them. This module also hosts the one-off maintenance passes:
//! purging the app's own CLI-engine sessions that older builds indexed, compacting
//! oversized tool output, and keeping the vector owner map (migration 0025) in sync.

use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension, ToSql};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// A thread shown in the cleanup list, with size for "what's worth deleting".
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupRow {
    pub id: i64,
    pub source: String,
    pub title: Option<String>,
    pub project_path: Option<String>,
    pub message_count: i64,
    pub bytes: i64, // UTF-8 byte size of the thread's message text
    pub updated_at: Option<i64>,
}

/// Threads ordered oldest-first (least-recently active), with their text byte size.
/// `before` (epoch secs) limits to threads not touched since then; `sources` filters
/// by kind. NULL `updated_at` (unknown age) sorts first as a cleanup candidate.
pub fn candidates(
    conn: &Connection,
    before: Option<i64>,
    sources: &[String],
    limit: i64,
) -> Result<Vec<CleanupRow>> {
    // Reads the precomputed t.bytes column — no per-message SUM/JOIN, so this stays
    // fast and never holds the DB lock long enough to freeze the Settings page.
    let mut sql = String::from(
        "SELECT t.id, s.kind, t.title, t.project_path, t.message_count, t.bytes, t.updated_at
         FROM threads t
         JOIN sources s ON s.id = t.source_id
         WHERE 1=1",
    );
    let mut args: Vec<Box<dyn ToSql>> = Vec::new();
    if let Some(b) = before {
        args.push(Box::new(b));
        sql.push_str(&format!(
            " AND (t.updated_at IS NULL OR t.updated_at <= ?{})",
            args.len()
        ));
    }
    if !sources.is_empty() {
        let ph: Vec<String> = (0..sources.len())
            .map(|i| format!("?{}", args.len() + 1 + i))
            .collect();
        sql.push_str(&format!(" AND s.kind IN ({})", ph.join(", ")));
        for s in sources {
            args.push(Box::new(s.clone()));
        }
    }
    sql.push_str(&format!(
        " ORDER BY t.updated_at ASC LIMIT ?{}",
        args.len() + 1
    ));
    args.push(Box::new(limit));

    let arg_refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(arg_refs.as_slice(), |r| {
        Ok(CleanupRow {
            id: r.get(0)?,
            source: r.get(1)?,
            title: r.get(2)?,
            project_path: r.get(3)?,
            message_count: r.get(4)?,
            bytes: r.get(5)?,
            updated_at: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Delete the given threads (and, by cascade + triggers, their messages, FTS rows,
/// and vector chunks). Returns how many threads were removed.
///
/// Each deleted thread is tombstoned first, so an indexer that sees the same transcript
/// again (a Cursor rescan, a Claude file being appended) skips it instead of bringing it
/// back. In-app chats are not tombstoned: nothing re-imports them, and a chat the user is
/// still looking at must stay saveable.
pub fn delete_threads(conn: &mut Connection, ids: &[i64]) -> Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let now = chrono::Utc::now().timestamp();
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut n = 0;
    {
        let mut tomb = tx.prepare(
            "INSERT OR REPLACE INTO deleted_threads (source_id, external_id, deleted_at)
             SELECT t.source_id, t.external_id, ?2
             FROM threads t JOIN sources s ON s.id = t.source_id
             WHERE t.id = ?1 AND s.kind <> 'in_app'",
        )?;
        let mut del = tx.prepare("DELETE FROM threads WHERE id = ?1")?;
        for id in ids {
            tomb.execute(params![id, now])?;
            n += del.execute([id])?;
        }
    }
    tx.commit()?;
    Ok(n)
}

/// Whether the user deleted this (source, external id) thread. Indexers call this before
/// parsing so a deleted transcript is neither re-imported nor re-parsed on every change.
pub fn is_tombstoned(conn: &Connection, source_id: i64, external_id: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM deleted_threads WHERE source_id = ?1 AND external_id = ?2",
            params![source_id, external_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Forget every tombstone so deleted threads are re-imported by the next full scan (e.g. a
/// future "restore deleted threads" setting). Returns how many were cleared.
pub fn clear_tombstones(conn: &Connection) -> Result<usize> {
    Ok(conn.execute("DELETE FROM deleted_threads", [])?)
}

/// Reclaim disk space freed by deletes (rewrites the DB file; also checkpoints WAL).
///
/// Runs on a dedicated connection. VACUUM builds a compacted copy of the database in a
/// transient temp database and then writes it back through the WAL, so it needs roughly
/// the live size of the index free in the temp dir AND again on the index's own volume.
/// `db::open` sets `temp_store = MEMORY` for fast sorts, which would put that whole copy
/// in RAM (gigabytes on a large index), so it is switched to FILE for the duration.
pub fn vacuum(conn: &Connection) -> Result<()> {
    // Cheap housekeeping first, so the compacted file doesn't keep dead vectors.
    reconcile_vec_owners(conn)?;
    sweep_orphan_vectors(conn)?;

    let live = live_bytes(conn)?;
    if let Some(db_path) = conn.path().filter(|p| !p.is_empty()).map(PathBuf::from) {
        let db_dir = db_path.parent().map(Path::to_path_buf).unwrap_or_default();
        check_free_space(&db_dir, &sqlite_temp_dir(), live)?;
    }

    conn.pragma_update(None, "temp_store", "FILE")?;
    let res = conn.execute_batch("VACUUM;");
    // Always restore the fast default, even if VACUUM failed (a failed VACUUM loses
    // nothing: the freed pages simply stay on the freelist for reuse).
    let _ = conn.pragma_update(None, "temp_store", "MEMORY");
    res?;
    // VACUUM in WAL mode leaves a WAL about the size of the database; fold it back now
    // instead of waiting for an autocheckpoint, so the disk space actually comes back.
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    Ok(())
}

/// Bytes VACUUM will copy: the database minus its free pages.
fn live_bytes(conn: &Connection) -> Result<u64> {
    let pragma = |name: &str| -> Result<i64> {
        Ok(conn.query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))?)
    };
    let pages = pragma("page_count")? - pragma("freelist_count")?;
    Ok((pages.max(0) as u64) * (pragma("page_size")?.max(0) as u64))
}

/// Where SQLite puts temp files on this platform (mirrors its own lookup order on unix).
fn sqlite_temp_dir() -> PathBuf {
    for var in ["SQLITE_TMPDIR", "TMPDIR"] {
        if let Some(v) = std::env::var_os(var).filter(|v| !v.is_empty()) {
            return PathBuf::from(v);
        }
    }
    if cfg!(unix) && Path::new("/var/tmp").is_dir() {
        return PathBuf::from("/var/tmp");
    }
    std::env::temp_dir()
}

/// Refuse to start a VACUUM that would run out of disk halfway: it needs `live` bytes for
/// the temp copy and `live` bytes of WAL next to the database (2x when both are on the
/// same volume). Skipped when free space can't be determined.
fn check_free_space(db_dir: &Path, temp_dir: &Path, live: u64) -> Result<()> {
    // 5% headroom for page overhead and whatever else writes meanwhile.
    let need = live + live / 20;
    let (Some(db_vol), Some(tmp_vol)) = (volume_space(db_dir), volume_space(temp_dir)) else {
        return Ok(());
    };
    let gb = |b: u64| b as f64 / 1_073_741_824.0;
    if db_vol.mount == tmp_vol.mount {
        if db_vol.available < need * 2 {
            bail!(
                "Not enough free disk space to reclaim space: compacting needs about {:.1} GB \
                 free on {} but only {:.1} GB is available.",
                gb(need * 2),
                db_vol.mount,
                gb(db_vol.available)
            );
        }
    } else {
        for (vol, what) in [(&db_vol, "the index's disk"), (&tmp_vol, "the temp disk")] {
            if vol.available < need {
                bail!(
                    "Not enough free disk space to reclaim space: compacting needs about \
                     {:.1} GB free on {what} ({}) but only {:.1} GB is available.",
                    gb(need),
                    vol.mount,
                    gb(vol.available)
                );
            }
        }
    }
    Ok(())
}

struct VolumeSpace {
    mount: String,
    available: u64,
}

/// Free space on the volume holding `dir`, via POSIX `df -Pk` (no extra dependency).
#[cfg(unix)]
fn volume_space(dir: &Path) -> Option<VolumeSpace> {
    let out = std::process::Command::new("df")
        .arg("-Pk")
        .arg(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_df(&String::from_utf8_lossy(&out.stdout))
}

/// Windows has no `df`; the size check is skipped there (SQLite still fails cleanly with
/// SQLITE_FULL if the disk fills, and a failed VACUUM loses nothing).
#[cfg(not(unix))]
fn volume_space(_dir: &Path) -> Option<VolumeSpace> {
    None
}

/// Parse `df -P` output: a header line, then `fs blocks used avail capacity mount...`.
#[cfg_attr(not(unix), allow(dead_code))]
fn parse_df(text: &str) -> Option<VolumeSpace> {
    let line = text.lines().nth(1)?;
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 6 {
        return None;
    }
    let available = fields[3].parse::<u64>().ok()? * 1024;
    // The mount point may contain spaces; it is everything from the sixth field on.
    let mount = fields[5..].join(" ");
    Some(VolumeSpace { mount, available })
}

// ---------------------------------------------------------------------------------------
// Vector owner map (migration 0025)
// ---------------------------------------------------------------------------------------

/// Add owner rows for any vectors written without one (e.g. by an embedder build that
/// predates migration 0025), so their message's delete still removes them. One full
/// scan of the vector table (sub-second at a few hundred thousand chunks); safe to run
/// after every embedding job. Returns how many rows were added.
pub fn reconcile_vec_owners(conn: &Connection) -> Result<usize> {
    Ok(conn.execute(
        "INSERT OR IGNORE INTO vec_chunk_owner (chunk_rowid, message_id)
         SELECT rowid, message_id FROM vec_chunks",
        [],
    )?)
}

/// Delete vectors whose message no longer exists (a message deleted while its batch was
/// being embedded). Each removed owner row removes its vector by rowid (trigger).
pub fn sweep_orphan_vectors(conn: &Connection) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM vec_chunk_owner
         WHERE NOT EXISTS (SELECT 1 FROM messages m WHERE m.id = vec_chunk_owner.message_id)",
        [],
    )?)
}

// ---------------------------------------------------------------------------------------
// One-off maintenance passes
// ---------------------------------------------------------------------------------------

/// `app_config` key recording that the app-session purge already ran on this index.
const PURGED_APP_SESSIONS_KEY: &str = "cleanup.app_cli_sessions_purged";

/// Delete threads that are really the app's own CLI-engine calls (distill / Ask / chat via
/// `claude -p` or `codex exec`), which older builds persisted and then indexed back as if
/// they were the user's sessions. Matching is strict (see `agent::is_app_cli_session`):
/// the session ran in the app's scratch directory, or in the temp dir AND opens with one
/// of the app's own prompts. Not tombstoned: the indexers skip these sessions anyway.
/// Returns how many threads were removed.
pub fn purge_app_cli_sessions(conn: &mut Connection) -> Result<usize> {
    let mut prefixes: Vec<String> = Vec::new();
    for dir in [std::env::temp_dir(), crate::agent::cli_work_dir_path()] {
        for form in [Some(dir.clone()), std::fs::canonicalize(&dir).ok()]
            .into_iter()
            .flatten()
        {
            let s = form
                .to_string_lossy()
                .trim_end_matches(['/', '\\'])
                .to_string();
            if !s.is_empty() && !prefixes.contains(&s) {
                prefixes.push(s);
            }
        }
    }

    // Candidate threads: Claude Code / Codex sessions whose cwd is under a temp form.
    let mut candidates: Vec<(i64, String)> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT t.id, t.project_path FROM threads t JOIN sources s ON s.id = t.source_id
             WHERE s.kind IN ('claude_code', 'codex') AND t.project_path LIKE ?1 ESCAPE '\\'",
        )?;
        for prefix in &prefixes {
            let pattern = format!("{}%", like_escape(prefix));
            let rows = stmt.query_map([pattern], |r| Ok((r.get(0)?, r.get(1)?)))?;
            for row in rows {
                let row: (i64, String) = row?;
                if !candidates.iter().any(|(id, _)| *id == row.0) {
                    candidates.push(row);
                }
            }
        }
    }

    let mut doomed = Vec::new();
    {
        let mut first_user = conn.prepare(
            "SELECT text FROM messages WHERE thread_id = ?1 AND role = 'user'
             ORDER BY seq LIMIT 3",
        )?;
        for (id, cwd) in candidates {
            let texts: Vec<String> = first_user
                .query_map([id], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            if crate::agent::is_app_cli_session(Some(&cwd), texts.iter().map(String::as_str)) {
                doomed.push(id);
            }
        }
    }

    if doomed.is_empty() {
        return Ok(0);
    }
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut n = 0;
    {
        let mut del = tx.prepare("DELETE FROM threads WHERE id = ?1")?;
        for id in &doomed {
            n += del.execute([id])?;
        }
    }
    tx.commit()?;
    Ok(n)
}

/// Run [`purge_app_cli_sessions`] once per index (remembered in `app_config`). Cheap to
/// call at every startup; returns how many threads it removed this time.
pub fn purge_app_cli_sessions_once(conn: &mut Connection) -> Result<usize> {
    let done: Option<String> = conn
        .query_row(
            "SELECT value FROM app_config WHERE key = ?1",
            [PURGED_APP_SESSIONS_KEY],
            |r| r.get(0),
        )
        .optional()?;
    if done.is_some() {
        return Ok(0);
    }
    let n = purge_app_cli_sessions(conn)?;
    conn.execute(
        "INSERT OR REPLACE INTO app_config (key, value) VALUES (?1, ?2)",
        params![
            PURGED_APP_SESSIONS_KEY,
            chrono::Utc::now().timestamp().to_string()
        ],
    )?;
    Ok(n)
}

/// Escape `%`, `_` and the escape char itself for a `LIKE ... ESCAPE '\'` pattern.
fn like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Sources whose indexers cap tool output at index time (`indexer::claude::cap_tool_text`).
/// Compacting another source's rows would only make its next re-index see a mismatch and
/// rewrite the thread with the uncapped text again.
pub const CAPPED_TOOL_SOURCES: &[&str] = &[
    "claude_code",
    "codex",
    "gemini",
    "qwen",
    "cline",
    "roo",
    "kilo",
    "continue",
];

/// Progress of one [`compact_tool_output`] call.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactReport {
    /// Tool rows rewritten in this call.
    pub rows: usize,
    /// UTF-8 bytes of message text removed in this call.
    pub bytes_saved: i64,
    /// True when no oversized rows remain (stop calling).
    pub done: bool,
}

/// The next `batch` oversized tool rows after id `?1`, for [`compact_tool_output`].
fn compact_select_sql(batch: usize) -> String {
    use crate::indexer::claude::TOOL_TEXT_CAP_CHARS;
    let kinds = CAPPED_TOOL_SOURCES
        .iter()
        .map(|k| format!("'{k}'"))
        .collect::<Vec<_>>()
        .join(", ");
    // octet_length() reads the record header only, so rows already under the cap are
    // skipped without loading their (often multi-page) text. A byte length at or below
    // the char cap guarantees the char count is too.
    //
    // The `+` prefixes keep this a rowid range scan in id order that stops after `batch`
    // matches, so the whole run is one pass over the table. Left to itself the planner
    // drives from the role or thread index instead and then sorts: every batch re-read all
    // ~600k tool rows (5-10s per batch of 200 on a large index, so minutes per call).
    format!(
        "SELECT m.id, m.thread_id, m.text FROM messages m
         WHERE m.id > ?1 AND +m.role = 'tool' AND octet_length(m.text) > {TOOL_TEXT_CAP_CHARS}
           AND +m.thread_id IN (
               SELECT t.id FROM threads t JOIN sources s ON s.id = t.source_id
               WHERE s.kind IN ({kinds}))
         ORDER BY m.id LIMIT {batch}"
    )
}

/// Shrink tool-output rows indexed before the index-time cap existed down to the same
/// head + tail form the indexer now stores, so old transcripts stop dominating disk and
/// FTS size. Processes at most `max_rows` rows per call (in short transactions) so the
/// caller can loop from a background thread and release the write lock in between; FTS
/// stays in sync through the update trigger. Run `vacuum` afterwards to return the space
/// to the OS. Only touches [`CAPPED_TOOL_SOURCES`].
pub fn compact_tool_output(conn: &mut Connection, max_rows: usize) -> Result<CompactReport> {
    use crate::indexer::claude::cap_tool_text;
    const BATCH: usize = 200;
    let mut report = CompactReport::default();
    let select = compact_select_sql(BATCH);
    let mut after = 0i64;
    while report.rows < max_rows {
        let rows: Vec<(i64, i64, String)> = {
            let mut stmt = conn.prepare(&select)?;
            let r = stmt
                .query_map([after], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            r
        };
        let Some(last) = rows.last() else {
            report.done = true;
            break;
        };
        after = last.0;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        {
            let mut upd = tx.prepare("UPDATE messages SET text = ?2 WHERE id = ?1")?;
            // content_hash describes the old text, so drop it: anything that trusts it to
            // skip a re-index must re-verify this thread instead.
            let mut bytes = tx.prepare(
                "UPDATE threads SET bytes = MAX(0, bytes - ?2), content_hash = NULL WHERE id = ?1",
            )?;
            for (id, thread_id, text) in &rows {
                let capped = cap_tool_text(text);
                if capped.len() >= text.len() {
                    continue; // multi-byte text under the char cap: nothing to trim
                }
                let saved = (text.len() - capped.len()) as i64;
                upd.execute(params![id, capped])?;
                bytes.execute(params![thread_id, saved])?;
                report.rows += 1;
                report.bytes_saved += saved;
            }
        }
        tx.commit()?;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::{source_id, upsert_thread, ParsedMessage, ParsedThread};

    fn temp_db() -> Connection {
        // Unique per test: tests in this module run in parallel.
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "calli_cleanup_{}_{}.db",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        crate::db::open(&p).unwrap()
    }

    fn msg(role: &str, text: &str) -> ParsedMessage {
        ParsedMessage {
            role: role.into(),
            text: text.into(),
            tool_name: None,
            ts: Some(1),
        }
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn thread(ext: &str, updated: i64, msgs: usize) -> ParsedThread {
        ParsedThread {
            external_id: ext.into(),
            title: Some(ext.into()),
            updated_at: Some(updated),
            created_at: Some(updated),
            messages: (0..msgs)
                .map(|i| ParsedMessage {
                    role: if i % 2 == 0 {
                        "user".into()
                    } else {
                        "assistant".into()
                    },
                    text: format!("message {i} body text"),
                    tool_name: None,
                    ts: Some(updated),
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn lists_oldest_first_and_deletes() {
        let mut conn = temp_db();
        let sid = source_id(&conn, "claude_code").unwrap();
        upsert_thread(&mut conn, sid, &thread("new", 2000, 2)).unwrap();
        upsert_thread(&mut conn, sid, &thread("old", 1000, 4)).unwrap();

        let rows = candidates(&conn, None, &[], 50).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].title.as_deref(), Some("old")); // oldest first
        assert!(rows[0].bytes > 0 && rows[0].message_count == 4);

        // `before` filter keeps only the old thread.
        let old_only = candidates(&conn, Some(1500), &[], 50).unwrap();
        assert_eq!(old_only.len(), 1);
        assert_eq!(old_only[0].title.as_deref(), Some("old"));

        // Delete it; messages + FTS + (would-be) chunks go with it via cascade/triggers.
        let removed = delete_threads(&mut conn, &[old_only[0].id]).unwrap();
        assert_eq!(removed, 1);
        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 1);
        let msgs: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(msgs, 2, "deleted thread's messages cascaded away");
    }

    #[test]
    fn delete_tombstones_external_threads_but_not_in_app_chats() {
        let mut conn = temp_db();
        let claude = source_id(&conn, "claude_code").unwrap();
        let in_app = source_id(&conn, "in_app").unwrap();
        upsert_thread(&mut conn, claude, &thread("ext-1", 1, 2)).unwrap();
        upsert_thread(&mut conn, in_app, &thread("chat-1", 1, 2)).unwrap();
        let ids: Vec<i64> = conn
            .prepare("SELECT id FROM threads")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(delete_threads(&mut conn, &ids).unwrap(), 2);
        assert!(is_tombstoned(&conn, claude, "ext-1").unwrap());
        assert!(!is_tombstoned(&conn, in_app, "chat-1").unwrap());
        assert_eq!(clear_tombstones(&conn).unwrap(), 1);
        assert!(!is_tombstoned(&conn, claude, "ext-1").unwrap());
    }

    #[test]
    fn purge_removes_only_the_apps_own_cli_sessions() {
        let mut conn = temp_db();
        let sid = source_id(&conn, "claude_code").unwrap();
        let tmp = std::env::temp_dir().to_string_lossy().to_string();
        let work = crate::agent::cli_work_dir_path()
            .to_string_lossy()
            .to_string();
        // The leading words of the app's distill system prompt (agent::DISTILL_SYSTEM).
        let distill = "You distill one AI coding-agent session into structured knowledge for \
                       the developer. Output ONLY a single JSON object";
        let mk = |ext: &str, cwd: &str, first: &str| ParsedThread {
            external_id: ext.into(),
            project_path: Some(cwd.into()),
            messages: vec![msg("user", first), msg("assistant", "ok")],
            ..Default::default()
        };
        for t in [
            mk("scratch", &work, "anything"),
            mk("legacy", &tmp, distill),
            mk("real-tmp-session", &tmp, "fix my script"),
            mk("real-project", "/Users/me/proj", distill),
        ] {
            upsert_thread(&mut conn, sid, &t).unwrap();
        }

        assert_eq!(purge_app_cli_sessions_once(&mut conn).unwrap(), 2);
        let left: Vec<String> = conn
            .prepare("SELECT external_id FROM threads ORDER BY external_id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(left, vec!["real-project", "real-tmp-session"]);
        // Runs once per index.
        upsert_thread(&mut conn, sid, &mk("scratch-2", &work, "x")).unwrap();
        assert_eq!(purge_app_cli_sessions_once(&mut conn).unwrap(), 0);
    }

    #[test]
    fn compact_tool_output_matches_the_index_time_cap() {
        use crate::indexer::claude::{cap_tool_text, TOOL_TEXT_CAP_CHARS};
        let mut conn = temp_db();
        let sid = source_id(&conn, "claude_code").unwrap();
        let big = format!("STARTMARK {} uniquetailword", "middle ".repeat(10_000));
        let t = ParsedThread {
            external_id: "big".into(),
            messages: vec![
                msg("user", "run it"),
                msg("tool", &big),
                msg("tool", "small output"),
            ],
            ..Default::default()
        };
        upsert_thread(&mut conn, sid, &t).unwrap();
        let bytes_before = count(&conn, "SELECT bytes FROM threads");

        let r = compact_tool_output(&mut conn, 1000).unwrap();
        assert_eq!(r.rows, 1, "only the oversized row is rewritten");
        assert!(r.done);
        assert!(r.bytes_saved > 0);
        let stored: String = conn
            .query_row("SELECT text FROM messages WHERE seq = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            stored,
            cap_tool_text(&big),
            "same text a fresh index would store"
        );
        assert!(stored.chars().count() <= TOOL_TEXT_CAP_CHARS);
        assert_eq!(
            count(&conn, "SELECT bytes FROM threads"),
            bytes_before - r.bytes_saved
        );
        // FTS follows the rewrite: the kept tail is still searchable.
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'uniquetailword'"
            ),
            1
        );
        // A second pass has nothing left to do.
        assert_eq!(compact_tool_output(&mut conn, 1000).unwrap().rows, 0);
    }

    /// Each batch must resume a rowid range scan, not re-read every tool row and sort them.
    #[test]
    fn compact_batches_scan_in_id_order() {
        let conn = temp_db();
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {}", compact_select_sql(200)))
            .unwrap()
            .query_map([0_i64], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let plan = plan.join(" | ");
        assert!(
            plan.contains("SEARCH m USING INTEGER PRIMARY KEY (rowid>?)"),
            "{plan}"
        );
        assert!(!plan.contains("TEMP B-TREE"), "{plan}");
    }

    #[test]
    fn vector_owner_repair_and_orphan_sweep() {
        let mut conn = temp_db();
        let sid = source_id(&conn, "claude_code").unwrap();
        upsert_thread(&mut conn, sid, &thread("v", 1, 2)).unwrap();
        let mid: i64 = conn
            .query_row("SELECT MIN(id) FROM messages", [], |r| r.get(0))
            .unwrap();
        let blob: Vec<u8> = (0..384).flat_map(|_| 0.5_f32.to_le_bytes()).collect();
        // A vector written without its owner row (an embedder that predates 0025).
        conn.execute(
            "INSERT INTO vec_chunks (message_id, embedding) VALUES (?1, ?2)",
            params![mid, blob],
        )
        .unwrap();
        assert_eq!(reconcile_vec_owners(&conn).unwrap(), 1);
        assert_eq!(reconcile_vec_owners(&conn).unwrap(), 0, "idempotent");

        // The message vanishes without the delete trigger firing (embedded = 0 here), so
        // its vector is orphaned; the sweep finds it through the owner map.
        conn.execute("DELETE FROM messages WHERE id = ?1", [mid])
            .unwrap();
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM vec_chunks"), 1);
        assert_eq!(sweep_orphan_vectors(&conn).unwrap(), 1);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM vec_chunks"), 0);
    }

    #[test]
    fn vacuum_runs_and_restores_in_memory_temp_store() {
        let conn = temp_db();
        vacuum(&conn).unwrap();
        let temp_store: i64 = conn
            .query_row("PRAGMA temp_store", [], |r| r.get(0))
            .unwrap();
        assert_eq!(temp_store, 2, "MEMORY again after the FILE-backed VACUUM");
    }

    #[test]
    fn parses_posix_df_output() {
        let out = "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                   /dev/disk3s5 971350180 512 4096 1% /System/Volumes/My Data\n";
        let v = parse_df(out).unwrap();
        assert_eq!(v.available, 4096 * 1024);
        assert_eq!(v.mount, "/System/Volumes/My Data");
        assert!(parse_df("garbage").is_none());
    }

    #[test]
    fn free_space_check_needs_room_on_both_volumes() {
        // Unknown free space never blocks a VACUUM.
        assert!(
            check_free_space(Path::new("/nonexistent-a"), Path::new("/nonexistent-b"), 1).is_ok()
        );
    }
}
