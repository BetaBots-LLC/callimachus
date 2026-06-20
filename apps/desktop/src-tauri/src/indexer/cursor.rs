//! Cursor indexer. Chat lives in the GLOBAL DB
//! `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb`
//! (table `cursorDiskKV`): `composerData:<id>` rows are threads, and
//! `bubbleId:<composerId>:<bubbleId>` rows are messages (`type` 1=user, 2=assistant).
//!
//! The DB can be multiple GB and is open while Cursor runs, so we open it
//! read-only and pull only the fields we need with SQL `json_extract`, never the
//! full blobs. We upsert per-composer to keep memory bounded.

use super::{file_unchanged, source_id, upsert_thread, IndexReport, ParsedMessage, ParsedThread};
use anyhow::Result;
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};

pub const KIND: &str = "cursor";

/// Path to Cursor's global state DB (macOS).
pub fn global_db_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| {
        PathBuf::from(h).join("Library/Application Support/Cursor/User/globalStorage/state.vscdb")
    })
}

struct Composer {
    id: String,
    name: Option<String>,
    created_at_ms: Option<i64>,
}

/// Index all Cursor composer threads from the global DB.
pub fn scan(conn: &mut Connection) -> Result<IndexReport> {
    let Some(db) = global_db_path() else {
        return Ok(IndexReport::default());
    };
    scan_path(conn, &db)
}

/// Index Cursor threads from a specific state.vscdb. Skips entirely if the file is
/// unchanged since the last pass (the whole store is one file).
fn scan_path(conn: &mut Connection, db: &Path) -> Result<IndexReport> {
    let mut report = IndexReport::default();
    if !db.exists() {
        return Ok(report);
    }
    let sid = source_id(conn, KIND)?;

    if file_unchanged(conn, db, KIND)? {
        return Ok(report); // nothing changed
    }

    let ro = super::open_external_readonly(db)?;
    let composers = list_composers(&ro)?;

    // CAST each extracted field so rusqlite always sees a predictable column type
    // (Cursor stores some numbers as REAL, which would otherwise fail i64 decoding).
    // Use a key RANGE (>=/<), not LIKE: SQLite's LIKE is case-insensitive by default
    // and would NOT use the PRIMARY KEY index — a full 4.4GB scan per composer.
    let mut bubble_stmt = ro.prepare(
        "SELECT CAST(json_extract(value, '$.type') AS INTEGER),
                CAST(json_extract(value, '$.text') AS TEXT),
                CAST(json_extract(value, '$.timingInfo.clientStartTime') AS INTEGER)
         FROM cursorDiskKV
         WHERE key >= ?1 AND key < ?2
         ORDER BY 3",
    )?;

    for c in composers {
        let lo = format!("bubbleId:{}:", c.id);
        let hi = format!("bubbleId:{}:\u{10FFFF}", c.id);
        let mut messages: Vec<ParsedMessage> = Vec::new();
        let mut first_user: Option<String> = None;
        let mut max_ts: Option<i64> = None;

        let rows = bubble_stmt.query_map(params![lo, hi], |r| {
            Ok((
                r.get::<_, Option<i64>>(0)?,    // type
                r.get::<_, Option<String>>(1)?, // text
                r.get::<_, Option<i64>>(2)?,    // clientStartTime (ms)
            ))
        })?;

        for row in rows {
            let (kind, text, ts_ms) = row?;
            let Some(text) = text.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) else {
                continue;
            };
            let role = if kind == Some(2) { "assistant" } else { "user" };
            let ts = ts_ms.map(|ms| ms / 1000);
            if role == "user" && first_user.is_none() {
                first_user = Some(text.clone());
            }
            if let Some(ts) = ts {
                max_ts = Some(max_ts.map_or(ts, |m| m.max(ts)));
            }
            messages.push(ParsedMessage {
                role: role.to_string(),
                text,
                tool_name: None,
                ts,
            });
        }

        if messages.is_empty() {
            report.threads_skipped += 1;
            continue;
        }

        let created_at = c.created_at_ms.map(|ms| ms / 1000);
        let title = c
            .name
            .filter(|n| !n.trim().is_empty())
            .or_else(|| first_user.map(truncate_title));
        let thread = ParsedThread {
            external_id: c.id,
            title,
            project_path: None, // Cursor does not store a per-thread workspace; left null
            git_branch: None,
            created_at,
            updated_at: max_ts.or(created_at),
            is_subagent: false,
            messages,
        };
        let n = upsert_thread(conn, sid, &thread)?;
        report.threads_indexed += 1;
        report.messages_indexed += n;
    }

    Ok(report)
}

