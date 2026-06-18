//! SQLite connection + schema bootstrap. A single connection guarded by a Mutex
//! lives in Tauri managed state (rusqlite::Connection is Send but not Sync).

pub mod migrations;

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once};

/// Managed Tauri state wrapper around the app's SQLite connection.
pub struct Db(pub Mutex<Connection>);

/// Register the sqlite-vec (vec0) extension exactly once, for all connections
/// opened afterwards. Must run before the first `Connection::open`.
fn register_vec() {
    static INIT: Once = Once::new();
    INIT.call_once(|| unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    });
}

/// The desktop app's index.db location, shared by the sidecar binaries (MCP
/// server, `cal` CLI) so they all read the same store. `CALLIMACHUS_DB` overrides;
/// otherwise the Tauri `app_data_dir` for bundle id `dev.shaller.callimachus`.
pub fn default_index_path() -> PathBuf {
    if let Ok(p) = std::env::var("CALLIMACHUS_DB") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home)
        .join("Library/Application Support/dev.shaller.callimachus")
        .join("index.db")
}

/// Open (or create) the database at `path`, apply pragmas, and migrate to latest.
pub fn open(path: &Path) -> Result<Connection> {
    register_vec();
    let mut conn = Connection::open(path)
        .with_context(|| format!("opening database at {}", path.display()))?;

    // WAL = concurrent readers while we write; required so indexing doesn't block reads.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;

    migrations::MIGRATIONS
        .to_latest(&mut conn)
        .context("running migrations")?;

    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> Connection {
        // Unique temp path per test run; an in-memory DB would also work but a file
        // exercises the real open() path including WAL pragmas. A process-wide
        // counter avoids collisions when tests run on the same nanosecond.
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("callimachus_test_{}_{n}.db", std::process::id()));
        open(&path).expect("open + migrate")
    }

    #[test]
    fn migrations_seed_sources() {
        let conn = temp_db();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM sources", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 12, "twelve source kinds seeded");
    }

    #[test]
    fn vec0_knn_works() {
        let conn = temp_db();
        // vec0 extension registered + migration created the virtual table.
        let ver: String = conn.query_row("SELECT vec_version()", [], |r| r.get(0)).unwrap();
        assert!(ver.starts_with('v'), "vec_version: {ver}");

        let to_blob = |v: &[f32]| -> Vec<u8> {
            v.iter().flat_map(|f| f.to_le_bytes()).collect::<Vec<u8>>()
        };
        let mut a = vec![0.0_f32; 384];
        a[0] = 1.0;
        let mut b = vec![0.0_f32; 384];
        b[1] = 1.0;
        conn.execute(
            "INSERT INTO vec_chunks (message_id, embedding) VALUES (10, ?1)",
            [to_blob(&a)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO vec_chunks (message_id, embedding) VALUES (20, ?1)",
            [to_blob(&b)],
        )
        .unwrap();

        // Query closest to `a` -> message 10 should win.
        let mid: i64 = conn
            .query_row(
                "SELECT message_id FROM vec_chunks WHERE embedding MATCH ?1 AND k = 1",
                [to_blob(&a)],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(mid, 10);
    }

    #[test]
    fn vec0_cte_join_query() {
        let conn = temp_db();
        let to_blob =
            |v: &[f32]| -> Vec<u8> { v.iter().flat_map(|f| f.to_le_bytes()).collect::<Vec<u8>>() };
        conn.execute(
            "INSERT INTO threads (source_id, external_id, title) VALUES (1, 'e', 't')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO messages (thread_id, seq, role, text) VALUES (?1, 0, 'user', 'hi')",
            [tid],
        )
        .unwrap();
        let mid = conn.last_insert_rowid();
        let mut a = vec![0.0_f32; 384];
        a[0] = 1.0;
        conn.execute(
            "INSERT INTO vec_chunks (message_id, embedding) VALUES (?1, ?2)",
            rusqlite::params![mid, to_blob(&a)],
        )
        .unwrap();

        let sql = "WITH knn AS MATERIALIZED (
                SELECT message_id, distance FROM vec_chunks
                WHERE embedding MATCH ?1 AND k = 200 ORDER BY distance
             )
             SELECT knn.message_id, MIN(knn.distance) AS d
             FROM knn
             JOIN messages m ON m.id = knn.message_id
             JOIN threads t ON t.id = m.thread_id
             JOIN sources s ON s.id = t.source_id
             WHERE t.is_subagent = 0
             GROUP BY knn.message_id ORDER BY d LIMIT 10";
        let r = conn.query_row(sql, [to_blob(&a)], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
        });
        if let Err(e) = &r {
            panic!("vec0 cte/join query failed: {e}");
        }
        assert_eq!(r.unwrap().0, mid);
    }

    #[test]
    fn fts_roundtrip_through_triggers() {
        let conn = temp_db();
        conn.execute(
            "INSERT INTO threads (source_id, external_id, title) VALUES (1, 'sess-1', 't')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO messages (thread_id, seq, role, text) VALUES (?1, 0, 'user', ?2)",
            rusqlite::params![tid, "indexing tauri threads with sqlite fts5"],
        )
        .unwrap();

        // The AFTER INSERT trigger should have populated the FTS index.
        let hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'sqlite'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 1, "FTS5 external-content trigger indexed the message");

        // And deletion should remove it from the index.
        conn.execute("DELETE FROM messages WHERE id = (SELECT MAX(id) FROM messages)", [])
            .unwrap();
        let hits_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'sqlite'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits_after, 0, "delete trigger cleaned the FTS index");
    }
}
