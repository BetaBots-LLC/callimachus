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

/// Free pages worth a whole-file rewrite: at least this many bytes...
pub const RECLAIM_MIN_BYTES: u64 = 50 * 1024 * 1024;
/// ...and at least this share of the file, in percent. A VACUUM rewrites the whole index
/// (5.1 GB measured) under the write lock and needs about twice that free on disk, so it
/// only runs when it gives back something worth it: on that index it would have freed
/// 62 pages (248 KB, 0.005% of the file).
pub const RECLAIM_MIN_PERCENT: u64 = 1;

/// Space a VACUUM would give back, for the Reclaim disk space row.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Reclaimable {
    /// Free pages inside the file (`freelist_count * page_size`).
    pub bytes: u64,
    /// The database size in pages (`page_count * page_size`).
    pub file_bytes: u64,
    /// Over both thresholds ([`RECLAIM_MIN_BYTES`], [`RECLAIM_MIN_PERCENT`]), so
    /// [`vacuum`] will rewrite the file instead of returning at once.
    pub worthwhile: bool,
}

/// Free space inside the index. Three header PRAGMAs (no page reads beyond page 1), cheap
/// enough for the header stats every view polls; works on a read-only connection.
pub fn reclaimable(conn: &Connection) -> Result<Reclaimable> {
    reclaimable_over(conn, RECLAIM_MIN_BYTES)
}

fn reclaimable_over(conn: &Connection, min_bytes: u64) -> Result<Reclaimable> {
    let pragma = |name: &str| -> Result<u64> {
        let v: i64 = conn.query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))?;
        Ok(v.max(0) as u64)
    };
    let page = pragma("page_size")?;
    let bytes = pragma("freelist_count")? * page;
    let file_bytes = pragma("page_count")? * page;
    Ok(Reclaimable {
        bytes,
        file_bytes,
        worthwhile: worth_reclaiming(bytes, file_bytes, min_bytes),
    })
}

fn worth_reclaiming(free: u64, file: u64, min_bytes: u64) -> bool {
    free > 0 && free >= min_bytes && free.saturating_mul(100) >= file * RECLAIM_MIN_PERCENT
}

/// What one [`vacuum`] call did.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VacuumReport {
    /// False when there was too little free space to be worth the rewrite (see
    /// [`Reclaimable::worthwhile`]); nothing was written.
    pub ran: bool,
    /// How much smaller the database file is afterwards (size before minus after).
    pub freed_bytes: u64,
    /// The free pages measured before deciding, so a skipped run can say how little it was.
    pub reclaimable_bytes: u64,
    /// The closing checkpoint was held back by a reader (a read-pool query, a background
    /// pass mid-scan), so the main file has not shrunk yet and `freed_bytes` undercounts:
    /// the rest comes back at a later checkpoint. Reported so the toast doesn't say
    /// "Reclaimed 0 B" for space that is only waiting.
    pub pending: bool,
}

/// Reclaim disk space freed by deletes (rewrites the DB file; also checkpoints WAL).
///
/// Unless the free pages pass [`RECLAIM_MIN_BYTES`] and [`RECLAIM_MIN_PERCENT`] of the
/// file, returns after the cheap vector housekeeping (which may delete dead vectors),
/// without the rewrite.
///
/// Runs on a dedicated connection. VACUUM builds a compacted copy of the database in a
/// transient temp database and then writes it back through the WAL, so it needs roughly
/// the live size of the index free in the temp dir AND again on the index's own volume.
/// `db::open` sets `temp_store = MEMORY` for fast sorts, which would put that whole copy
/// in RAM (gigabytes on a large index), so it is switched to FILE for the duration.
pub fn vacuum(conn: &Connection) -> Result<VacuumReport> {
    vacuum_over(conn, RECLAIM_MIN_BYTES)
}

fn vacuum_over(conn: &Connection, min_bytes: u64) -> Result<VacuumReport> {
    // Cheap housekeeping first, so the compacted file doesn't keep dead vectors.
    reconcile_vec_owners(conn)?;
    sweep_orphan_vectors(conn)?;

    let free = reclaimable_over(conn, min_bytes)?;
    if !free.worthwhile {
        return Ok(VacuumReport {
            ran: false,
            freed_bytes: 0,
            reclaimable_bytes: free.bytes,
            pending: false,
        });
    }

    let db_path = conn.path().filter(|p| !p.is_empty()).map(PathBuf::from);
    let live = live_bytes(conn)?;
    if let Some(db_path) = &db_path {
        let db_dir = db_path.parent().map(Path::to_path_buf).unwrap_or_default();
        check_free_space(&db_dir, &sqlite_temp_dir(), live)?;
    }
    // Fold the WAL in first, so `before` and `after` both measure a checkpointed main
    // file: growth still sitting in the WAL would otherwise land in the file during the
    // closing checkpoint and be subtracted from what the VACUUM freed.
    let _ = checkpoint_truncate(conn);
    let before = file_bytes(conn, db_path.as_deref());

    conn.pragma_update(None, "temp_store", "FILE")?;
    let res = conn.execute_batch("VACUUM;");
    // Always restore the fast default, even if VACUUM failed (a failed VACUUM loses
    // nothing: the freed pages simply stay on the freelist for reuse).
    let _ = conn.pragma_update(None, "temp_store", "MEMORY");
    res?;
    // VACUUM in WAL mode leaves a WAL about the size of the database; fold it back now
    // instead of waiting for an autocheckpoint, so the disk space actually comes back. The
    // main file shrinks only here, so "freed" is measured after it. A reader holding an
    // older snapshot stops the checkpoint partway, and the file is not truncated yet.
    let complete = checkpoint_truncate(conn).unwrap_or(false);
    let after = file_bytes(conn, db_path.as_deref());
    Ok(VacuumReport {
        ran: true,
        freed_bytes: before.saturating_sub(after),
        reclaimable_bytes: free.bytes,
        pending: !complete,
    })
}

