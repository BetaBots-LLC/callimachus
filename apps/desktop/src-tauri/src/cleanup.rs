//! Storage cleanup: list threads oldest-first with their on-disk footprint, delete
//! selected ones, and reclaim freed pages. Deleting a thread cascades to its
//! messages (FK `ON DELETE CASCADE`), which fires the triggers that clear the FTS
//! index and vector chunks — so one `DELETE FROM threads` removes everything.

use anyhow::Result;
use rusqlite::{Connection, ToSql};
use serde::Serialize;

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
        sql.push_str(&format!(" AND (t.updated_at IS NULL OR t.updated_at <= ?{})", args.len()));
    }
    if !sources.is_empty() {
        let ph: Vec<String> = (0..sources.len()).map(|i| format!("?{}", args.len() + 1 + i)).collect();
        sql.push_str(&format!(" AND s.kind IN ({})", ph.join(", ")));
        for s in sources {
            args.push(Box::new(s.clone()));
        }
    }
    sql.push_str(&format!(" ORDER BY t.updated_at ASC LIMIT ?{}", args.len() + 1));
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
pub fn delete_threads(conn: &mut Connection, ids: &[i64]) -> Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let tx = conn.transaction()?;
    let mut n = 0;
    {
        let mut stmt = tx.prepare("DELETE FROM threads WHERE id = ?1")?;
        for id in ids {
            n += stmt.execute([id])?;
        }
    }
    tx.commit()?;
    Ok(n)
}

/// Reclaim disk space freed by deletes (rewrites the DB file; also checkpoints WAL).
pub fn vacuum(conn: &Connection) -> Result<()> {
    conn.execute_batch("VACUUM;")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::{source_id, upsert_thread, ParsedMessage, ParsedThread};

    fn temp_db() -> Connection {
        let p = std::env::temp_dir().join(format!("calli_cleanup_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        crate::db::open(&p).unwrap()
    }

    fn thread(ext: &str, updated: i64, msgs: usize) -> ParsedThread {
        ParsedThread {
            external_id: ext.into(),
            title: Some(ext.into()),
            updated_at: Some(updated),
            created_at: Some(updated),
            messages: (0..msgs)
                .map(|i| ParsedMessage {
                    role: if i % 2 == 0 { "user".into() } else { "assistant".into() },
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
        let left: i64 = conn.query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0)).unwrap();
        assert_eq!(left, 1);
        let msgs: i64 = conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0)).unwrap();
        assert_eq!(msgs, 2, "deleted thread's messages cascaded away");
    }
}