fn list_composers(ro: &Connection) -> Result<Vec<Composer>> {
    // substr(key, 14) strips the "composerData:" prefix (13 chars).
    // Range scan (not LIKE) so the PRIMARY KEY index is used. ';' (0x3b) is the
    // byte just after ':' (0x3a), bounding the 'composerData:' prefix.
    let mut stmt = ro.prepare(
        "SELECT substr(key, 14),
                CAST(json_extract(value, '$.name') AS TEXT),
                CAST(json_extract(value, '$.createdAt') AS INTEGER)
         FROM cursorDiskKV
         WHERE key >= 'composerData:' AND key < 'composerData;'",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Composer {
            id: r.get(0)?,
            name: r.get::<_, Option<String>>(1)?,
            created_at_ms: r.get::<_, Option<i64>>(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn truncate_title(s: String) -> String {
    let s = s.trim();
    if s.chars().count() > 80 {
        format!("{}…", s.chars().take(80).collect::<String>())
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a tiny Cursor-shaped DB and verify we extract a thread + messages.
    #[test]
    fn extracts_from_cursor_shaped_db() {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "callimachus_cursorsrc_{}.vscdb",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        {
            let src = Connection::open(&path).unwrap();
            src.execute_batch(
                "CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value BLOB);
                 INSERT INTO cursorDiskKV VALUES
                   ('composerData:abc', json('{\"composerId\":\"abc\",\"name\":\"Venv help\",\"createdAt\":1733732099602}')),
                   ('bubbleId:abc:b1', json('{\"type\":1,\"text\":\"how do i activate venv\",\"timingInfo\":{\"clientStartTime\":1733732100000}}')),
                   ('bubbleId:abc:b2', json('{\"type\":2,\"text\":\"run source .venv/bin/activate\",\"timingInfo\":{\"clientStartTime\":1733732105000}}'));",
            )
            .unwrap();
        }

        // list_composers
        let ro = super::super::open_external_readonly(&path).unwrap();
        let composers = list_composers(&ro).unwrap();
        assert_eq!(composers.len(), 1);
        assert_eq!(composers[0].id, "abc");
        assert_eq!(composers[0].name.as_deref(), Some("Venv help"));
        drop(ro);

        // Full scan_path: thread + ordered messages land in the canonical store.
        let mut db = std::env::temp_dir();
        db.push(format!("callimachus_cursordst_{}.db", std::process::id()));
        let mut conn = crate::db::open(&db).unwrap();
        let report = scan_path(&mut conn, &path).unwrap();
        assert_eq!(report.threads_indexed, 1);
        assert_eq!(report.messages_indexed, 2);

        let detail = crate::search::thread_detail(
            &conn,
            conn.query_row("SELECT id FROM threads", [], |r| r.get(0))
                .unwrap(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(detail.title.as_deref(), Some("Venv help"));
        assert_eq!(detail.messages.len(), 2);
        assert_eq!(detail.messages[0].role, "user"); // ordered by clientStartTime
        assert_eq!(detail.messages[1].role, "assistant");
    }

    /// Real-data smoke test. Run with `cargo test -- --ignored real_cursor_index --nocapture`.
    #[test]
    #[ignore]
    fn real_cursor_index() {
        let mut p = std::env::temp_dir();
        p.push(format!("callimachus_cursor_real_{}.db", std::process::id()));
        let mut conn = crate::db::open(&p).unwrap();
        let report = scan(&mut conn).unwrap();
        eprintln!("{report:?}");
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        eprintln!("cursor messages indexed: {n}");
    }
}