/// `PRAGMA wal_checkpoint(TRUNCATE)`; true when it ran to completion. Its row is (busy,
/// WAL frames, frames checkpointed): busy, or fewer frames copied than the WAL holds,
/// means a reader held it back. Not in WAL mode (an in-memory test db) reads as -1 frames,
/// which is complete: there is nothing to fold back.
fn checkpoint_truncate(conn: &Connection) -> Result<bool> {
    let (busy, log, done): (i64, i64, i64) =
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
    Ok(busy == 0 && done >= log)
}

/// The main database file's size on disk, else (no file, e.g. in memory) its page count.
fn file_bytes(conn: &Connection, path: Option<&Path>) -> u64 {
    if let Some(len) = path
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
    {
        return len;
    }
    reclaimable_over(conn, 0).map_or(0, |r| r.file_bytes)
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
/// Windows' VFS ignores SQLITE_TMPDIR / TMPDIR and asks `GetTempPathW` (TMP, TEMP,
/// USERPROFILE), which is what `temp_dir` returns; a TMPDIR from an MSYS or Git Bash shell
/// would otherwise point the free-space probe at the wrong volume.
fn sqlite_temp_dir() -> PathBuf {
    if cfg!(windows) {
        return std::env::temp_dir();
    }
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

/// Free space on the volume holding `dir`. Windows has no `df`, and skipping the check there
/// let a VACUUM write about 2x the index (temp copy plus WAL) to the system drive, which can
/// fill C: for every app before SQLite reports SQLITE_FULL. `GetVolumePathNameW` names the
/// volume root (or mount folder) holding `dir`, which two paths compare by the way `df`'s
/// mount point does on unix, and `GetDiskFreeSpaceExW` reports what this user may write
/// there (disk quotas included).
#[cfg(windows)]
fn volume_space(dir: &Path) -> Option<VolumeSpace> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetVolumePathNameW};

    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
    // A drive root, a UNC share root or a mount-folder path; 1024 covers long mount paths.
    let mut root = vec![0u16; 1024];
    // SAFETY: `wide` is NUL-terminated, and `root` is writable for the length passed.
    let ok = unsafe { GetVolumePathNameW(wide.as_ptr(), root.as_mut_ptr(), root.len() as u32) };
    if ok == 0 {
        return None;
    }
    let len = root.iter().position(|&c| c == 0)?;
    let mut available = 0u64;
    // SAFETY: `root` is NUL-terminated at `len` and `available` is a valid out pointer; the
    // two totals are optional and may be null.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            root.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return None;
    }
    let mount = std::ffi::OsString::from_wide(&root[..len]);
    Some(VolumeSpace {
        mount: normalize_volume_root(&mount.to_string_lossy()),
        available,
    })
}

/// No free-space probe on other platforms; the check is skipped (SQLite still fails
/// cleanly with SQLITE_FULL if the disk fills, and a failed VACUUM loses nothing).
#[cfg(not(any(unix, windows)))]
fn volume_space(_dir: &Path) -> Option<VolumeSpace> {
    None
}

