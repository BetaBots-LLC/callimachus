//! Agent session snapshots: durable, resumable checkpoints of an indexed thread. A snapshot
//! captures the thread's packed transcript (via `context::pack_thread`) plus a carry-forward
//! block of the project's distilled decisions / gotchas / open TODOs (via
//! `knowledge::get_project_memory`), so the next agent can continue across a context-window
//! compaction or across tools (Claude Code -> Codex -> Cursor). Resume reuses
//! `agent::cli_resume::launch_with_context` to relaunch any agent CLI with the checkpoint.

use crate::{context, export, knowledge, search};
use anyhow::{anyhow, Result};
use rusqlite::{Connection, OptionalExtension, ToSql};
use serde::Serialize;

/// Char budget for the packed transcript inside a snapshot (≈12k tokens, same ladder as the
/// MCP `get_thread` default).
const SNAPSHOT_BUDGET_CHARS: usize = context::DEFAULT_BUDGET_CHARS;

/// Snapshot metadata (no body), for lists.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub id: i64,
    pub thread_id: Option<i64>,
    pub project_path: Option<String>,
    pub source_kind: Option<String>,
    pub label: String,
    pub token_estimate: i64,
    pub created_at: i64,
}

/// A snapshot plus its loadable checkpoint body.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotDetail {
    #[serde(flatten)]
    pub meta: Snapshot,
    pub body: String,
}

/// Assemble a checkpoint body: the carry-forward project memory first (so the next agent reads
/// the durable decisions before the transcript), a divider, then the packed transcript.
fn assemble(carry: &str, transcript: &str) -> String {
    let mut body = String::new();
    if !carry.trim().is_empty() {
        body.push_str(carry.trim_end());
        body.push_str("\n\n---\n\n");
    }
    body.push_str(transcript);
    body
}

/// A snapshot assembled but not yet stored.
struct NewSnapshot {
    thread_id: i64,
    project_path: Option<String>,
    source_kind: String,
    label: String,
    body: String,
    token_estimate: i64,
}

/// Snapshot an indexed thread. `label` defaults to the thread title (then a generic fallback).
pub fn create(conn: &Connection, thread_id: i64, label: Option<&str>) -> Result<Snapshot> {
    let new = build(conn, thread_id, label)?;
    insert(conn, new)
}

