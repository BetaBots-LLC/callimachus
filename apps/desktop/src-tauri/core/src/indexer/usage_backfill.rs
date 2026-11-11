//! One-time token usage backfill for the sessions Codex, Gemini CLI, Qwen Code and OpenCode
//! wrote before their indexers recorded usage. A Reindex never re-parses an unchanged
//! transcript (each source's mtime and size cursor), so without this those sessions showed no
//! spend until their file next changed, which for a finished session is never.
//!
//! For each such thread with no usage row, [`backfill_usage`] re-parses its transcript (found
//! through `index_state`, the files the scans recorded) and writes only the usage columns onto
//! the rows already stored, by `seq`. Nothing is replaced: no message id churns, nothing is
//! re-queued for embedding, no cursor moves, and fact provenance stays put.
//!
//! The stored rows can differ from a fresh parse: an index from before 0034 skipped Codex's
//! freeform tool calls (`apply_patch`) and kept every re-recorded Gemini copy. So the parse
//! is aligned to the stored rows by text first (see [`align`]), and a call whose message is
//! not stored lands on the nearest stored row before it, so no spend is lost.

use super::{codex, gemini, opencode, qwen, MsgUsage, ParsedThread};
use rusqlite::{Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Set once a backfill pass completes. Absent means pending, so it runs once after the
/// update (phase 2 ships no migration to set a pending flag); a fresh index is marked done
/// at once by [`mark_usage_backfilled`], since its sessions are parsed with usage already.
pub const BACKFILL_DONE: &str = "usage.backfill_done";

/// The sources whose indexers started recording usage in this release.
const SOURCES: [&str; 4] = [codex::KIND, gemini::KIND, qwen::KIND, opencode::KIND];

/// Most threads per write batch.
const BATCH: usize = 50;
/// A batch stops gathering once its parses have run this long, so the GUI connection
/// (shared with star, tag and Remember) is never held much longer.
const BATCH_TIME: Duration = Duration::from_millis(200);
/// How often a paused backfill checks whether the writer job it yields to is done.
const PAUSE_POLL: Duration = Duration::from_secs(2);
/// How far ahead in the stored rows a parsed message is looked for. A Gemini record
/// re-recorded a few times leaves that many stored copies to skip; a wider window only
/// raises the odds of matching a short repeated text (`ok`) out of place.
const ALIGN_WINDOW: usize = 64;

/// What a backfill pass did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UsageBackfillReport {
    /// Threads of the four sources that had no usage when the pass started.
    pub threads: usize,
    /// Threads the pass wrote usage onto.
    pub filled: usize,
    /// Message rows given usage.
    pub rows: usize,
    /// Threads whose transcript is gone or no longer lines up with the stored rows.
    pub missed: usize,
}

/// Mark the backfill done without running it: for a fresh index, whose sessions are all
/// parsed with usage by the indexing that fills it.
pub fn mark_usage_backfilled(conn: &Connection) -> anyhow::Result<()> {
    crate::knowledge::app_config::put_flag(conn, BACKFILL_DONE, true)
}

/// Run [`backfill_usage`] unless [`BACKFILL_DONE`] is set, then set it (an interrupted pass
/// runs again next time and skips the threads it already filled). Returns None when it had
/// already run. For the watcher (after the `file_edits` backfill) and `cal index`; `pause`
/// says a writer job is running, and the backfill waits between batches while it does.
pub fn backfill_usage_once(
    db: &crate::db::Db,
    pause: &dyn Fn() -> bool,
) -> anyhow::Result<Option<UsageBackfillReport>> {
    let lock = || crate::db::lock_conn(&db.0);
    if crate::knowledge::app_config::get_flag(&lock(), BACKFILL_DONE)? {
        return Ok(None);
    }
    let report = backfill_usage(db, pause, &Roots::from_home())?;
    mark_usage_backfilled(&lock())?;
    Ok(Some(report))
}

/// Where the Gemini-layout sources keep their chats: their external ids are the path below
/// these roots. Injected so tests need no HOME.
pub struct Roots {
    pub gemini: Option<PathBuf>,
    pub qwen: Option<PathBuf>,
}

