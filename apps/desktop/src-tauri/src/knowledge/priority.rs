//! The auto-distill priority queue: sessions that just ended, distilled ahead of the
//! recent-and-idle worklist. The SessionEnd hooks of Claude Code, Codex and Gemini CLI
//! (`cal session-end [--for codex|gemini]`) add the session; `auto_distill_worklist` drains
//! it first. A session that ended is done, so the
//! idle wait (which only guards against distilling a live session) and the 30-day window do
//! not apply, but each entry still counts against the daily cap.
//!
//! Stored in `app_config` under [`PRIORITY_KEY`] as a JSON array of `{ "ref", "ts" }`,
//! newest first, so the hook (a separate process) and the app share it without a schema
//! change. References are `claude:<session_id>`, `codex:<session_id>`,
//! `gemini:<session_id>` or `thread:<id>`.

use super::app_config;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

/// `app_config` key holding the queue.
pub const PRIORITY_KEY: &str = "knowledge.distill_priority";

/// Most entries kept. A day of sessions fits many times over; the daily cap, not this,
/// bounds what is sent.
const PRIORITY_MAX: usize = 50;

/// Entries older than this are dropped: past a week, "the session that just ended" is just
/// history, which the regular worklist (or a project's Build memory) covers.
const PRIORITY_TTL_SECS: i64 = 7 * 86_400;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    #[serde(rename = "ref")]
    reference: String,
    ts: i64,
}

fn load(conn: &Connection) -> Result<Vec<Entry>> {
    // A malformed value (hand-edited, or a future shape) reads as empty instead of
    // wedging every enqueue and drain on a parse error.
    Ok(app_config::get(conn, PRIORITY_KEY)?
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default())
}

fn save(conn: &Connection, entries: &[Entry]) -> Result<()> {
    if entries.is_empty() {
        return app_config::delete(conn, PRIORITY_KEY);
    }
    app_config::put(conn, PRIORITY_KEY, &serde_json::to_string(entries)?)
}

/// Run `f` on the queue as one read-modify-write. The hook and the app are different
/// processes, so outside a caller's transaction this takes the write lock first (IMMEDIATE),
/// and a concurrent enqueue can never be lost between the read and the write.
fn update<T>(conn: &Connection, f: impl FnOnce(&mut Vec<Entry>) -> Result<T>) -> Result<T> {
    let tx = conn
        .is_autocommit()
        .then(|| rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate))
        .transpose()?;
    let mut entries = load(conn)?;
    let before = entries.clone();
    let out = f(&mut entries)?;
    if entries != before {
        save(conn, &entries)?;
    }
    if let Some(tx) = tx {
        tx.commit()?;
    }
    Ok(out)
}