/// Pack a thread into a snapshot (reads only).
fn build(conn: &Connection, thread_id: i64, label: Option<&str>) -> Result<NewSnapshot> {
    let detail = search::thread_detail(conn, thread_id)?
        .ok_or_else(|| anyhow!("thread {thread_id} not found"))?;
    let transcript = context::pack_thread(conn, thread_id, SNAPSHOT_BUDGET_CHARS)?
        .ok_or_else(|| anyhow!("thread {thread_id} has no packable content"))?;

    // Carry-forward uses the thread's CANONICAL project key (the same COALESCE the rest of the
    // app aggregates memory by), not the raw path, so the lookup actually matches.
    let project_key: Option<String> = conn
        .query_row(
            "SELECT COALESCE(project_key, project_path) FROM threads WHERE id = ?1",
            [thread_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    // The carry block is a short refresher (COMPACT budget), not the whole memory: the
    // resumed session can still pull more with `cal memory` / project_memory, and its own
    // SessionStart hook may inject memory too.
    let carry = match project_key.as_deref() {
        Some(pk) if !pk.is_empty() => {
            let budget = export::MemoryBudget::COMPACT;
            let mem = knowledge::get_project_memory(conn, pk, budget.fetch_limit())?;
            if mem.decisions.is_empty()
                && mem.gotchas.is_empty()
                && !mem.open_todos.iter().any(|t| t.pinned)
            {
                String::new()
            } else {
                export::agent_memory_md_budgeted(pk, &mem, None, &budget)
            }
        }
        _ => String::new(),
    };

    let body = assemble(&carry, &transcript);
    let label = label
        .map(str::to_string)
        .or_else(|| detail.title.clone())
        .unwrap_or_else(|| format!("snapshot of thread {thread_id}"));
    let token_estimate = (body.len() / 4) as i64;
    Ok(NewSnapshot {
        thread_id,
        project_path: detail.project_path,
        source_kind: detail.source,
        label,
        body,
        token_estimate,
    })
}

/// Store a built snapshot.
fn insert(conn: &Connection, new: NewSnapshot) -> Result<Snapshot> {
    let created_at = chrono::Utc::now().timestamp();
    conn.execute(
        "INSERT INTO snapshots
            (thread_id, project_path, source_kind, label, body, token_estimate, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            new.thread_id,
            new.project_path,
            new.source_kind,
            new.label,
            new.body,
            new.token_estimate,
            created_at,
        ],
    )?;
    Ok(Snapshot {
        id: conn.last_insert_rowid(),
        thread_id: Some(new.thread_id),
        project_path: new.project_path,
        source_kind: Some(new.source_kind),
        label: new.label,
        token_estimate: new.token_estimate,
        created_at,
    })
}

/// Label prefix of hook-created snapshots.
pub const AUTO_LABEL_PREFIX: &str = "auto · ";

/// Create a rolling AUTO snapshot (from a hook such as PreCompact). Each event keeps one
/// slot per thread: the new snapshot replaces only the previous auto snapshot with the SAME
/// label, so one event can't wipe another's checkpoint, and snapshots the user named are
/// never touched. Insert and cleanup run in one IMMEDIATE transaction, inserting first, so
/// a failed snapshot leaves the previous one in place. The thread is packed BEFORE that
/// transaction: the hook fires on large live sessions, and holding the write lock through
/// the pack would stall the app's indexer, which is busy writing that same session.
pub fn create_rolling_auto(conn: &Connection, thread_id: i64, event: &str) -> Result<Snapshot> {
    let label = format!("{AUTO_LABEL_PREFIX}{event}");
    let new = build(conn, thread_id, Some(&label))?;
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let snap = insert(&tx, new)?;
    // Also drop this thread's legacy SubagentStop checkpoint: that hook is gone, so
    // nothing would ever replace it.
    tx.execute(
        "DELETE FROM snapshots WHERE thread_id = ?1 AND id != ?3
            AND (label = ?2 OR label = ?4)",
        rusqlite::params![thread_id, label, snap.id, LEGACY_SUBAGENT_LABEL],
    )?;
    tx.commit()?;
    Ok(snap)
}

/// Label of the auto snapshots the removed SubagentStop hook used to write.
const LEGACY_SUBAGENT_LABEL: &str = "auto · SubagentStop";

/// Delete the legacy SubagentStop auto snapshots that a later auto snapshot of the same
/// thread supersedes (each was a full re-pack of the parent thread). One with nothing newer
/// is kept: older builds kept a single auto slot per thread, so a SubagentStop after a
/// PreCompact replaced it, and that row is now the thread's only checkpoint. The next
/// PreCompact replaces it (`create_rolling_auto`). Safe to run at startup; returns the
/// number removed.
pub fn purge_legacy_auto_snapshots(conn: &Connection) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM snapshots WHERE label = ?1 AND EXISTS (
            SELECT 1 FROM snapshots n
            WHERE n.thread_id = snapshots.thread_id AND n.id > snapshots.id
              AND n.label LIKE ?2 AND n.label != ?1)",
        rusqlite::params![LEGACY_SUBAGENT_LABEL, format!("{AUTO_LABEL_PREFIX}%")],
    )?)
}