impl Roots {
    pub fn from_home() -> Self {
        Self {
            gemini: gemini::tmp_root(),
            qwen: qwen::tmp_root(),
        }
    }
}

/// Where a thread's transcript is.
enum Transcript {
    /// A file the source's parser reads whole (Codex rollout, Gemini or Qwen chat, an
    /// OpenCode JSON session file).
    File(PathBuf),
    /// A session inside OpenCode's SQLite DB.
    OpenCodeDb(PathBuf),
}

/// A thread to fill: (id, source kind, external id).
type Candidate = (i64, String, String);
/// Transcripts by (source kind, external id).
type Transcripts = HashMap<(String, String), Transcript>;

/// The threads to fill, and every transcript the scans recorded.
fn candidates(conn: &Connection, roots: &Roots) -> anyhow::Result<(Vec<Candidate>, Transcripts)> {
    let kinds = SOURCES.map(|k| format!("'{k}'")).join(", ");
    let mut stmt = conn.prepare(&format!(
        "{CANDIDATES_SQL} AND s.kind IN ({kinds}) ORDER BY t.id"
    ))?;
    let threads = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<Vec<Candidate>>>()?;
    let mut files = HashMap::new();
    if threads.is_empty() {
        return Ok((threads, files));
    }
    let mut stmt = conn.prepare(&format!(
        "SELECT source_kind, path FROM index_state WHERE source_kind IN ({kinds})"
    ))?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let opencode_db = threads
        .iter()
        .any(|(_, kind, _)| kind == opencode::KIND)
        .then(|| {
            rows.iter()
                .filter(|(k, p)| k == opencode::KIND && p.ends_with(".db"))
                .map(|(_, p)| PathBuf::from(p))
                .find(|p| p.is_file())
        })
        .flatten();
    for (kind, path) in rows {
        let path = PathBuf::from(path);
        let id = match kind.as_str() {
            codex::KIND => codex::rollout_session_id(&path),
            gemini::KIND => chat_id(roots.gemini.as_deref(), &path),
            qwen::KIND => chat_id(roots.qwen.as_deref(), &path),
            // A JSON session file is named for its session (`ses_<id>.json`) unless its
            // body says otherwise; the parse reads the body, so the stem is only the key.
            opencode::KIND if path.extension().is_some_and(|e| e == "json") => path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string),
            _ => None,
        };
        if let Some(id) = id {
            files.insert((kind, id), Transcript::File(path));
        }
    }
    // Every OpenCode session not in the JSON tree lives in the DB (SQLite wins in a scan
    // that reads both, so prefer it here too).
    if let Some(db) = opencode_db {
        for (_, kind, id) in threads.iter().filter(|(_, k, _)| k == opencode::KIND) {
            files.insert(
                (kind.clone(), id.clone()),
                Transcript::OpenCodeDb(db.clone()),
            );
        }
    }
    Ok((threads, files))
}

/// Threads with no usage row. The NOT EXISTS probe is one seek of the partial
/// `idx_messages_usage` (thread_id first, rows with a model only).
const CANDIDATES_SQL: &str = "SELECT t.id, s.kind, t.external_id
     FROM threads t JOIN sources s ON s.id = t.source_id
     WHERE NOT EXISTS (SELECT 1 FROM messages m WHERE m.thread_id = t.id AND m.model IS NOT NULL)";

/// A Gemini-layout chat's external id: its path below the tmp root, '/'-joined (as
/// `gemini::index_chat` keys it). None outside the root.
fn chat_id(root: Option<&Path>, path: &Path) -> Option<String> {
    let root = root?;
    path.starts_with(root)
        .then(|| super::claude::external_id_for(root, path))
}

