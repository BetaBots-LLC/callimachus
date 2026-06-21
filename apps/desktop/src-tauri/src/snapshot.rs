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

/// Snapshot an indexed thread. `label` defaults to the thread title (then a generic fallback).
pub fn create(conn: &Connection, thread_id: i64, label: Option<&str>) -> Result<Snapshot> {
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
    let carry = match project_key.as_deref() {
        Some(pk) if !pk.is_empty() => {
            let mem = knowledge::get_project_memory(conn, pk, 40)?;
            export::agent_memory_md(pk, &mem, None)
        }
        _ => String::new(),
    };

    let body = assemble(&carry, &transcript);
    let label = label
        .map(str::to_string)
        .or_else(|| detail.title.clone())
        .unwrap_or_else(|| format!("snapshot of thread {thread_id}"));
    let token_estimate = (body.len() / 4) as i64;
    let created_at = chrono::Utc::now().timestamp();

    conn.execute(
        "INSERT INTO snapshots
            (thread_id, project_path, source_kind, label, body, token_estimate, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            thread_id,
            detail.project_path,
            detail.source,
            label,
            body,
            token_estimate,
            created_at,
        ],
    )?;

    Ok(Snapshot {
        id: conn.last_insert_rowid(),
        thread_id: Some(thread_id),
        project_path: detail.project_path,
        source_kind: Some(detail.source),
        label,
        token_estimate,
        created_at,
    })
}

/// Create a rolling AUTO snapshot (from a PreCompact / SubagentStop hook). Keeps only the
/// latest auto-snapshot per thread — prior auto ones are dropped first — so repeated
/// compactions or subagent stops don't flood the list. Manual (named) snapshots are untouched.
pub fn create_rolling_auto(conn: &Connection, thread_id: i64, event: &str) -> Result<Snapshot> {
    conn.execute(
        "DELETE FROM snapshots WHERE thread_id = ?1 AND label LIKE 'auto · %'",
        [thread_id],
    )?;
    create(conn, thread_id, Some(&format!("auto · {event}")))
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
    fn rolling_auto_keeps_one_per_thread_and_spares_manual() {
        let mut conn = temp_db();
        let tid = seed_thread(&mut conn);
        let manual = create(&conn, tid, Some("manual checkpoint")).unwrap();
        create_rolling_auto(&conn, tid, "PreCompact").unwrap();
        create_rolling_auto(&conn, tid, "SubagentStop").unwrap();

        let snaps = list(&conn, None, 100).unwrap();
        // The manual snapshot plus exactly one auto (the most recent).
        assert_eq!(snaps.len(), 2);
        assert!(
            snaps.iter().any(|s| s.id == manual.id),
            "manual snapshot is preserved"
        );
        let autos: Vec<_> = snaps
            .iter()
            .filter(|s| s.label.starts_with("auto · "))
            .collect();
        assert_eq!(autos.len(), 1, "only the latest auto snapshot is kept");
        assert_eq!(autos[0].label, "auto · SubagentStop");
    }
}