/// One spelling per Windows volume root, so [`check_free_space`] compares volumes, not
/// strings: paths are case-insensitive there (`c:\` and `C:\` are one volume), the
/// trailing separator is normalized to exactly one, and the extended-length prefix is
/// dropped (`GetVolumePathNameW` keeps it, so a `\\?\C:\...` CALLIMACHUS_DB or temp path
/// named a different volume than `C:\` and the 2x rule quietly fell back to 1x).
#[cfg_attr(not(windows), allow(dead_code))]
fn normalize_volume_root(root: &str) -> String {
    let root = if let Some(share) = root.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{share}")
    } else {
        root.strip_prefix(r"\\?\").unwrap_or(root).to_string()
    };
    let mut s = root.trim_end_matches(['\\', '/']).to_uppercase();
    s.push('\\');
    s
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

/// Whether some vectors have no owner row, such as ones an older build (no owner map) kept
/// writing after a newer process migrated the shared index. Two b-tree counts, well under
/// a millisecond warm at ~115k vectors, not a scan of the vectors. Owner rows never
/// outnumber vectors (deleting one deletes its vector, and nothing deletes a vector that
/// still has one), so more vectors than owner rows means at least one is unowned.
pub fn vectors_missing_owners(conn: &Connection) -> Result<bool> {
    Ok(conn
        .prepare_cached(
            "SELECT (SELECT COUNT(*) FROM vec_chunks_rowids)
                  > (SELECT COUNT(*) FROM vec_chunk_owner)",
        )?
        .query_row([], |r| r.get(0))?)
}

/// Add owner rows for any vectors written without one (e.g. by an embedder build that
/// predates migration 0025), so their message's delete still removes them and the
/// project/source-scoped KNN pre-filter sees them (until then semantic search falls back to
/// post-filtering; see `embed::semantic_search_vec`). Checks [`vectors_missing_owners`]
/// first, so when nothing is missing it costs two counts and is cheap enough for every
/// launch; otherwise one full scan of the vector table (sub-second at a few hundred
/// thousand chunks). Search can't do this repair itself: it runs on read-only connections.
/// Returns how many rows were added.
pub fn reconcile_vec_owners(conn: &Connection) -> Result<usize> {
    if !vectors_missing_owners(conn)? {
        return Ok(0);
    }
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

/// Compact `vec_chunks` once live vectors fill less than this share of its slots.
const VEC_COMPACT_OCCUPANCY: f64 = 0.7;

/// What one [`compact_vec_chunks`] rebuild did.
#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VecCompaction {
    pub vectors: i64,
    pub chunks_before: i64,
    pub chunks_after: i64,
}

/// Rebuild `vec_chunks` without the holes deletes leave in it, keeping every rowid.
///
/// vec0 (0.1.9) stores vectors in fixed 1024-slot chunks and only ever appends to the
/// newest one, so a deleted vector's slot is never reused; only a chunk that empties
/// completely is freed. Migration 0029's tool-vector purge left a 4.7 GB index at 56%
/// occupancy (205 chunks for 117k vectors), and KNN reads and ranks every slot of every
/// chunk. Rebuilt, it fit in 115 chunks, and a k=500 KNN went from 165 ms to 106 ms.
///
/// Does nothing (returns `None`) unless occupancy is under [`VEC_COMPACT_OCCUPANCY`] and the
/// rebuild would free at least one chunk, so it is cheap to call after every embedding
/// build. Otherwise it holds the write lock for one IMMEDIATE transaction (4-9 s for 117k
/// vectors, depending on how warm the cache is), so run it from a background job, never
/// from a migration a hook might run. vec0 can't be renamed, so the vectors go through a
/// scratch table in the main database (TEMP would sit in RAM under temp_store = MEMORY).
///
/// It returns no disk space; the file grows. The scratch copy is written before the old
/// chunks are dropped, so it lands at the end of the file (+229 MB on that index), and the
/// old chunks and the scratch table then become free pages inside the file (~366 MB there).
/// Later writes reuse them; only a VACUUM (Shrink index / Reclaim space) gives them back to
/// the disk. What the rebuild buys is a faster KNN, not a smaller file.
///
/// The table is recreated from its own stored CREATE statement. Triggers that reference
/// `vec_chunks` (0025's owner map) survive the DROP / CREATE, and the rowid sequence is
/// carried over so a later insert never reuses the rowid of a vector deleted before the
/// rebuild.
pub fn compact_vec_chunks(conn: &mut Connection) -> Result<Option<VecCompaction>> {
    if !vec_chunks_fragmented(conn)? {
        return Ok(None);
    }
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    // Re-check under the write lock: another writer may have changed the table meanwhile.
    if !vec_chunks_fragmented(&tx)? {
        return Ok(None);
    }
    let chunks_before = count_vec_chunk_rows(&tx)?;
    let create: String = tx.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'vec_chunks'",
        [],
        |r| r.get(0),
    )?;
    let seq: Option<i64> = tx
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name = 'vec_chunks_rowids'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    tx.execute_batch(
        "DROP TABLE IF EXISTS vec_compact_scratch;
         CREATE TABLE vec_compact_scratch (
             rid INTEGER PRIMARY KEY, message_id INTEGER, embedding BLOB NOT NULL
         );
         INSERT INTO vec_compact_scratch (rid, message_id, embedding)
             SELECT rowid, message_id, embedding FROM vec_chunks;
         DROP TABLE vec_chunks;",
    )?;
    tx.execute_batch(&create)?;
    let vectors = tx.execute(
        "INSERT INTO vec_chunks (rowid, message_id, embedding)
         SELECT rid, message_id, embedding FROM vec_compact_scratch ORDER BY rid",
        [],
    )? as i64;
    // Leave the owner map exact: no owner row for a vector that is gone (its rowid must
    // stay free), and one for every vector that kept none.
    tx.execute_batch(
        "DELETE FROM vec_chunk_owner
             WHERE chunk_rowid NOT IN (SELECT rid FROM vec_compact_scratch);
         INSERT OR IGNORE INTO vec_chunk_owner (chunk_rowid, message_id)
             SELECT rid, message_id FROM vec_compact_scratch;
         DROP TABLE vec_compact_scratch;",
    )?;
    if let Some(seq) = seq {
        let updated = tx.execute(
            "UPDATE sqlite_sequence SET seq = MAX(seq, ?1) WHERE name = 'vec_chunks_rowids'",
            [seq],
        )?;
        if updated == 0 {
            // An empty rebuild has no sequence row yet.
            tx.execute(
                "INSERT INTO sqlite_sequence (name, seq) VALUES ('vec_chunks_rowids', ?1)",
                [seq],
            )?;
        }
    }
    let chunks_after = count_vec_chunk_rows(&tx)?;
    tx.commit()?;
    Ok(Some(VecCompaction {
        vectors,
        chunks_before,
        chunks_after,
    }))
}

fn count_vec_chunk_rows(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM vec_chunks_chunks", [], |r| r.get(0))?)
}

/// Whether a [`compact_vec_chunks`] rebuild is worth its write lock.
fn vec_chunks_fragmented(conn: &Connection) -> Result<bool> {
    // `size` is each chunk's slot capacity; the rowids shadow table holds one row per live
    // vector. Both are small b-tree counts, not a scan of the vectors.
    let (chunks, slots): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(size), 0) FROM vec_chunks_chunks",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let live: i64 = conn.query_row("SELECT COUNT(*) FROM vec_chunks_rowids", [], |r| r.get(0))?;
    Ok(worth_compacting(live, chunks, slots))
}