/// An agent session id as it appears in a transcript file name (Claude Code, Gemini CLI) or
/// as the thread's external id (Codex). Anything else (empty, a path, LIKE wildcards) is
/// refused, since the id ends up in a LIKE pattern.
fn valid_session_id(sid: &str) -> bool {
    !sid.is_empty()
        && sid.len() <= 128
        && sid
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The agent prefixes a session reference can carry, each resolved against its own
/// source's threads (see [`resolve`]).
const SESSION_PREFIXES: [&str; 3] = ["claude:", "codex:", "gemini:"];

fn valid_reference(reference: &str) -> bool {
    if let Some(sid) = SESSION_PREFIXES
        .iter()
        .find_map(|p| reference.strip_prefix(p))
    {
        return valid_session_id(sid);
    }
    reference
        .strip_prefix("thread:")
        .is_some_and(|id| id.parse::<i64>().is_ok())
}

/// Put `reference` (`claude:` / `codex:` / `gemini:<session_id>` or `thread:<id>`) at the front of the priority
/// queue, dropping an older copy of it, entries past a week, and anything past
/// [`PRIORITY_MAX`]. One small `app_config` write: cheap enough for a SessionEnd hook.
pub fn enqueue_distill_priority(conn: &Connection, reference: &str, now: i64) -> Result<()> {
    if !valid_reference(reference) {
        anyhow::bail!("not a distill reference: {reference:?}");
    }
    update(conn, |q| {
        q.retain(|e| e.reference != reference && now - e.ts <= PRIORITY_TTL_SECS);
        q.insert(
            0,
            Entry {
                reference: reference.to_string(),
                ts: now,
            },
        );
        q.truncate(PRIORITY_MAX);
        Ok(())
    })
}

/// The indexed thread a Claude Code session id names: the top-level transcript
/// `<project slug>/<sid>.jsonl` (or `<sid>.jsonl` at the projects root). The suffix match
/// can't seek, but it walks only the claude_code range of the (source_id, external_id)
/// unique index, covering: 3-5 ms over 20,033 claude_code threads on a 21,289-thread index.
const CLAUDE_SESSION_SQL: &str = "SELECT t.id FROM threads t
     WHERE t.source_id = (SELECT id FROM sources WHERE kind = 'claude_code')
       AND (t.external_id = ?1 OR t.external_id LIKE ?2 ESCAPE '\\')
     ORDER BY t.id LIMIT 1";

/// The indexed Codex thread for a session id. Codex keys a thread by its session id (the
/// state DB's `threads.id`, the `session_meta` id of a loose rollout, and the uuid at the end
/// of the rollout file name are the same value), so this is one seek on the
/// (source_id, external_id) unique index.
const CODEX_SESSION_SQL: &str = "SELECT t.id FROM threads t
     WHERE t.source_id = (SELECT id FROM sources WHERE kind = 'codex')
       AND t.external_id = ?1";

/// Gemini CLI top-level chat files are `<project>/chats/session-<YYYY-MM-DDTHH-MM>-<first 8
/// chars of the session id>.jsonl` (chatRecordingService.ts; subagent files nest one level
/// deeper as `<parent id>/<id>.jsonl` and are never queued), so a session id only narrows to
/// its 8-char prefix here; [`gemini_session_matches`] confirms against the file's own
/// `sessionId`. Like the Claude lookup, the suffix match walks the gemini range of the
/// covering unique index.
const GEMINI_SESSION_SQL: &str = "SELECT t.id, t.external_id FROM threads t
     WHERE t.source_id = (SELECT id FROM sources WHERE kind = 'gemini')
       AND t.external_id LIKE ?1 ESCAPE '\\'
     ORDER BY t.id LIMIT 16";

/// The thread a Codex session id names, if it is indexed (also what `cal snapshot-hook --for
/// codex` snapshots).
pub fn codex_thread_for_session(conn: &Connection, sid: &str) -> Result<Option<i64>> {
    if !valid_session_id(sid) {
        return Ok(None);
    }
    Ok(conn
        .prepare_cached(CODEX_SESSION_SQL)?
        .query_row([sid], |r| r.get(0))
        .optional()?)
}

/// The thread a Gemini CLI session id names, if it is indexed. `root` is Gemini's `tmp` dir
/// (the external ids are relative to it), read to tell apart two sessions that share an
/// 8-char prefix. A single candidate whose file is gone (deleted after indexing) still counts:
/// the prefix is 32 bits of a random uuid.
fn gemini_thread_for_session(
    conn: &Connection,
    sid: &str,
    root: Option<&std::path::Path>,
) -> Result<Option<i64>> {
    if !valid_session_id(sid) {
        return Ok(None);
    }
    let short: String = sid.chars().take(8).collect();
    let pattern = format!("%/chats/session-%-{}.jsonl", short.replace('_', "\\_"));
    let candidates: Vec<(i64, String)> = conn
        .prepare_cached(GEMINI_SESSION_SQL)?
        .query_map([pattern], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let lone = candidates.len() == 1;
    for (id, external_id) in candidates {
        match root.map(|r| gemini_session_matches(&r.join(&external_id), sid)) {
            Some(Some(true)) => return Ok(Some(id)),
            Some(Some(false)) => continue,
            // Unreadable file or no root: trust a lone candidate only.
            _ if lone => return Ok(Some(id)),
            _ => continue,
        }
    }
    Ok(None)
}

/// Whether the Gemini chat file at `path` records session `sid` in its first-line metadata
/// (`sessionId`); None when the file or that line can't be read.
fn gemini_session_matches(path: &std::path::Path, sid: &str) -> Option<bool> {
    use std::io::{BufRead, Read};
    let file = std::fs::File::open(path).ok()?;
    let mut first = String::new();
    // The metadata record is a few hundred bytes; cap the read so a corrupt file without
    // newlines is not slurped whole.
    std::io::BufReader::new(file)
        .take(64 * 1024)
        .read_line(&mut first)
        .ok()?;
    let meta: serde_json::Value = serde_json::from_str(first.trim()).ok()?;
    Some(meta.get("sessionId")?.as_str()? == sid)
}

/// The thread `reference` names, if it is indexed.
fn resolve(conn: &Connection, reference: &str) -> Result<Option<i64>> {
    if let Some(sid) = reference.strip_prefix("codex:") {
        return codex_thread_for_session(conn, sid);
    }
    if let Some(sid) = reference.strip_prefix("gemini:") {
        let root = crate::indexer::gemini::tmp_root();
        return gemini_thread_for_session(conn, sid, root.as_deref());
    }
    if let Some(sid) = reference.strip_prefix("claude:") {
        if !valid_session_id(sid) {
            return Ok(None);
        }
        let file = format!("{sid}.jsonl");
        // `_` is allowed in ids and is a LIKE wildcard, so it is escaped (`%` never gets
        // past valid_session_id).
        let pattern = format!("%/{}", file.replace('_', "\\_"));
        let mut stmt = conn.prepare_cached(CLAUDE_SESSION_SQL)?;
        return Ok(stmt
            .query_row(rusqlite::params![file, pattern], |r| r.get(0))
            .optional()?);
    }
    let Some(id) = reference
        .strip_prefix("thread:")
        .and_then(|id| id.parse::<i64>().ok())
    else {
        return Ok(None);
    };
    Ok(conn
        .query_row("SELECT id FROM threads WHERE id = ?1", [id], |r| r.get(0))
        .optional()?)
}

/// How long after the SessionEnd hook queued a session it counts as ready even when the
/// thread has not been re-indexed since. The session's last turns can still be in the
/// watcher's debounce (800 ms) and batch when the minute tick or another session's batch
/// kicks a drain; distilling then would run on a stale copy, the tail would make the thread
/// pending again, and the regular worklist would distill it a second time (two LLM calls,
/// two units of the daily cap). A re-index at or after the hook skips the wait.
const SETTLE_SECS: i64 = 15;

/// `last_indexed_at` of thread `id` when it is waiting for a distill and worth one (the
/// same predicate as the regular worklist, without its recency and idle filters); None
/// when it is not pending.
fn pending_indexed_at(conn: &Connection, id: i64) -> Result<Option<i64>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT COALESCE(last_indexed_at, 0) FROM threads WHERE id = ?1 AND {}",
            super::pending_predicate()
        ))?
        .query_row([id], |r| r.get(0))
        .optional()?)
}