/// List snapshots newest-first, optionally scoped to a project-path substring.
pub fn list(conn: &Connection, project: Option<&str>, limit: usize) -> Result<Vec<Snapshot>> {
    let mut sql = String::from(
        "SELECT id, thread_id, project_path, source_kind, label, token_estimate, created_at
         FROM snapshots",
    );
    let mut args: Vec<Box<dyn ToSql>> = Vec::new();
    if let Some(p) = project.filter(|p| !p.is_empty()) {
        args.push(Box::new(format!("%{p}%")));
        sql.push_str(&format!(" WHERE project_path LIKE ?{}", args.len()));
    }
    args.push(Box::new(limit as i64));
    sql.push_str(&format!(" ORDER BY created_at DESC LIMIT ?{}", args.len()));

    let arg_refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(arg_refs.as_slice(), |r| {
        Ok(Snapshot {
            id: r.get(0)?,
            thread_id: r.get(1)?,
            project_path: r.get(2)?,
            source_kind: r.get(3)?,
            label: r.get(4)?,
            token_estimate: r.get(5)?,
            created_at: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Load a snapshot (metadata + body) by id.
pub fn load(conn: &Connection, id: i64) -> Result<Option<SnapshotDetail>> {
    Ok(conn
        .query_row(
            "SELECT id, thread_id, project_path, source_kind, label, body, token_estimate, created_at
             FROM snapshots WHERE id = ?1",
            [id],
            |r| {
                Ok(SnapshotDetail {
                    meta: Snapshot {
                        id: r.get(0)?,
                        thread_id: r.get(1)?,
                        project_path: r.get(2)?,
                        source_kind: r.get(3)?,
                        label: r.get(4)?,
                        token_estimate: r.get(6)?,
                        created_at: r.get(7)?,
                    },
                    body: r.get(5)?,
                })
            },
        )
        .optional()?)
}

/// Delete a snapshot; returns whether a row was removed.
pub fn delete(conn: &Connection, id: i64) -> Result<bool> {
    Ok(conn.execute("DELETE FROM snapshots WHERE id = ?1", [id])? > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::{source_id, upsert_thread, ParsedMessage, ParsedThread};
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    fn temp_db() -> Connection {
        let p = std::env::temp_dir().join(format!(
            "calli_snap_{}_{}.db",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        crate::db::open(&p).unwrap()
    }

    fn msg(role: &str, text: &str, ts: i64) -> ParsedMessage {
        ParsedMessage {
            role: role.into(),
            text: text.into(),
            tool_name: None,
            ts: Some(ts),
        }
    }

    fn seed_thread(conn: &mut Connection) -> i64 {
        let sid = source_id(conn, "claude_code").unwrap();
        let t = ParsedThread {
            external_id: "t1".into(),
            title: Some("auth refactor".into()),
            project_path: Some("/proj/app".into()),
            messages: vec![
                msg("user", "how should we structure auth", 100),
                msg("assistant", "use a token refresh on 401", 150),
            ],
            ..Default::default()
        };
        upsert_thread(conn, sid, &t).unwrap();
        conn.query_row("SELECT id FROM threads WHERE external_id = 't1'", [], |r| {
            r.get(0)
        })
        .unwrap()
    }

    #[test]
    fn create_load_list_roundtrip() {
        let mut conn = temp_db();
        let tid = seed_thread(&mut conn);

        let snap = create(&conn, tid, None).unwrap();
        assert_eq!(snap.thread_id, Some(tid));
        assert_eq!(snap.label, "auth refactor"); // defaults to thread title
        assert_eq!(snap.source_kind.as_deref(), Some("claude_code"));
        assert!(snap.token_estimate > 0);

        let loaded = load(&conn, snap.id).unwrap().expect("snapshot exists");
        assert!(
            loaded.body.contains("token refresh"),
            "body should contain the packed transcript"
        );

        let listed = list(&conn, Some("/proj/app"), 10).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, snap.id);
        // A non-matching project filter returns nothing.
        assert!(list(&conn, Some("/other"), 10).unwrap().is_empty());
    }

    #[test]
    fn explicit_label_and_delete() {
        let mut conn = temp_db();
        let tid = seed_thread(&mut conn);
        let snap = create(&conn, tid, Some("before the rewrite")).unwrap();
        assert_eq!(snap.label, "before the rewrite");
        assert!(delete(&conn, snap.id).unwrap());
        assert!(load(&conn, snap.id).unwrap().is_none());
        assert!(!delete(&conn, snap.id).unwrap()); // already gone
    }

    #[test]
    fn create_unknown_thread_errors() {
        let conn = temp_db();
        assert!(create(&conn, 9999, None).is_err());
    }

    #[test]
    fn rolling_auto_keeps_one_per_event_and_spares_manual() {
        let mut conn = temp_db();
        let tid = seed_thread(&mut conn);
        let manual = create(&conn, tid, Some("manual checkpoint")).unwrap();
        create_rolling_auto(&conn, tid, "PreCompact").unwrap();
        let latest = create_rolling_auto(&conn, tid, "PreCompact").unwrap();
        create_rolling_auto(&conn, tid, "SessionEnd").unwrap();

        let snaps = list(&conn, None, 100).unwrap();
        // The manual snapshot plus one auto per event (the most recent of each).
        assert_eq!(snaps.len(), 3);
        assert!(
            snaps.iter().any(|s| s.id == manual.id),
            "manual snapshot is preserved"
        );
        let pre: Vec<_> = snaps
            .iter()
            .filter(|s| s.label == "auto · PreCompact")
            .collect();
        assert_eq!(pre.len(), 1, "only the latest PreCompact snapshot is kept");
        assert_eq!(pre[0].id, latest.id);
        assert!(
            snaps.iter().any(|s| s.label == "auto · SessionEnd"),
            "a different event does not replace the PreCompact checkpoint"
        );

        // Legacy SubagentStop checkpoints are cleaned up.
        create(&conn, tid, Some(LEGACY_SUBAGENT_LABEL)).unwrap();
        create_rolling_auto(&conn, tid, "PreCompact").unwrap();
        let labels: Vec<String> = list(&conn, None, 100)
            .unwrap()
            .into_iter()
            .map(|s| s.label)
            .collect();
        assert!(!labels.iter().any(|l| l == LEGACY_SUBAGENT_LABEL));
    }

    #[test]
    fn startup_purge_keeps_a_threads_only_auto_checkpoint() {
        let mut conn = temp_db();
        let tid = seed_thread(&mut conn);
        // What older builds left: the SubagentStop snapshot replaced the PreCompact one, so
        // it is the thread's only auto checkpoint.
        let only = create(&conn, tid, Some(LEGACY_SUBAGENT_LABEL)).unwrap();
        create(&conn, tid, Some("manual")).unwrap();
        assert_eq!(purge_legacy_auto_snapshots(&conn).unwrap(), 0);
        assert!(load(&conn, only.id).unwrap().is_some());
        // A later auto snapshot of the thread supersedes it.
        create(&conn, tid, Some("auto · PreCompact")).unwrap();
        assert_eq!(purge_legacy_auto_snapshots(&conn).unwrap(), 1);
        assert!(load(&conn, only.id).unwrap().is_none());
    }

    #[test]
    fn carry_block_is_compact_and_redacted() {
        let mut conn = temp_db();
        let tid = seed_thread(&mut conn);
        for i in 0..30 {
            knowledge::record_fact(
                &conn,
                "/proj/app",
                if i % 2 == 0 { "decision" } else { "gotcha" },
                &format!("fact number {i} {}", "context ".repeat(30)),
                None,
                100 + i,
            )
            .unwrap();
        }
        knowledge::record_fact(
            &conn,
            "/proj/app",
            "gotcha",
            "staging key is sk-ant-api03-AbCdEfGhIjKlMnOpQrStUv0123456789",
            None,
            999,
        )
        .unwrap();
        let snap = create(&conn, tid, None).unwrap();
        let body = load(&conn, snap.id).unwrap().unwrap().body;
        let (carry, _) = body.split_once("\n\n---\n\n").expect("carry block");
        assert!(carry.chars().count() <= export::MemoryBudget::COMPACT.max_chars);
        assert!(carry.contains("Watch out for"));
        assert!(!body.contains("sk-ant-api03"));
    }
}