/// Re-parse `kind`'s transcript for the thread `external_id`. None when it is gone or
/// yields nothing.
fn parse(
    kind: &str,
    external_id: &str,
    transcript: &Transcript,
    opencode_ro: &mut Option<(PathBuf, Connection)>,
) -> anyhow::Result<Option<ParsedThread>> {
    match transcript {
        Transcript::File(path) if !path.is_file() => Ok(None),
        Transcript::File(path) => match kind {
            codex::KIND => Ok(Some(codex::parse_rollout_thread(path)?)),
            gemini::KIND => gemini::parse_file(path, external_id),
            qwen::KIND => qwen::parse_file(path, external_id),
            opencode::KIND => opencode::json_session_thread(path),
            _ => Ok(None),
        },
        Transcript::OpenCodeDb(db) => {
            if opencode_ro.as_ref().is_none_or(|(p, _)| p != db) {
                *opencode_ro = Some((db.clone(), super::open_external_readonly(db)?));
            }
            let (_, ro) = opencode_ro.as_ref().expect("opened above");
            opencode::sqlite_session_thread(ro, external_id)
        }
    }
}

/// One thread's usage, keyed by the stored row it lands on.
struct Planned {
    thread_id: i64,
    /// The stored message count the plan was made against.
    count: i64,
    usage: Vec<(i64, MsgUsage)>,
}

/// Fill usage for every thread of the four sources that has none (see the module doc), in
/// batches: transcripts are parsed and aligned without the write lock, then each batch is
/// written in one short IMMEDIATE transaction that re-checks the thread still has no usage
/// and the same message count (the watcher may have re-indexed it meanwhile).
pub fn backfill_usage(
    db: &crate::db::Db,
    pause: &dyn Fn() -> bool,
    roots: &Roots,
) -> anyhow::Result<UsageBackfillReport> {
    let lock = || crate::db::lock_conn(&db.0);
    let started = Instant::now();
    let (threads, files) = candidates(&lock(), roots)?;
    let mut report = UsageBackfillReport {
        threads: threads.len(),
        ..Default::default()
    };
    let mut opencode_ro = None;
    let mut next = 0;
    while next < threads.len() {
        while pause() {
            std::thread::sleep(PAUSE_POLL);
        }
        let batch_start = Instant::now();
        let mut batch: Vec<Planned> = Vec::new();
        while next < threads.len() && batch.len() < BATCH && batch_start.elapsed() < BATCH_TIME {
            let (thread_id, kind, external_id) = &threads[next];
            next += 1;
            let key = (kind.clone(), external_id.clone());
            let parsed = match files.get(&key) {
                Some(t) => parse(kind, external_id, t, &mut opencode_ro).unwrap_or_else(|e| {
                    app_log!("[index] usage backfill: {kind} {external_id}: {e:#}");
                    None
                }),
                None => None,
            };
            let Some(parsed) = parsed else {
                report.missed += 1;
                continue;
            };
            if parsed.usage.is_empty() {
                continue; // a session that spent nothing the transcript recorded
            }
            let stored = stored_texts(&lock(), *thread_id)?;
            match place_usage(&parsed, &stored) {
                Some(usage) => batch.push(Planned {
                    thread_id: *thread_id,
                    count: stored.len() as i64,
                    usage,
                }),
                None => report.missed += 1,
            }
        }
        if !batch.is_empty() {
            let mut conn = lock();
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            for p in &batch {
                if write_planned(&tx, p)? {
                    report.filled += 1;
                    report.rows += p.usage.len();
                }
            }
            tx.commit()?;
        }
    }
    app_log!(
        "[index] usage backfill done: {} of {} sessions filled ({} rows, {} missed) in {:.1}s",
        report.filled,
        report.threads,
        report.rows,
        report.missed,
        started.elapsed().as_secs_f64()
    );
    Ok(report)
}

/// A thread's stored rows as (seq, role, tool name, text), in order.
type StoredRow = (i64, String, Option<String>, String);