/// What the queue holds right now: the pending thread of each live entry in queue order
/// (deduped), and which entries are done with (expired, or resolved to a thread that is
/// no longer pending: distilled, errored, too short). Entries whose session is not indexed
/// yet, or not settled (see [`SETTLE_SECS`]), stay, waiting for the watcher.
struct Scan {
    ready: Vec<(usize, i64)>,
    drop: Vec<usize>,
}

fn scan(conn: &Connection, entries: &[Entry], now: i64) -> Result<Scan> {
    let mut out = Scan {
        ready: Vec::new(),
        drop: Vec::new(),
    };
    for (i, e) in entries.iter().enumerate() {
        if now - e.ts > PRIORITY_TTL_SECS {
            out.drop.push(i);
            continue;
        }
        let Some(id) = resolve(conn, &e.reference)? else {
            continue;
        };
        if out.ready.iter().any(|(_, t)| *t == id) {
            out.drop.push(i);
            continue;
        }
        match pending_indexed_at(conn, id)? {
            None => out.drop.push(i),
            Some(indexed) if indexed >= e.ts || now - e.ts >= SETTLE_SECS => {
                out.ready.push((i, id))
            }
            Some(_) => {} // the last turns may still be on their way in
        }
    }
    Ok(out)
}

/// Take up to `take` queued threads for an auto-distill round, newest entry first, and
/// remove them (and every finished or expired entry) from the queue. The caller counts
/// them against the daily cap.
pub(super) fn take_priority(conn: &Connection, take: i64, now: i64) -> Result<Vec<i64>> {
    if take <= 0 {
        return Ok(Vec::new());
    }
    update(conn, |q| {
        let s = scan(conn, q, now)?;
        let taken: Vec<(usize, i64)> = s.ready.into_iter().take(take as usize).collect();
        let mut remove: Vec<usize> = s.drop;
        remove.extend(taken.iter().map(|(i, _)| *i));
        let mut i = 0;
        q.retain(|_| {
            let keep = !remove.contains(&i);
            i += 1;
            keep
        });
        Ok(taken.into_iter().map(|(_, id)| id).collect())
    })
}

/// The queued threads that are ready to distill, read-only (for the Settings count and
/// the watcher's "kick now?" check, which may run on a read-only connection).
pub(super) fn ready_priority(conn: &Connection, now: i64) -> Result<Vec<i64>> {
    let entries = load(conn)?;
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    Ok(scan(conn, &entries, now)?
        .ready
        .into_iter()
        .map(|(_, id)| id)
        .collect())
}