fn worth_compacting(live: i64, chunks: i64, slots: i64) -> bool {
    if chunks == 0 || slots == 0 {
        return false;
    }
    let per_chunk = slots / chunks;
    let needed = (live + per_chunk - 1) / per_chunk;
    chunks > needed && (live as f64) < VEC_COMPACT_OCCUPANCY * slots as f64
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
    if crate::knowledge::app_config::get(conn, PURGED_APP_SESSIONS_KEY)?.is_some() {
        return Ok(0);
    }
    let n = purge_app_cli_sessions(conn)?;
    crate::knowledge::app_config::put(
        conn,
        PURGED_APP_SESSIONS_KEY,
        &chrono::Utc::now().timestamp().to_string(),
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

/// Progress of one [`compact_tool_output`] call.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactReport {
    /// Tool output (and Claude Code tool-call) rows rewritten in this call.
    pub rows: usize,
    /// UTF-8 bytes of message text removed in this call.
    pub bytes_saved: i64,
    /// True when no oversized rows remain (stop calling).
    pub done: bool,
    /// The last message id this call looked at: pass it to [`compact_tool_output_after`]
    /// so the next call resumes there instead of re-reading the rows already done.
    #[serde(skip)]
    pub last_id: i64,
}

/// The next `batch` oversized tool rows after id `?1`, for [`compact_tool_output`].
fn compact_select_sql(batch: usize) -> String {
    use crate::indexer::claude::TOOL_TEXT_CAP_CHARS;
    // The registry flags which indexers cap tool output (`indexer::Source::caps_tool_output`);
    // compacting any other source's rows would just be undone by its next re-index.
    let kinds = crate::indexer::capped_tool_sources()
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
    //
    // Claude Code also caps its tool calls (assistant rows with a tool_name, "Write: {json}").
    // Only its calls: the other capping sources store calls in full, so compacting theirs
    // would be undone by the next re-index.
    let claude = crate::indexer::claude::KIND;
    format!(
        "SELECT m.id, m.thread_id, m.text FROM messages m
         WHERE m.id > ?1 AND octet_length(m.text) > {TOOL_TEXT_CAP_CHARS}
           AND ((+m.role = 'tool'
                 AND +m.thread_id IN (
                     SELECT t.id FROM threads t JOIN sources s ON s.id = t.source_id
                     WHERE s.kind IN ({kinds})))
                OR (+m.role = 'assistant' AND +m.tool_name IS NOT NULL
                    AND +m.thread_id IN (
                        SELECT t.id FROM threads t JOIN sources s ON s.id = t.source_id
                        WHERE s.kind = '{claude}')))
         ORDER BY m.id LIMIT {batch}"
    )
}

/// Shrink tool-output rows (and Claude Code tool calls) indexed before the index-time cap
/// existed down to the same head + tail form the indexer now stores, so old transcripts
/// stop dominating disk and FTS size. Looks at about `max_rows` oversized rows per call (in
/// short transactions) so the caller can loop from a background thread and release the
/// write lock in between; FTS stays in sync through the update trigger. Run `vacuum`
/// afterwards to return the space to the OS. Only touches the sources whose indexers cap
/// tool output (`indexer::capped_tool_sources`), and tool calls only for Claude Code.
pub fn compact_tool_output(conn: &mut Connection, max_rows: usize) -> Result<CompactReport> {
    compact_tool_output_after(conn, 0, max_rows)
}

/// [`compact_tool_output`] over message ids above `after` only (the previous call's
/// `last_id`). Ids only grow (a re-indexed thread's rows are re-inserted with new ones), so
/// nothing at or below `after` can need compacting again within one run.
pub fn compact_tool_output_after(
    conn: &mut Connection,
    after: i64,
    max_rows: usize,
) -> Result<CompactReport> {
    compact_where(conn, after, max_rows, &mut |_, _| Ok(true))
}

/// [`compact_tool_output_after`] limited to threads whose transcript is still on disk
/// (`on_disk`), for the automatic background pass. The cap drops the middle of each row
/// for good, so it is only applied where the transcript still holds the full text (a
/// re-index of that file stores the same capped text anyway). Rows of threads whose
/// transcript the agent has since deleted, where the index holds the only copy, are left
/// for an explicit Shrink index.
pub fn compact_tool_output_on_disk(
    conn: &mut Connection,
    after: i64,
    max_rows: usize,
    on_disk: &mut TranscriptsOnDisk,
) -> Result<CompactReport> {
    compact_where(conn, after, max_rows, &mut |c, thread_id| {
        on_disk.has(c, thread_id)
    })
}

fn compact_where(
    conn: &mut Connection,
    after: i64,
    max_rows: usize,
    keep: &mut dyn FnMut(&Connection, i64) -> Result<bool>,
) -> Result<CompactReport> {
    use crate::indexer::claude::cap_tool_text;
    const BATCH: usize = 200;
    let mut report = CompactReport {
        last_id: after,
        ..Default::default()
    };
    let select = compact_select_sql(BATCH);
    let mut after = after;
    // Counts rows looked at, not rows rewritten: with a `keep` filter a batch may rewrite
    // none, and a bound on rewrites alone would let one call scan the whole table.
    let mut seen = 0usize;
    while seen < max_rows {
        // Read outside the write transaction, so the (up to ~1 s) scan for a batch never
        // holds the write lock; the guarded UPDATE below makes a row changed meanwhile safe.
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
        report.last_id = after;
        seen += rows.len();
        let mut kept = Vec::with_capacity(rows.len());
        for row in rows {
            if keep(conn, row.1)? {
                kept.push(row);
            }
        }
        if kept.is_empty() {
            continue;
        }
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        {
            // Only while the row still holds the text read above (its byte length; the
            // capped form is always shorter). A second compaction that capped it first, or
            // a re-index that replaced the thread, leaves 0 rows updated, and the thread's
            // bytes are then left alone instead of being decremented twice.
            let mut upd = tx.prepare(
                "UPDATE messages SET text = ?2 WHERE id = ?1 AND octet_length(text) = ?3",
            )?;
            // content_hash describes the old text, so drop it: anything that trusts it to
            // skip a re-index must re-verify this thread instead.
            let mut bytes = tx.prepare(
                "UPDATE threads SET bytes = MAX(0, bytes - ?2), content_hash = NULL WHERE id = ?1",
            )?;
            for (id, thread_id, text) in &kept {
                let capped = cap_tool_text(text);
                if capped.len() >= text.len() {
                    continue; // multi-byte text under the char cap: nothing to trim
                }
                let saved = (text.len() - capped.len()) as i64;
                if upd.execute(params![id, capped, text.len() as i64])? == 0 {
                    continue;
                }
                bytes.execute(params![thread_id, saved])?;
                report.rows += 1;
                report.bytes_saved += saved;
            }
        }
        tx.commit()?;
    }
    Ok(report)
}

/// Which threads of the capped sources still have their transcript on disk, for
/// [`compact_tool_output_on_disk`]. Built from `index_state`, which records every file a
/// file-based indexer read (absolute path and source kind): a file that still exists is a
/// transcript a re-index can read again. Threads are matched to files by their id, and a
/// thread that matches no existing file counts as gone, so a miss only skips a row.
pub struct TranscriptsOnDisk {
    /// Existing files per source kind, keyed by file name, as normalized ('/') paths.
    by_name: std::collections::HashMap<(String, String), Vec<String>>,
    /// Existing files per source kind, for ids that name a session instead of a path.
    by_kind: std::collections::HashMap<String, Vec<String>>,
    /// Verdicts already reached, per thread id (a batch holds many rows of one thread).
    seen: std::collections::HashMap<i64, bool>,
}

impl TranscriptsOnDisk {
    /// Stat every file `index_state` lists for the capped sources (about 20k on a large
    /// index; one metadata call each).
    pub fn load(conn: &Connection) -> Result<Self> {
        let kinds: Vec<&str> = crate::indexer::capped_tool_sources().collect();
        let mut stmt = conn.prepare("SELECT path, source_kind FROM index_state")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut me = Self {
            by_name: Default::default(),
            by_kind: Default::default(),
            seen: Default::default(),
        };
        for (path, kind) in rows {
            if !kinds.contains(&kind.as_str()) || !Path::new(&path).is_file() {
                continue;
            }
            me.add(&kind, &path);
        }
        Ok(me)
    }

    fn add(&mut self, kind: &str, path: &str) {
        let norm = path.replace('\\', "/");
        let name = norm.rsplit('/').next().unwrap_or(&norm).to_string();
        self.by_name
            .entry((kind.to_string(), name))
            .or_default()
            .push(norm.clone());
        self.by_kind.entry(kind.to_string()).or_default().push(norm);
    }

    /// How many existing transcript files were found (for the log line).
    pub fn files(&self) -> usize {
        self.by_kind.values().map(Vec::len).sum()
    }

    /// Whether thread `thread_id`'s transcript is still on disk.
    pub fn has(&mut self, conn: &Connection, thread_id: i64) -> Result<bool> {
        if let Some(&v) = self.seen.get(&thread_id) {
            return Ok(v);
        }
        let row: Option<(String, String)> = conn
            .prepare_cached(
                "SELECT t.external_id, s.kind FROM threads t JOIN sources s ON s.id = t.source_id
                 WHERE t.id = ?1",
            )?
            .query_row([thread_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let v = row.is_some_and(|(id, kind)| self.matches(&kind, &id));
        self.seen.insert(thread_id, v);
        Ok(v)
    }

    fn matches(&self, kind: &str, external_id: &str) -> bool {
        let id = external_id.replace('\\', "/");
        let name = id.rsplit('/').next().unwrap_or(&id);
        if name.ends_with(".jsonl") || name.ends_with(".json") {
            // The id is the transcript's path below its root (Claude Code, Gemini, Qwen):
            // some existing file must end with exactly that path.
            let tail = format!("/{id}");
            return self
                .by_name
                .get(&(kind.to_string(), name.to_string()))
                .is_some_and(|paths| paths.iter().any(|p| p.ends_with(&tail)));
        }
        // A session id (Codex `rollout-<ts>-<uuid>.jsonl`, Continue `<id>.json`) or a task
        // folder (Cline-style `<editor>/<taskId>`): the id's last segment must be a whole
        // folder name, the file stem, or the stem's `-`-separated tail. Short tokens are
        // refused, so a generic name can't match some unrelated file.
        if name.len() < 8 {
            return false;
        }
        let dashed = format!("-{name}");
        self.by_kind.get(kind).is_some_and(|paths| {
            paths.iter().any(|p| {
                let mut parts = p.rsplit('/');
                let file = parts.next().unwrap_or("");
                let stem = file.rsplit_once('.').map_or(file, |(s, _)| s);
                stem == name || stem.ends_with(&dashed) || parts.any(|dir| dir == name)
            })
        })
    }
}

/// Bump when the background compaction's selection changes in a way the cap and the
/// capped source set (both part of the marker) don't show: the Claude-only tool-call rule
/// in [`compact_select_sql`], or which threads [`compact_tool_output_on_disk`] keeps. A
/// changed marker runs the pass once more on every index.
const COMPACTION_VERSION: u32 = 2;

/// `app_config` key recording the compaction a run last finished (see
/// [`compaction_marker`]). Rows indexed since are capped as they are written, so later
/// launches skip the pass; a build that changes the cap, the capped sources or
/// [`COMPACTION_VERSION`] runs it again.
const TOOL_OUTPUT_COMPACTED_KEY: &str = "cleanup.tool_output_compacted_cap";

/// `v<COMPACTION_VERSION>:<cap>:<capped kinds>`, so a later build that starts capping
/// another source re-runs the pass instead of trusting a marker set before.
fn compaction_marker() -> String {
    let kinds = crate::indexer::capped_tool_sources()
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "v{COMPACTION_VERSION}:{}:{kinds}",
        crate::indexer::claude::TOOL_TEXT_CAP_CHARS
    )
}

/// Whether the background compaction still has to finish on this index for this build.
pub fn tool_output_compaction_pending(conn: &Connection) -> Result<bool> {
    Ok(
        crate::knowledge::app_config::get(conn, TOOL_OUTPUT_COMPACTED_KEY)?.as_deref()
            != Some(compaction_marker().as_str()),
    )
}

/// Record that a compaction run reached the end of the table for this build.
pub fn mark_tool_output_compacted(conn: &Connection) -> Result<()> {
    crate::knowledge::app_config::put(conn, TOOL_OUTPUT_COMPACTED_KEY, &compaction_marker())
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

    /// Claude Code caps its tool calls at index time, so legacy uncapped ones are compacted
    /// too; a prose assistant row and another source's (uncapped) tool calls are left alone.
    #[test]
    fn compact_tool_output_caps_claude_code_tool_calls_only() {
        use crate::indexer::claude::cap_tool_text;
        let mut conn = temp_db();
        let big = format!("Write: {{\"content\":\"{}\"}}", "line ".repeat(10_000));
        let call = |text: &str| ParsedMessage {
            role: "assistant".into(),
            text: text.into(),
            tool_name: Some("Write".into()),
            ts: Some(1),
        };
        let mk = |ext: &str| ParsedThread {
            external_id: ext.into(),
            messages: vec![
                msg("user", "write it"),
                call(&big),
                msg("assistant", &"prose".repeat(5_000)),
            ],
            ..Default::default()
        };
        let claude = source_id(&conn, "claude_code").unwrap();
        let codex = source_id(&conn, "codex").unwrap();
        upsert_thread(&mut conn, claude, &mk("cc")).unwrap();
        upsert_thread(&mut conn, codex, &mk("cx")).unwrap();

        let r = compact_tool_output(&mut conn, 1000).unwrap();
        assert_eq!(r.rows, 1, "only Claude Code's tool call is rewritten");
        let text_of = |sid: i64, seq: i64| -> String {
            conn.query_row(
                "SELECT m.text FROM messages m JOIN threads t ON t.id = m.thread_id
                 WHERE t.source_id = ?1 AND m.seq = ?2",
                params![sid, seq],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(text_of(claude, 1), cap_tool_text(&big));
        assert_eq!(text_of(claude, 2), "prose".repeat(5_000));
        assert_eq!(text_of(codex, 1), big);
    }

    /// A background run resumes after the last id the previous pass looked at, and its
    /// completion marker is tied to the build's compaction (cap, capped sources, version).
    #[test]
    fn compact_resumes_after_the_cursor_and_marks_completion() {
        let mut conn = temp_db();
        let sid = source_id(&conn, "claude_code").unwrap();
        let big = "x".repeat(40_000);
        let t = ParsedThread {
            external_id: "big".into(),
            messages: vec![msg("tool", &big), msg("tool", &big), msg("tool", &big)],
            ..Default::default()
        };
        upsert_thread(&mut conn, sid, &t).unwrap();
        let ids: Vec<i64> = conn
            .prepare("SELECT id FROM messages ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();

        // Rows at or below the cursor are not looked at again.
        let r = compact_tool_output_after(&mut conn, ids[1], 2_000).unwrap();
        assert_eq!(r.rows, 1);
        assert!(r.done);
        assert_eq!(r.last_id, ids[2]);
        let long = "SELECT COUNT(*) FROM messages WHERE length(text) > 30000";
        assert_eq!(count(&conn, long), 2);
        let r = compact_tool_output_after(&mut conn, 0, 2_000).unwrap();
        assert_eq!((r.rows, r.done), (2, true));
        // Nothing left: the cursor stays where it was given.
        let r = compact_tool_output_after(&mut conn, ids[2], 2_000).unwrap();
        assert_eq!((r.rows, r.done, r.last_id), (0, true, ids[2]));

        assert!(tool_output_compaction_pending(&conn).unwrap());
        mark_tool_output_compacted(&conn).unwrap();
        assert!(!tool_output_compaction_pending(&conn).unwrap());
        // Finished by another build (an older marker held only the cap): run again.
        let cap = crate::indexer::claude::TOOL_TEXT_CAP_CHARS.to_string();
        crate::knowledge::app_config::put(&conn, TOOL_OUTPUT_COMPACTED_KEY, &cap).unwrap();
        assert!(tool_output_compaction_pending(&conn).unwrap());
    }

    /// The automatic pass trims only threads whose transcript is still on disk (a re-index
    /// would store the same capped text); a thread whose file is gone keeps its full text
    /// for an explicit Shrink index, which trims everything.
    #[test]
    fn background_compaction_keeps_threads_whose_transcript_is_gone() {
        let mut conn = temp_db();
        let dir = std::env::temp_dir().join(format!(
            "calli_on_disk_{}_{}",
            std::process::id(),
            conn.path().unwrap().rsplit(['/', '\\']).next().unwrap()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let big = "x".repeat(40_000);
        let uuid = "0199aaaa-bbbb-cccc-dddd-eeeeffff0000";
        let rollout = format!("sessions/2025/01/01/rollout-2025-01-01T00-00-00-{uuid}.jsonl");
        // (kind, external id, transcript path, still on disk)
        let cases = [
            (
                "claude_code",
                "slug/kept.jsonl",
                "projects/slug/kept.jsonl",
                true,
            ),
            (
                "claude_code",
                "slug/gone.jsonl",
                "projects/slug/gone.jsonl",
                false,
            ),
            ("codex", uuid, rollout.as_str(), true),
            (
                "cline",
                "Code/1780000000000",
                "Code/User/globalStorage/x/tasks/1780000000000/api_conversation_history.json",
                true,
            ),
        ];
        for (kind, ext, rel, exists) in cases {
            let path = dir.join(rel);
            if exists {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, "{}").unwrap();
            }
            conn.execute(
                "INSERT INTO index_state (path, source_kind) VALUES (?1, ?2)",
                params![path.to_string_lossy(), kind],
            )
            .unwrap();
            let sid = source_id(&conn, kind).unwrap();
            let t = ParsedThread {
                external_id: ext.into(),
                messages: vec![msg("tool", &big)],
                ..Default::default()
            };
            upsert_thread(&mut conn, sid, &t).unwrap();
        }
        let gone = "SELECT length(m.text) FROM messages m JOIN threads t ON t.id = m.thread_id
                    WHERE t.external_id = 'slug/gone.jsonl'";

        let mut on_disk = TranscriptsOnDisk::load(&conn).unwrap();
        assert_eq!(on_disk.files(), 3);
        let r = compact_tool_output_on_disk(&mut conn, 0, 2_000, &mut on_disk).unwrap();
        assert_eq!((r.rows, r.done), (3, true));
        assert_eq!(count(&conn, gone), 40_000, "the only copy is left whole");
        let r = compact_tool_output(&mut conn, 2_000).unwrap();
        assert_eq!(r.rows, 1, "Shrink index trims it");
        assert!(count(&conn, gone) < 40_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A row another compaction capped after this one read it is not rewritten again, and
    /// its thread's bytes are not decremented a second time.
    #[test]
    fn a_row_capped_meanwhile_is_not_counted_twice() {
        use crate::indexer::claude::cap_tool_text;
        let mut conn = temp_db();
        let sid = source_id(&conn, "claude_code").unwrap();
        let big = "x".repeat(40_000);
        let t = ParsedThread {
            external_id: "big".into(),
            messages: vec![msg("tool", &big)],
            ..Default::default()
        };
        upsert_thread(&mut conn, sid, &t).unwrap();
        let bytes_before = count(&conn, "SELECT bytes FROM threads");
        let r = compact_where(&mut conn, 0, 2_000, &mut |c, _| {
            // The other compaction: caps the row between this one's read and its update.
            c.execute("UPDATE messages SET text = ?1", [cap_tool_text(&big)])?;
            Ok(true)
        })
        .unwrap();
        assert_eq!((r.rows, r.bytes_saved), (0, 0));
        assert_eq!(count(&conn, "SELECT bytes FROM threads"), bytes_before);
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
        assert!(!vectors_missing_owners(&conn).unwrap(), "empty index");
        // A vector written without its owner row (an embedder that predates 0025).
        conn.execute(
            "INSERT INTO vec_chunks (message_id, embedding) VALUES (?1, ?2)",
            params![mid, blob],
        )
        .unwrap();
        assert!(vectors_missing_owners(&conn).unwrap());
        assert_eq!(reconcile_vec_owners(&conn).unwrap(), 1);
        assert!(!vectors_missing_owners(&conn).unwrap());
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
    fn compaction_threshold() {
        assert!(!worth_compacting(0, 0, 0), "no vectors yet");
        assert!(
            !worth_compacting(10, 1, 1024),
            "one sparse chunk frees nothing"
        );
        assert!(!worth_compacting(1500, 2, 2048), "73% full");
        assert!(
            worth_compacting(114_811, 205, 205 * 1024),
            "the 0029-purged index"
        );
        assert!(
            !worth_compacting(113 * 1024 - 5, 113, 113 * 1024),
            "dense after a rebuild"
        );
    }

    #[test]
    fn vec_compaction_keeps_rowids_owners_and_triggers() {
        let mut conn = temp_db();
        conn.execute(
            "INSERT INTO threads (source_id, external_id, title) VALUES (1, 'vc', 't')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        // A distinct direction per vector, so KNN can name the exact one back.
        let blob = |i: usize| -> Vec<u8> {
            let mut v = vec![0.0_f32; 384];
            v[i % 384] = 1.0;
            v[(i / 384) % 384] += 0.5;
            v.iter().flat_map(|f| f.to_le_bytes()).collect()
        };
        let mut rows = Vec::new(); // (message_id, chunk_rowid, i)
        {
            let tx = conn.transaction().unwrap();
            for i in 0..2100_usize {
                tx.execute(
                    "INSERT INTO messages (thread_id, seq, role, text, embedded)
                     VALUES (?1, ?2, 'user', 'v', 1)",
                    params![tid, i as i64],
                )
                .unwrap();
                let mid = tx.last_insert_rowid();
                tx.execute(
                    "INSERT INTO vec_chunks (message_id, embedding) VALUES (?1, ?2)",
                    params![mid, blob(i)],
                )
                .unwrap();
                let rid = tx.last_insert_rowid();
                tx.execute(
                    "INSERT INTO vec_chunk_owner (chunk_rowid, message_id) VALUES (?1, ?2)",
                    params![rid, mid],
                )
                .unwrap();
                rows.push((mid, rid, i));
            }
            tx.commit().unwrap();
        }
        let chunks = |c: &Connection| count(c, "SELECT COUNT(*) FROM vec_chunks_chunks");
        assert_eq!(chunks(&conn), 3);
        assert_eq!(
            compact_vec_chunks(&mut conn).unwrap(),
            None,
            "dense: nothing to do"
        );

        // Delete most messages, scattered so every chunk keeps a few live slots.
        {
            let tx = conn.transaction().unwrap();
            for (mid, _, i) in &rows {
                if i % 7 != 0 {
                    tx.execute("DELETE FROM messages WHERE id = ?1", [mid])
                        .unwrap();
                }
            }
            tx.commit().unwrap();
        }
        let kept: Vec<(i64, i64, usize)> = rows.iter().filter(|r| r.2 % 7 == 0).copied().collect();
        assert_eq!(chunks(&conn), 3, "vec0 never reuses the holes");
        let top_rowid = rows.last().unwrap().1;

        let report = compact_vec_chunks(&mut conn).unwrap().expect("fragmented");
        assert_eq!(
            report,
            VecCompaction {
                vectors: kept.len() as i64,
                chunks_before: 3,
                chunks_after: 1,
            }
        );
        assert_eq!(chunks(&conn), 1);
        assert_eq!(compact_vec_chunks(&mut conn).unwrap(), None, "dense again");
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'vec_compact_scratch'"
            ),
            0
        );

        // Every survivor keeps its rowid, message and vector, and the owner map is exact.
        for (mid, rid, i) in &kept {
            let (m, e): (i64, Vec<u8>) = conn
                .query_row(
                    "SELECT message_id, embedding FROM vec_chunks WHERE rowid = ?1",
                    [rid],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!((m, e), (*mid, blob(*i)));
        }
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM vec_chunk_owner"),
            kept.len() as i64
        );
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM vec_chunk_owner o
                 WHERE NOT EXISTS (SELECT 1 FROM vec_chunks_rowids r WHERE r.rowid = o.chunk_rowid)"
            ),
            0
        );
        let (_, want, i) = kept[3];
        let nearest: i64 = conn
            .query_row(
                "SELECT rowid FROM vec_chunks WHERE embedding MATCH ?1 AND k = 1",
                [blob(i)],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(nearest, want, "KNN works on the rebuilt table");

        // The owner-map triggers still remove a deleted message's vector.
        conn.execute("DELETE FROM messages WHERE id = ?1", [kept[0].0])
            .unwrap();
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM vec_chunks"),
            kept.len() as i64 - 1
        );
        // A new vector gets a rowid no deleted vector ever had.
        conn.execute(
            "INSERT INTO vec_chunks (message_id, embedding) VALUES (?1, ?2)",
            params![kept[1].0, blob(9999)],
        )
        .unwrap();
        assert!(conn.last_insert_rowid() > top_rowid);
    }

    #[test]
    fn vacuum_skips_a_file_with_little_free_space() {
        let conn = temp_db();
        let size = |c: &Connection| std::fs::metadata(c.path().unwrap()).unwrap().len();
        let before = size(&conn);
        let r = vacuum(&conn).unwrap();
        assert!(!r.ran, "a fresh index has nothing worth a rewrite");
        assert_eq!(r.freed_bytes, 0);
        assert!(!reclaimable(&conn).unwrap().worthwhile);
        assert_eq!(size(&conn), before, "nothing was written");
    }

    #[test]
    fn vacuum_reports_the_bytes_it_freed_and_restores_in_memory_temp_store() {
        let conn = temp_db();
        conn.execute_batch(
            "CREATE TABLE filler (b BLOB);
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 64)
             INSERT INTO filler SELECT zeroblob(32768) FROM n;
             PRAGMA wal_checkpoint(TRUNCATE);
             DROP TABLE filler;",
        )
        .unwrap();
        let free = reclaimable_over(&conn, 0).unwrap();
        assert!(free.bytes >= 2_000_000, "{free:?}");
        assert!(free.worthwhile);
        assert!(!reclaimable(&conn).unwrap().worthwhile, "under 50 MB");

        let r = vacuum_over(&conn, 0).unwrap();
        assert!(r.ran);
        assert_eq!(r.reclaimable_bytes, free.bytes);
        assert!(r.freed_bytes >= 2_000_000, "{r:?}");
        assert!(!r.pending, "no reader held the checkpoint back");
        assert_eq!(reclaimable_over(&conn, 0).unwrap().bytes, 0);
        let temp_store: i64 = conn
            .query_row("PRAGMA temp_store", [], |r| r.get(0))
            .unwrap();
        assert_eq!(temp_store, 2, "MEMORY again after the FILE-backed VACUUM");
    }

    /// A reader holding an older snapshot stops the closing checkpoint partway, so the file
    /// has not shrunk yet: the report says the space is pending instead of "freed 0 B".
    #[test]
    fn vacuum_held_back_by_a_reader_reports_pending() {
        let conn = temp_db();
        conn.execute_batch(
            "CREATE TABLE filler (b BLOB);
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 64)
             INSERT INTO filler SELECT zeroblob(32768) FROM n;
             PRAGMA wal_checkpoint(TRUNCATE);
             DROP TABLE filler;",
        )
        .unwrap();
        let reader = Connection::open(conn.path().unwrap()).unwrap();
        reader
            .execute_batch("BEGIN; SELECT COUNT(*) FROM sqlite_master;")
            .unwrap();
        // Don't wait out the reader for BUSY_TIMEOUT (15 s): the checkpoint gives up at once.
        conn.busy_timeout(std::time::Duration::ZERO).unwrap();
        let r = vacuum_over(&conn, 0).unwrap();
        assert!(r.ran && r.pending, "{r:?}");
        reader.execute_batch("COMMIT").unwrap();
        assert!(
            checkpoint_truncate(&conn).unwrap(),
            "completes once the reader is gone"
        );
        let r = vacuum_over(&conn, 0).unwrap();
        assert!(!r.ran && !r.pending, "{r:?}");
    }

    #[test]
    fn reclaim_threshold_needs_50_mb_and_1_percent() {
        const MB: u64 = 1024 * 1024;
        // The measured 5.1 GB index: 248 KB free.
        assert!(!worth_reclaiming(
            248 * 1024,
            5_135_241_216,
            RECLAIM_MIN_BYTES
        ));
        assert!(
            !worth_reclaiming(49 * MB, 100 * MB, RECLAIM_MIN_BYTES),
            "under 50 MB"
        );
        assert!(
            !worth_reclaiming(60 * MB, 10_000 * MB, RECLAIM_MIN_BYTES),
            "under 1%"
        );
        assert!(worth_reclaiming(60 * MB, 1_000 * MB, RECLAIM_MIN_BYTES));
        assert!(worth_reclaiming(101 * MB, 10_000 * MB, RECLAIM_MIN_BYTES));
        assert!(!worth_reclaiming(0, 0, 0), "nothing free is never worth it");
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
    fn windows_volume_roots_normalize_to_one_spelling() {
        assert_eq!(normalize_volume_root("c:\\"), "C:\\");
        assert_eq!(normalize_volume_root("C:"), "C:\\");
        assert_eq!(
            normalize_volume_root("d:\\Mounts\\Data\\"),
            normalize_volume_root("D:\\MOUNTS\\data")
        );
        assert_ne!(normalize_volume_root("C:\\"), normalize_volume_root("D:\\"));
        // Extended-length spellings name the same volumes.
        assert_eq!(normalize_volume_root(r"\\?\c:\"), "C:\\");
        assert_eq!(
            normalize_volume_root(r"\\?\UNC\server\share\"),
            normalize_volume_root(r"\\Server\Share")
        );
    }

    /// Two folders on one volume resolve to the same root (so the 2x rule applies), whatever
    /// the path's case, and that root is where the folder lives.
    #[cfg(windows)]
    #[test]
    fn windows_volume_space_compares_by_volume_root() {
        let tmp = std::env::temp_dir();
        let sub = tmp.join(format!("calli_volume_{}", std::process::id()));
        std::fs::create_dir_all(&sub).unwrap();
        let a = volume_space(&tmp).expect("temp dir volume");
        let b = volume_space(&sub).expect("subfolder volume");
        let lower = volume_space(Path::new(&tmp.to_string_lossy().to_lowercase()))
            .expect("lower-case path volume");
        let _ = std::fs::remove_dir(&sub);
        assert_eq!(a.mount, b.mount);
        assert_eq!(a.mount, lower.mount);
        assert!(a.mount.ends_with('\\'), "{}", a.mount);
        assert!(
            tmp.to_string_lossy()
                .to_uppercase()
                .starts_with(a.mount.trim_end_matches('\\')),
            "{} is not under {}",
            tmp.display(),
            a.mount
        );
        assert!(a.available > 0);
    }

    #[test]
    fn free_space_check_needs_room_on_both_volumes() {
        // Unknown free space never blocks a VACUUM.
        assert!(
            check_free_space(Path::new("/nonexistent-a"), Path::new("/nonexistent-b"), 1).is_ok()
        );
    }
}
