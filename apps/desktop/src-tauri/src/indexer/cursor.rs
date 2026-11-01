//! Cursor indexer. Chat lives in the GLOBAL DB
//! `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb`
//! (table `cursorDiskKV`): `composerData:<id>` rows are threads, and
//! `bubbleId:<composerId>:<bubbleId>` rows are messages (`type` 1=user, 2=assistant).
//!
//! The DB can be multiple GB and is open while Cursor runs, so we open it
//! read-only and pull only the fields we need with SQL `json_extract`, never the
//! full blobs. We upsert per-composer to keep memory bounded.
//!
//! Change detection is two-level. The whole store is one file (plus its `-wal`, where
//! Cursor's new writes land until a checkpoint), so a pass starts only when either file
//! changed. Within a pass, each composer is fingerprinted from cheap metadata
//! (`lastUpdatedAt`, bubble count, and while it is recent the bubbles' byte size) and
//! only composers whose fingerprint moved get their bubbles extracted and re-upserted.
//! Without this, every checkpoint re-extracted every bubble (gigabytes of JSON) and
//! rewrote every thread.

use super::{
    file_change_state, set_file_state, source_id, upsert_thread, IndexReport, ParsedMessage,
    ParsedThread,
};
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};

pub const KIND: &str = "cursor";

/// Path to Cursor's global state DB. Uses the platform per-user config dir
/// (macOS `~/Library/Application Support`, Windows `%APPDATA%`, Linux `~/.config`),
/// which is where VSCode-family editors keep `User/globalStorage`.
pub fn global_db_path() -> Option<PathBuf> {
    dirs::config_dir().map(|c| c.join("Cursor/User/globalStorage/state.vscdb"))
}

struct Composer {
    id: String,
    name: Option<String>,
    created_at_ms: Option<i64>,
    /// `composerData.lastUpdatedAt` (ms); absent on older Cursor builds.
    last_updated_ms: Option<i64>,
}

/// A composer whose `lastUpdatedAt` is older than this is treated as settled: its
/// fingerprint is checked from the timestamp + bubble count alone, skipping the bubble
/// size sum. Recent composers (an assistant reply may still be streaming into an existing
/// bubble without changing the count) always get the full check.
const SETTLED_AFTER_MS: i64 = 15 * 60 * 1000;

/// `<db>-wal`: where a WAL-mode SQLite DB keeps writes until the next checkpoint.
fn wal_path(db: &Path) -> PathBuf {
    let mut s = db.as_os_str().to_owned();
    s.push("-wal");
    PathBuf::from(s)
}

/// `index_state` key for one composer's fingerprint.
fn composer_key(db: &Path, composer_id: &str) -> String {
    format!("{}#composer:{composer_id}", db.to_string_lossy())
}

/// Stored fingerprint `"<lastUpdatedAt>:<bubbles>:<bytes>"` for a composer, if any.
fn stored_fingerprint(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT hash FROM index_state WHERE path = ?1", [key], |r| {
            r.get::<_, Option<String>>(0)
        })
        .optional()?
        .flatten())
}

fn store_fingerprint(conn: &Connection, key: &str, fingerprint: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO index_state (path, source_kind, mtime, size, hash, updated_at)
         VALUES (?1, ?2, 0, 0, ?3, ?4)
         ON CONFLICT (path) DO UPDATE SET hash = excluded.hash, updated_at = excluded.updated_at",
        params![key, KIND, fingerprint, chrono::Utc::now().timestamp()],
    )?;
    Ok(())
}

/// Index all Cursor composer threads from the global DB.
pub fn scan(conn: &mut Connection, tick: &mut dyn FnMut()) -> Result<IndexReport> {
    let Some(db) = global_db_path() else {
        return Ok(IndexReport::default());
    };
    scan_path(conn, &db, tick)
}