/// Whether an auto-distill kick now would distill a queued session: distillation and
/// auto-distill are on, today's allowance is not spent, and a queued session is indexed
/// and pending. The watcher asks after each batch it indexes and on its minute tick, since
/// the SessionEnd hook only writes the queue and cannot start a distill itself.
pub fn distill_priority_ready(conn: &Connection, now: i64) -> Result<bool> {
    if app_config::get(conn, PRIORITY_KEY)?.is_none() {
        return Ok(false); // the common case: one primary-key lookup
    }
    let cfg = super::get_config(conn)?;
    if !cfg.enabled || !cfg.auto_distill || super::auto_distill_remaining_today(conn, now)? <= 0 {
        return Ok(false);
    }
    Ok(!ready_priority(conn, now)?.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> Connection {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "calli_prio_{}_{}.db",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        crate::db::open(&p).unwrap()
    }

    /// A claude_code thread `<slug>/<sid>.jsonl` with `n` user/assistant turns, updated at
    /// `at`.
    fn session(conn: &mut Connection, sid: &str, n: usize, at: i64) -> i64 {
        thread_in(
            conn,
            "claude_code",
            &format!("-Users-me-proj/{sid}.jsonl"),
            n,
            at,
        )
    }

    /// A thread of source `kind` keyed `external_id`, with `n` turns updated at `at`.
    fn thread_in(conn: &mut Connection, kind: &str, external_id: &str, n: usize, at: i64) -> i64 {
        use crate::indexer::{source_id, upsert_thread, ParsedMessage, ParsedThread};
        let sid = external_id;
        let src = source_id(conn, kind).unwrap();
        upsert_thread(
            conn,
            src,
            &ParsedThread {
                external_id: external_id.to_string(),
                title: Some(sid.into()),
                project_path: Some("/Users/me/proj".into()),
                created_at: Some(at),
                updated_at: Some(at),
                messages: (0..n)
                    .map(|i| ParsedMessage {
                        role: if i % 2 == 0 { "user" } else { "assistant" }.into(),
                        text: format!("turn {i}"),
                        tool_name: None,
                        ts: Some(at),
                    })
                    .collect(),
                ..Default::default()
            },
        )
        .unwrap();
        conn.query_row(
            "SELECT id FROM threads WHERE external_id = ?1",
            [external_id],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn plan(conn: &Connection, sql: &str, params: impl rusqlite::Params) -> String {
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        stmt.query_map(params, |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join(" | ")
    }

    #[test]
    fn codex_and_gemini_lookups_use_the_unique_index() {
        let conn = temp_db();
        let codex = plan(&conn, CODEX_SESSION_SQL, ["019dc6b0-998d"]);
        assert!(
            codex.contains(
                "COVERING INDEX sqlite_autoindex_threads_1 (source_id=? AND external_id=?)"
            ),
            "{codex}"
        );
        let gemini = plan(
            &conn,
            GEMINI_SESSION_SQL,
            ["%/chats/session-%-abcd1234.jsonl"],
        );
        assert!(
            gemini.contains("COVERING INDEX sqlite_autoindex_threads_1 (source_id=?)"),
            "{gemini}"
        );
    }

    /// `codex:<sid>` resolves to the codex thread keyed by that id, never a claude one.
    #[test]
    fn codex_sessions_resolve_by_external_id() {
        let mut conn = temp_db();
        let sid = "019dc6b0-998d-7a31-a39d-1229e6b38072";
        let id = thread_in(&mut conn, "codex", sid, 4, 1_000);
        thread_in(
            &mut conn,
            "claude_code",
            &format!("p/{sid}.jsonl"),
            4,
            1_000,
        );
        assert_eq!(resolve(&conn, &format!("codex:{sid}")).unwrap(), Some(id));
        assert_eq!(resolve(&conn, "codex:019dc6b0").unwrap(), None);
        enqueue_distill_priority(&conn, &format!("codex:{sid}"), 5).unwrap();
        enqueue_distill_priority(&conn, &format!("gemini:{sid}"), 6).unwrap();
        assert!(enqueue_distill_priority(&conn, "codex:a%b", 7).is_err());
    }

    /// `gemini:<sid>` narrows by the 8-char prefix in the chat file name, then the file's
    /// own `sessionId` picks between sessions sharing it; a lone candidate whose file is
    /// gone still resolves.
    #[test]
    fn gemini_sessions_resolve_through_the_chat_file() {
        let mut conn = temp_db();
        let root = std::env::temp_dir().join(format!("calli_prio_gem_{}", std::process::id()));
        let chats = root.join("proj").join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        let a = "abcd1234-0000-4000-8000-00000000000a";
        let b = "abcd1234-0000-4000-8000-00000000000b";
        let file_a = "proj/chats/session-2026-11-06T10-00-abcd1234.jsonl";
        let file_b = "proj/chats/session-2026-11-06T11-00-abcd1234.jsonl";
        for (file, sid) in [(file_a, a), (file_b, b)] {
            std::fs::write(
                root.join(file),
                format!(
                    "{{\"sessionId\":\"{sid}\",\"startTime\":\"2026-11-06T10:00:00Z\"}}\n{{}}\n"
                ),
            )
            .unwrap();
        }
        let ta = thread_in(&mut conn, "gemini", file_a, 4, 1_000);
        let tb = thread_in(&mut conn, "gemini", file_b, 4, 1_000);
        // A subagent file never matches the top-level name shape.
        thread_in(
            &mut conn,
            "gemini",
            &format!("proj/chats/{a}/{a}.jsonl"),
            4,
            1_000,
        );
        assert_eq!(
            gemini_thread_for_session(&conn, a, Some(&root)).unwrap(),
            Some(ta)
        );
        assert_eq!(
            gemini_thread_for_session(&conn, b, Some(&root)).unwrap(),
            Some(tb)
        );
        assert_eq!(
            gemini_thread_for_session(&conn, a, None).unwrap(),
            None,
            "two candidates and no file to tell them apart"
        );
        assert_eq!(
            gemini_thread_for_session(&conn, "ffff0000-1", Some(&root)).unwrap(),
            None
        );
        let gone = "cccc5678-0000-4000-8000-00000000000c";
        let tc = thread_in(
            &mut conn,
            "gemini",
            "proj/chats/session-2026-11-05T09-00-cccc5678.jsonl",
            4,
            1_000,
        );
        assert_eq!(
            gemini_thread_for_session(&conn, gone, Some(&root)).unwrap(),
            Some(tc),
            "a lone candidate whose file is gone"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn queue(conn: &Connection) -> Vec<String> {
        load(conn)
            .unwrap()
            .into_iter()
            .map(|e| e.reference)
            .collect()
    }

    #[test]
    fn enqueue_is_newest_first_deduped_capped_and_validated() {
        let conn = temp_db();
        enqueue_distill_priority(&conn, "claude:a", 10).unwrap();
        enqueue_distill_priority(&conn, "thread:7", 11).unwrap();
        enqueue_distill_priority(&conn, "claude:a", 12).unwrap();
        assert_eq!(queue(&conn), ["claude:a", "thread:7"]);
        for i in 0..60 {
            enqueue_distill_priority(&conn, &format!("thread:{i}"), 20 + i).unwrap();
        }
        assert_eq!(queue(&conn).len(), PRIORITY_MAX);
        assert_eq!(queue(&conn)[0], "thread:59");
        for bad in [
            "claude:",
            "claude:../x",
            "claude:a%",
            "thread:x",
            "nope:1",
            "",
        ] {
            assert!(enqueue_distill_priority(&conn, bad, 1).is_err(), "{bad}");
        }
        // Entries past a week fall off on the next enqueue.
        enqueue_distill_priority(&conn, "claude:late", 20 + PRIORITY_TTL_SECS + 100).unwrap();
        assert_eq!(queue(&conn), ["claude:late"]);
    }

    #[test]
    fn claude_session_lookup_walks_the_covering_index() {
        let conn = temp_db();
        let mut stmt = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {CLAUDE_SESSION_SQL}"))
            .unwrap();
        let plan = stmt
            .query_map(rusqlite::params!["a.jsonl", "%/a.jsonl"], |r| {
                r.get::<_, String>(3)
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join(" | ");
        assert!(
            plan.contains("COVERING INDEX sqlite_autoindex_threads_1 (source_id=?)"),
            "{plan}"
        );
    }

    /// The worklist takes queued sessions first, ignoring the idle wait and the 30-day
    /// window, counts them against the cap, and drops entries once taken, finished or
    /// expired. Unindexed sessions wait.
    #[test]
    fn priority_drain_order_cap_and_expiry() {
        let mut conn = temp_db();
        let now = 1_800_000_000;
        // `live` ended a minute ago (the regular worklist would wait 30 minutes), `old` is
        // two months old (outside the 30-day window), `idle` is ordinary pending history.
        let live = session(&mut conn, "live", 4, now - 60);
        let old = session(&mut conn, "old", 4, now - 60 * 86_400);
        let idle = session(&mut conn, "idle", 4, now - 7_200);
        let short = session(&mut conn, "short", 2, now - 60);
        enqueue_distill_priority(&conn, "claude:old", now - 40).unwrap();
        enqueue_distill_priority(&conn, &format!("thread:{short}"), now - 39).unwrap();
        enqueue_distill_priority(&conn, "claude:not-indexed-yet", now - 38).unwrap();
        enqueue_distill_priority(&conn, "claude:live", now - 35).unwrap();

        assert_eq!(
            super::super::auto_distill_eligible_count(&conn, now).unwrap(),
            3
        );
        super::super::set_auto_distill_daily_cap(&conn, 2).unwrap();
        let first = super::super::auto_distill_worklist(&conn, 25, now).unwrap();
        assert_eq!(first, vec![live, old], "queue order, newest first, capped");
        assert_eq!(
            super::super::auto_distill_remaining_today(&conn, now).unwrap(),
            0
        );
        // Taken and finished (`short` is too short to distill) entries are gone; the
        // unindexed session waits for the watcher.
        assert_eq!(queue(&conn), ["claude:not-indexed-yet"]);

        // Once those are distilled, the next day's round is the regular worklist.
        conn.execute(
            "UPDATE threads SET knowledge_extracted = 1, knowledge_extracted_at = ?1,
                knowledge_msg_count = distillable_count WHERE id IN (?2, ?3)",
            rusqlite::params![now, live, old],
        )
        .unwrap();
        super::super::set_auto_distill_daily_cap(&conn, 10).unwrap();
        let next = super::super::auto_distill_worklist(&conn, 25, now + 86_400).unwrap();
        assert_eq!(next, vec![idle]);

        // Past a week, the waiting entry expires.
        assert!(take_priority(&conn, 5, now + 8 * 86_400)
            .unwrap()
            .is_empty());
        assert!(queue(&conn).is_empty());
        assert!(app_config::get(&conn, PRIORITY_KEY).unwrap().is_none());
    }

    /// A session queued moments ago waits for its last turns: ready once the thread is
    /// re-indexed at or after the hook, or once [`SETTLE_SECS`] pass without one.
    #[test]
    fn a_just_ended_session_waits_to_settle() {
        let mut conn = temp_db();
        let now = 1_800_000_000;
        let id = session(&mut conn, "fresh", 4, now - 60);
        let indexed_at = |conn: &Connection, at: i64| {
            conn.execute(
                "UPDATE threads SET last_indexed_at = ?1 WHERE id = ?2",
                [at, id],
            )
            .unwrap();
        };
        indexed_at(&conn, now - 30);
        enqueue_distill_priority(&conn, "claude:fresh", now - 2).unwrap();
        assert!(
            ready_priority(&conn, now).unwrap().is_empty(),
            "tail may be in flight"
        );
        assert_eq!(queue(&conn), ["claude:fresh"], "kept, not dropped");
        assert_eq!(
            ready_priority(&conn, now - 2 + SETTLE_SECS).unwrap(),
            vec![id],
            "settled without a re-index"
        );
        indexed_at(&conn, now - 1);
        assert_eq!(
            ready_priority(&conn, now).unwrap(),
            vec![id],
            "re-indexed after the hook"
        );
    }

    #[test]
    fn ready_check_needs_memory_on_and_a_pending_session() {
        let mut conn = temp_db();
        let now = 1_800_000_000;
        assert!(!distill_priority_ready(&conn, now).unwrap());
        enqueue_distill_priority(&conn, "claude:s1", now - SETTLE_SECS).unwrap();
        assert!(
            !distill_priority_ready(&conn, now).unwrap(),
            "not indexed yet"
        );
        session(&mut conn, "s1", 4, now - 30);
        assert!(
            !distill_priority_ready(&conn, now).unwrap(),
            "memory is off"
        );
        app_config::put_flag(&conn, "knowledge.enabled", true).unwrap();
        super::super::set_auto_distill(&conn, true).unwrap();
        assert!(distill_priority_ready(&conn, now).unwrap());
        super::super::set_auto_distill_daily_cap(&conn, 0).unwrap();
        assert!(!distill_priority_ready(&conn, now).unwrap(), "cap spent");
    }
}
