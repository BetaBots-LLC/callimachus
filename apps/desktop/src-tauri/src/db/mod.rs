//! SQLite connection + schema bootstrap. A single connection guarded by a Mutex
//! lives in Tauri managed state (rusqlite::Connection is Send but not Sync).

pub mod migrations;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once};

/// Managed Tauri state wrapper around the app's SQLite connection.
pub struct Db(pub Mutex<Connection>);

/// Register the sqlite-vec (vec0) extension exactly once, for all connections
/// opened afterwards. Must run before the first `Connection::open`.
// The transmute is the documented sqlite-vec registration idiom (fn ptr -> the
// extension entry-point type); the source/target types are unambiguous here.
#[allow(clippy::missing_transmute_annotations)]
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
    // Create the parent dir so a first run (e.g. the MCP server in a fresh
    // container, or CALLIMACHUS_DB pointing somewhere new) can create the file —
    // rusqlite makes the file but not the directory.
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating db directory {}", parent.display()))?;
    }
    let mut conn = Connection::open(path)
        .with_context(|| format!("opening database at {}", path.display()))?;

    // WAL = concurrent readers while we write; required so indexing doesn't block reads.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // Wait (don't fail) for a write lock — the app, `cal`, and the MCP server open
    // the same file from separate processes, AND in-process the indexer + file watcher
    // each hold their own write connection. Combined with BEGIN IMMEDIATE on every write
    // txn (so the busy handler actually engages instead of an instant upgrade-conflict),
    // a generous timeout lets concurrent writers queue rather than error "database locked".
    conn.busy_timeout(std::time::Duration::from_secs(15))?;
    // Performance pragmas (defaults are tiny). cache_size is the biggest single win:
    // -65536 = 64 MiB page cache. mmap maps the file for read; temp_store keeps
    // sorts/CTEs in RAM; the WAL caps keep the -wal file from growing unbounded and
    // checkpoints small. See db-perf audit.
    conn.pragma_update(None, "cache_size", -65536_i64)?; // 64 MiB
    conn.pragma_update(None, "mmap_size", 268_435_456_i64)?; // 256 MiB
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "wal_autocheckpoint", 2000_i64)?; // ~8 MiB
    conn.pragma_update(None, "journal_size_limit", 67_108_864_i64)?; // cap -wal at 64 MiB

    migrations::MIGRATIONS
        .to_latest(&mut conn)
        .context("running migrations")?;

    Ok(conn)
}

/// Open the index **read-only**, for the sidecar processes (MCP server, `cal`).
///
/// The desktop app is the single writer; sidecars only query. In WAL mode a
/// read-only connection never blocks (or is blocked by) the app's writes, which is
/// what keeps "database is locked" from ever happening across processes. Does NOT
/// migrate or change journal mode (read-only can't, and the app already owns that).
pub fn open_readonly(path: &Path) -> Result<Connection> {
    register_vec(); // vec0 must be loaded to read the KNN (vec_chunks) index
    if !path.exists() {
        anyhow::bail!(
            "no index at {} — open the Callimachus app once to build it (or set CALLIMACHUS_DB)",
            path.display()
        );
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("opening {} read-only", path.display()))?;
    // Still honor a busy wait for the rare moment a WAL checkpoint is in flight.
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    // Reader pragmas: readers benefit most from mmap; smaller cache since these are
    // short-lived (sidecars) or pooled (the in-process read pool). query_only is a
    // belt-and-suspenders guard. NO autocheckpoint — readers must not checkpoint.
    conn.pragma_update(None, "mmap_size", 268_435_456_i64)?; // 256 MiB
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "cache_size", -16384_i64)?; // 16 MiB
    conn.pragma_update(None, "query_only", "ON")?;
    Ok(conn)
}

// ---------------------------------------------------------------------------
// Read pool — concurrent read-only connections (WAL allows unlimited readers), so
// UI read commands run in parallel instead of serializing behind the writer mutex.
// ---------------------------------------------------------------------------

/// r2d2 manager that opens read-only connections to the index (reader pragmas applied).
pub struct ReadManager {
    path: PathBuf,
}

impl r2d2::ManageConnection for ReadManager {
    type Connection = Connection;
    type Error = rusqlite::Error;

    fn connect(&self) -> std::result::Result<Connection, rusqlite::Error> {
        let conn = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "mmap_size", 268_435_456_i64)?;
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        conn.pragma_update(None, "cache_size", -16384_i64)?;
        conn.pragma_update(None, "query_only", "ON")?;
        Ok(conn)
    }

    fn is_valid(&self, conn: &mut Connection) -> std::result::Result<(), rusqlite::Error> {
        conn.execute_batch("SELECT 1;")
    }

    fn has_broken(&self, _conn: &mut Connection) -> bool {
        false
    }
}

/// Managed Tauri state: the pool of read-only connections.
pub struct ReadPool(pub r2d2::Pool<ReadManager>);
pub type ReadConn = r2d2::PooledConnection<ReadManager>;

/// Build the read pool. Call AFTER the writer has opened + migrated the DB (read-only
/// connections cannot migrate). `size` ~ cpu cores.
pub fn read_pool(path: &Path, size: u32) -> Result<ReadPool> {
    register_vec(); // vec0 must be loaded for the KNN (vec_chunks/vec_facts) reads
    if !path.exists() {
        anyhow::bail!(
            "no index at {} — open the app to build it first",
            path.display()
        );
    }
    let pool = r2d2::Pool::builder()
        .max_size(size.max(2))
        .build(ReadManager {
            path: path.to_path_buf(),
        })
        .context("building read pool")?;
    Ok(ReadPool(pool))
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
        let path =
            std::env::temp_dir().join(format!("callimachus_test_{}_{n}.db", std::process::id()));
        open(&path).expect("open + migrate")
    }

    #[test]
    fn read_pool_opens_queries_and_has_vec0() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(9000);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("calli_pool_{}_{n}.db", std::process::id()));
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(path.with_extension(ext));
        }
        let _writer = open(&path).unwrap(); // create + migrate first (readers can't)
        let pool = read_pool(&path, 3).unwrap();
        let conn = pool.0.get().unwrap();
        let sources: i64 = conn
            .query_row("SELECT COUNT(*) FROM sources", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sources, 12, "pooled read sees the migrated schema");
        // vec0 must be loaded on pooled connections for KNN reads.
        let ver: String = conn
            .query_row("SELECT vec_version()", [], |r| r.get(0))
            .unwrap();
        assert!(ver.starts_with('v'), "vec0 on pooled conn: {ver}");
        // A second checkout works (pool serves concurrent readers).
        let c2 = pool.0.get().unwrap();
        let _: i64 = c2
            .query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0))
            .unwrap();
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
        let ver: String = conn
            .query_row("SELECT vec_version()", [], |r| r.get(0))
            .unwrap();
        assert!(ver.starts_with('v'), "vec_version: {ver}");

        let to_blob =
            |v: &[f32]| -> Vec<u8> { v.iter().flat_map(|f| f.to_le_bytes()).collect::<Vec<u8>>() };
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
        conn.execute(
            "DELETE FROM messages WHERE id = (SELECT MAX(id) FROM messages)",
            [],
        )
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