fn stored_texts(conn: &Connection, thread_id: i64) -> rusqlite::Result<Vec<StoredRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT seq, role, tool_name, text FROM messages WHERE thread_id = ?1 ORDER BY seq",
    )?;
    let rows = stmt
        .query_map([thread_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect();
    rows
}

/// Write one planned thread's usage, unless it changed since the plan (usage arrived, or the
/// message count moved). Returns whether it wrote.
fn write_planned(tx: &Connection, p: &Planned) -> rusqlite::Result<bool> {
    let current: Option<(i64, bool)> = tx
        .query_row(
            "SELECT (SELECT COUNT(*) FROM messages WHERE thread_id = ?1),
                    EXISTS (SELECT 1 FROM messages WHERE thread_id = ?1 AND model IS NOT NULL)
             FROM threads WHERE id = ?1",
            [p.thread_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if current != Some((p.count, false)) {
        return Ok(false);
    }
    let mut upd = tx.prepare_cached(
        "UPDATE messages SET model = ?3, input_tokens = ?4, output_tokens = ?5,
                cache_write_tokens = ?6, cache_read_tokens = ?7
         WHERE thread_id = ?1 AND seq = ?2",
    )?;
    for (seq, u) in &p.usage {
        upd.execute(rusqlite::params![
            p.thread_id,
            seq,
            u.model,
            u.input,
            u.output,
            u.cache_write,
            u.cache_read
        ])?;
    }
    Ok(true)
}

/// For each parsed message, the stored seq holding the same message, or None when it is
/// not stored. Greedy and in order: a parsed message is looked for in the next
/// [`ALIGN_WINDOW`] stored rows, so stored rows a fresh parse no longer makes (re-recorded
/// Gemini copies) are skipped, and parsed messages the old index never stored (Codex
/// freeform calls) stay unmatched without moving the cursor.
fn align(parsed: &ParsedThread, stored: &[StoredRow]) -> Vec<Option<i64>> {
    let mut out = Vec::with_capacity(parsed.messages.len());
    let mut j = 0;
    for m in &parsed.messages {
        let end = (j + ALIGN_WINDOW).min(stored.len());
        let hit = (j..end).find(|&k| {
            let (_, role, tool, text) = &stored[k];
            *role == m.role
                && *tool == m.tool_name
                && (*text == m.text || super::is_recapped(m, text))
        });
        match hit {
            Some(k) => {
                out.push(Some(stored[k].0));
                j = k + 1;
            }
            None => out.push(None),
        }
    }
    out
}

/// The parse's usage keyed by stored seq: each call on its aligned row, or, when its row is
/// not stored, on the nearest stored row before it (after it, for a call before every
/// stored row). Calls landing on one row add up. None when nothing aligns.
fn place_usage(parsed: &ParsedThread, stored: &[StoredRow]) -> Option<Vec<(i64, MsgUsage)>> {
    let aligned = align(parsed, stored);
    let first = aligned.iter().flatten().next().copied()?;
    let mut out: Vec<(i64, MsgUsage)> = Vec::new();
    for (i, u) in &parsed.usage {
        let seq = aligned[..=(*i).min(aligned.len().saturating_sub(1))]
            .iter()
            .rev()
            .flatten()
            .next()
            .copied()
            .unwrap_or(first);
        match out.iter_mut().find(|(s, _)| *s == seq) {
            Some((_, have)) => {
                have.input += u.input;
                have.output += u.output;
                have.cache_write += u.cache_write;
                have.cache_read += u.cache_read;
            }
            None => out.push((seq, u.clone())),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::{set_file_state, source_id, upsert_thread, ParsedMessage};

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("calli_usage_bf_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn db_at(dir: &Path) -> crate::db::Db {
        crate::db::Db(std::sync::Mutex::new(
            crate::db::open(&dir.join("index.db")).unwrap(),
        ))
    }

    fn msg(role: &str, text: &str, tool: Option<&str>) -> ParsedMessage {
        ParsedMessage {
            role: role.to_string(),
            text: text.to_string(),
            tool_name: tool.map(str::to_string),
            ts: Some(1_700_000_000),
        }
    }

    /// Store `messages` the way an index from before usage did: no usage, file recorded.
    fn store_old(
        conn: &mut Connection,
        kind: &str,
        id: &str,
        file: &Path,
        messages: Vec<ParsedMessage>,
    ) {
        let sid = source_id(conn, kind).unwrap();
        let t = ParsedThread {
            external_id: id.to_string(),
            messages,
            ..Default::default()
        };
        upsert_thread(conn, sid, &t).unwrap();
        set_file_state(conn, &file.to_string_lossy(), kind, 1, 1).unwrap();
    }

    fn usage_rows(conn: &Connection, external_id: &str) -> Vec<(i64, String, i64)> {
        let mut stmt = conn
            .prepare(
                "SELECT m.seq, m.model, m.input_tokens FROM messages m
                 JOIN threads t ON t.id = m.thread_id
                 WHERE t.external_id = ?1 AND m.model IS NOT NULL ORDER BY m.seq",
            )
            .unwrap();
        stmt.query_map([external_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// The bug this exists for: sessions indexed before their source recorded usage, with
    /// files that never change again, get their usage written in place. A Codex thread from
    /// before 0034 lacks the `apply_patch` rows a fresh parse has; a Gemini thread kept each
    /// re-recorded copy. Message ids stay put and a second pass does nothing.
    #[test]
    fn older_sessions_get_their_usage_in_place() {
        let dir = scratch("fill");
        let db = db_at(&dir);
        let rollout =
            dir.join("rollout-2026-04-25T15-09-15-019dc6b0-998d-7a31-a39d-1229e6b38072.jsonl");
        std::fs::write(
            &rollout,
            [
                r#"{"timestamp":"2026-04-25T22:09:38.623Z","type":"session_meta","payload":{"id":"019dc6b0-998d-7a31-a39d-1229e6b38072","cwd":"/proj"}}"#,
                r#"{"timestamp":"2026-04-25T22:09:38.700Z","type":"turn_context","payload":{"cwd":"/proj","model":"gpt-5.3-codex"}}"#,
                r#"{"timestamp":"2026-04-25T22:09:39.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"fix it"}]}}"#,
                r#"{"timestamp":"2026-04-25T22:09:40.000Z","type":"response_item","payload":{"type":"custom_tool_call","name":"apply_patch","input":"*** Begin Patch\n*** Update File: a.rs\n*** End Patch"}}"#,
                r#"{"timestamp":"2026-04-25T22:09:41.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":0,"output_tokens":50}}}}"#,
                r#"{"timestamp":"2026-04-25T22:09:43.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}}"#,
                r#"{"timestamp":"2026-04-25T22:09:44.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1400,"cached_input_tokens":0,"output_tokens":60}}}}"#,
            ]
            .join("\n"),
        )
        .unwrap();
        let gem_root = dir.join("gemini-tmp");
        let chat = gem_root.join("p/chats/session-1.jsonl");
        std::fs::create_dir_all(chat.parent().unwrap()).unwrap();
        std::fs::write(
            &chat,
            r#"{"sessionId":"s","startTime":"2026-06-01T10:00:00.000Z","directories":["/p"]}
{"id":"m1","type":"user","timestamp":"2026-06-01T10:00:00.000Z","content":"hello"}
{"id":"m2","type":"gemini","timestamp":"2026-06-01T10:00:05.000Z","content":"hi there","model":"gemini-2.5-pro"}
{"id":"m2","type":"gemini","timestamp":"2026-06-01T10:00:05.000Z","content":"hi there","model":"gemini-2.5-pro","tokens":{"input":1200,"output":40,"cached":1000,"thoughts":10,"tool":0,"total":1250}}
"#,
        )
        .unwrap();
        {
            let mut conn = db.0.lock().unwrap();
            store_old(
                &mut conn,
                codex::KIND,
                "019dc6b0-998d-7a31-a39d-1229e6b38072",
                &rollout,
                vec![msg("user", "fix it", None), msg("assistant", "done", None)],
            );
            // The old Gemini parse kept both copies of m2.
            store_old(
                &mut conn,
                gemini::KIND,
                "p/chats/session-1.jsonl",
                &chat,
                vec![
                    msg("user", "hello", None),
                    msg("assistant", "hi there", None),
                    msg("assistant", "hi there", None),
                ],
            );
            // A thread whose transcript is gone.
            store_old(
                &mut conn,
                codex::KIND,
                "gone",
                &dir.join("rollout-x-gone.jsonl"),
                vec![msg("user", "x", None)],
            );
        }
        let ids_before: Vec<i64> = {
            let conn = db.0.lock().unwrap();
            let mut s = conn.prepare("SELECT id FROM messages ORDER BY id").unwrap();
            s.query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        let roots = Roots {
            gemini: Some(gem_root.clone()),
            qwen: None,
        };
        let r = backfill_usage(&db, &|| false, &roots).unwrap();
        assert_eq!(
            r,
            UsageBackfillReport {
                threads: 3,
                filled: 2,
                rows: 3,
                missed: 1
            }
        );
        let conn = db.0.lock().unwrap();
        // The patch call is not stored: its call's tokens land on the row before it.
        assert_eq!(
            usage_rows(&conn, "019dc6b0-998d-7a31-a39d-1229e6b38072"),
            vec![
                (0, "gpt-5.3-codex".into(), 1000),
                (1, "gpt-5.3-codex".into(), 400)
            ]
        );
        assert_eq!(
            usage_rows(&conn, "p/chats/session-1.jsonl"),
            vec![(1, "gemini-2.5-pro".into(), 200)]
        );
        let ids_after: Vec<i64> = {
            let mut s = conn.prepare("SELECT id FROM messages ORDER BY id").unwrap();
            s.query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(ids_before, ids_after, "no row replaced");
        drop(conn);
        let again = backfill_usage(&db, &|| false, &roots).unwrap();
        assert_eq!(
            (again.threads, again.filled),
            (1, 0),
            "only the missing one is left"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An OpenCode session stored without usage is re-read from `session_v2` when the DB holds
    /// it there (on a DB with no V1 tables at all), and its per-step usage lands on its rows.
    #[test]
    fn opencode_v2_sessions_get_their_usage() {
        use super::opencode::fixture::*;
        let dir = scratch("oc_v2");
        let db = db_at(&dir);
        let oc = dir.join("opencode.db");
        {
            let c = Connection::open(&oc).unwrap();
            c.execute_batch(V2_DDL).unwrap();
            c.execute_batch(PARTS_DDL).unwrap();
            session(&c, "ses_x", Some("V2"), None, None);
            message(&c, "msg_1", "ses_x", 0, "user", user("hello v2"));
            let step = assistant(serde_json::json!([]), Some(tokens(11, 3, 0, 0, 0)));
            message(&c, "msg_2", "ses_x", 1, "assistant", step);
            part(
                &c,
                "msg_2",
                "ses_x",
                0,
                serde_json::json!({"type": "text", "text": "hi from v2"}),
            );
        }
        {
            let mut conn = db.0.lock().unwrap();
            store_old(
                &mut conn,
                opencode::KIND,
                "ses_x",
                &oc,
                vec![
                    msg("user", "hello v2", None),
                    msg("assistant", "hi from v2", None),
                ],
            );
        }
        let roots = Roots {
            gemini: None,
            qwen: None,
        };
        let r = backfill_usage(&db, &|| false, &roots).unwrap();
        assert_eq!((r.threads, r.filled, r.missed), (1, 1, 0));
        let conn = db.0.lock().unwrap();
        assert_eq!(
            usage_rows(&conn, "ses_x"),
            vec![(1, "claude-sonnet-4-5".into(), 11)]
        );
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Runs once: the flag set, a later call is a no-op; a fresh index is marked done.
    #[test]
    fn runs_once_and_a_fresh_index_skips_it() {
        let dir = scratch("once");
        let db = db_at(&dir);
        assert!(backfill_usage_once(&db, &|| false).unwrap().is_some());
        assert!(backfill_usage_once(&db, &|| false).unwrap().is_none());
        let dir2 = scratch("fresh");
        let db2 = db_at(&dir2);
        mark_usage_backfilled(&db2.0.lock().unwrap()).unwrap();
        assert!(backfill_usage_once(&db2, &|| false).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn the_candidate_probe_stays_on_the_usage_index() {
        let dir = scratch("plan");
        let db = db_at(&dir);
        let conn = db.0.lock().unwrap();
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {CANDIDATES_SQL}"))
            .unwrap()
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        let plan = plan.join("\n");
        assert!(plan.contains("idx_messages_usage"), "{plan}");
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