/// Index Cursor threads from a specific state.vscdb. Skips entirely if the file is
/// unchanged since the last pass (the whole store is one file).
fn scan_path(conn: &mut Connection, db: &Path, tick: &mut dyn FnMut()) -> Result<IndexReport> {
    let mut report = IndexReport::default();
    if !db.exists() {
        return Ok(report);
    }
    let sid = source_id(conn, KIND)?;

    // Change key covers the main file AND its WAL: new chats land in the -wal first, and
    // the main file only changes at checkpoint time, so watching it alone lagged.
    let wal = wal_path(db);
    let (main_unchanged, mtime, size) = file_change_state(conn, db)?;
    let wal_state = if wal.exists() {
        Some(file_change_state(conn, &wal)?)
    } else {
        None
    };
    if main_unchanged
        && wal_state
            .as_ref()
            .is_none_or(|(unchanged, _, _)| *unchanged)
    {
        return Ok(report); // nothing changed
    }

    let ro = super::open_external_readonly(db)?;
    let composers = list_composers(&ro)?;
    let now_ms = chrono::Utc::now().timestamp_millis();

    // Index-only count of a composer's bubbles (key range on the key index), plus, when
    // asked, their total byte size: octet_length() reads only each record's header.
    let mut count_stmt =
        ro.prepare("SELECT COUNT(*) FROM cursorDiskKV WHERE key >= ?1 AND key < ?2")?;
    let mut bytes_stmt = ro.prepare(
        "SELECT COALESCE(SUM(octet_length(value)), 0) FROM cursorDiskKV
         WHERE key >= ?1 AND key < ?2",
    )?;

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
        tick();
        let lo = format!("bubbleId:{}:", c.id);
        let hi = format!("bubbleId:{}:\u{10FFFF}", c.id);

        // Deleted by the user: never re-import.
        if crate::cleanup::is_tombstoned(conn, sid, &c.id)? {
            report.threads_skipped += 1;
            continue;
        }

        // Skip composers whose fingerprint hasn't moved since they were last indexed.
        let key = composer_key(db, &c.id);
        let stored = stored_fingerprint(conn, &key)?;
        let updated = c.last_updated_ms.unwrap_or(-1);
        let bubbles: i64 = count_stmt.query_row(params![lo, hi], |r| r.get(0))?;
        let settled = c
            .last_updated_ms
            .is_some_and(|u| now_ms - u > SETTLED_AFTER_MS);
        let cheap_prefix = format!("{updated}:{bubbles}:");
        if settled
            && stored
                .as_deref()
                .is_some_and(|f| f.starts_with(&cheap_prefix))
        {
            report.threads_skipped += 1;
            continue;
        }
        let bytes: i64 = bytes_stmt.query_row(params![lo, hi], |r| r.get(0))?;
        let fingerprint = format!("{cheap_prefix}{bytes}");
        if stored.as_deref() == Some(fingerprint.as_str()) {
            report.threads_skipped += 1;
            continue;
        }

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
            // Nothing to index yet, but remember the fingerprint so the next pass doesn't
            // re-extract it again.
            store_fingerprint(conn, &key, &fingerprint)?;
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
            usage: Vec::new(),
            messages,
        };
        let n = upsert_thread(conn, sid, &thread)?;
        // Only after a successful upsert, so a failed write is retried next pass.
        store_fingerprint(conn, &key, &fingerprint)?;
        report.threads_indexed += 1;
        report.messages_indexed += n;
    }

    // Record the DB file as indexed ONLY after every composer upserted without error: a
    // mid-pass failure returns above via `?`, leaving the file "changed" so the watcher's
    // retry re-indexes it instead of skipping the dropped threads.
    set_file_state(conn, &db.to_string_lossy(), KIND, mtime, size)?;
    if let Some((_, wal_mtime, wal_size)) = wal_state {
        set_file_state(conn, &wal.to_string_lossy(), KIND, wal_mtime, wal_size)?;
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
                CAST(json_extract(value, '$.createdAt') AS INTEGER),
                CAST(json_extract(value, '$.lastUpdatedAt') AS INTEGER)
         FROM cursorDiskKV
         WHERE key >= 'composerData:' AND key < 'composerData;'",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Composer {
            id: r.get(0)?,
            name: r.get::<_, Option<String>>(1)?,
            created_at_ms: r.get::<_, Option<i64>>(2)?,
            last_updated_ms: r.get::<_, Option<i64>>(3)?,
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
        let report = scan_path(&mut conn, &path, &mut || {}).unwrap();
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

    /// A change to the store re-extracts only the composers that changed, catches a bubble
    /// growing in place (streaming) and never re-imports a deleted composer.
    #[test]
    fn rescans_touch_only_changed_composers() {
        let dir = std::env::temp_dir().join(format!("calli_cursor_fp_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src_path = dir.join("state.vscdb");
        let src = Connection::open(&src_path).unwrap();
        src.execute_batch(
            "CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value BLOB);
             INSERT INTO cursorDiskKV VALUES
               ('composerData:a', json('{\"name\":\"A\",\"createdAt\":1733732099602}')),
               ('bubbleId:a:1', json('{\"type\":1,\"text\":\"question a\"}')),
               ('composerData:b', json('{\"name\":\"B\",\"createdAt\":1733732099602}')),
               ('bubbleId:b:1', json('{\"type\":1,\"text\":\"question b\"}'));",
        )
        .unwrap();

        let mut conn = crate::db::open(&dir.join("index.db")).unwrap();
        let first = scan_path(&mut conn, &src_path, &mut || {}).unwrap();
        assert_eq!(first.threads_indexed, 2);

        // Composer a's bubble grows in place (same bubble count): only a is re-extracted.
        src.execute(
            "UPDATE cursorDiskKV SET value = json('{\"type\":1,\"text\":\"question a, now longer\"}')
             WHERE key = 'bubbleId:a:1'",
            [],
        )
        .unwrap();
        // Force the file-level change key to differ (mtime has 1s resolution).
        conn.execute(
            "DELETE FROM index_state WHERE path NOT LIKE '%#composer:%'",
            [],
        )
        .unwrap();
        let second = scan_path(&mut conn, &src_path, &mut || {}).unwrap();
        assert_eq!(second.threads_indexed, 1, "unchanged composer b is skipped");

        // Deleting thread b tombstones it; a later change to b doesn't bring it back.
        let b: i64 = conn
            .query_row("SELECT id FROM threads WHERE external_id = 'b'", [], |r| {
                r.get(0)
            })
            .unwrap();
        crate::cleanup::delete_threads(&mut conn, &[b]).unwrap();
        src.execute(
            "INSERT INTO cursorDiskKV VALUES ('bubbleId:b:2', json('{\"type\":2,\"text\":\"answer\"}'))",
            [],
        )
        .unwrap();
        conn.execute(
            "DELETE FROM index_state WHERE path NOT LIKE '%#composer:%'",
            [],
        )
        .unwrap();
        let third = scan_path(&mut conn, &src_path, &mut || {}).unwrap();
        assert_eq!(third.threads_indexed, 0);
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM threads WHERE external_id = 'b'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "tombstoned composer stays deleted");
    }

    /// Real-data smoke test. Run with `cargo test -- --ignored real_cursor_index --nocapture`.
    #[test]
    #[ignore]
    fn real_cursor_index() {
        let mut p = std::env::temp_dir();
        p.push(format!("callimachus_cursor_real_{}.db", std::process::id()));
        let mut conn = crate::db::open(&p).unwrap();
        let report = scan(&mut conn, &mut || {}).unwrap();
        eprintln!("{report:?}");
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        eprintln!("cursor messages indexed: {n}");
    }
}
