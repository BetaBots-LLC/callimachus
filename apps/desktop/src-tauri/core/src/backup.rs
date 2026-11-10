//! Snapshots of the index, `cal backup`, and the restore the app's startup error screen offers
//! for a damaged index.
//!
//! Why: for most users the index is the only copy of their history. On the author's machine
//! 89% of main Claude Code sessions (2.16 GB of text) have no transcript left on disk, because
//! Claude Code deletes them after `cleanupPeriodDays`; nothing else backed the index up, and a
//! damaged file meant a rebuild that could not bring those sessions back.
//!
//! A snapshot is a folder `<index dir>/snapshots/<UTC date-time>-<reason>-<index id>/` (then
//! `-<n>` for a second one in the same second) holding a clone of the index file and of its
//! `-wal`, named like the live pair (`index.db`, `index.db-wal`), so the folder opens as one
//! database. Reasons: `pre-migration` (taken by `db::migrate` before it upgrades the schema,
//! kept 7 days, one at a time), `weekly` (the app's idle tick, only after `PRAGMA quick_check`
//! passed, one at a time) and `manual` (`cal backup`, never removed automatically). `index.db`
//! and the dev build's `index.dev.db` share a folder, so a snapshot lists for the index whose
//! file name it holds.
//!
//! Lineage: an index gets an identity (`backup.index_id` in its `app_config`, 8 hex digits)
//! the first time an automatic snapshot of it is about to be taken. It is committed before the
//! clone, so every snapshot carries it and a restored copy has the identity of the snapshot it
//! came from; the folder name holds it too (`cal backup`, which never writes to the index, names
//! its folder with it only when the index already has one). Every automatic removal (retention,
//! a rewrite, the vector partition, an upgrade that went through) acts only on automatic
//! snapshots whose identity is the live index's. An index built after the damaged one was
//! moved away (the startup screen's "start fresh") has another identity, or none yet, so the
//! snapshots holding the old history stay until the user deletes them: they still list, count
//! in Settings and `cal doctor`, and can be restored. Folders named before identities existed
//! (`<stamp>-<reason>`) list too, and are never removed automatically.
//!
//! Consistency without a checkpoint: the clone is taken under `BEGIN IMMEDIATE` on a
//! connection of its own. A held write lock means no commit, so nobody can reset or truncate
//! the WAL (RESTART and TRUNCATE checkpoints wait for writers); a PASSIVE checkpoint another
//! connection runs meanwhile only copies frames into `index.db` that stay in the WAL, which is
//! cloned after it, so every page the clone of `index.db` might have caught mid-change is
//! overridden by its frame. Before a snapshot counts, the copy is opened (its own `-shm` is
//! rebuilt, with checkpoint-on-close off so the clone is not written to) and its
//! `user_version` and thread count must match the live ones read under the lock; otherwise
//! the folder is removed.
//!
//! Cheap clones only, for automatic snapshots: `clonefile(2)` on macOS (APFS). A clone is
//! free when taken and costs real disk as the live index changes, up to the index's size.
//! Measured on copies of the author's 5.3 GB index (`ATTR_CMNEXT_PRIVATESIZE`, the bytes a
//! snapshot alone holds): taking one costs about 10 ms; after the one-time 0036 vector
//! rebuild (324,715 vectors, a 627 MB table) the snapshot taken before it held 649 MB of the
//! index file plus its 38 MB WAL, about 13% of the index, which is why that rebuild drops it;
//! and 32 minutes of live use (one busy agent session being indexed) rewrote 10.3 MB of the
//! live file's pages. A whole day was not measured; at that pace a busy day rewrites a few
//! hundred MB, and a page rewritten again costs nothing more. A snapshot's WAL clone grows
//! by at most the WAL's own size (`journal_size_limit`, 64 MB). Linux reflinks and Windows block clones are not used
//! yet (they cannot be tested here): there, and wherever a clone fails, automatic snapshots
//! are not taken (one log line), and `cal backup` makes a full copy through SQLite's online
//! backup API instead. When the weekly run finds that the index's disk cannot clone (exFAT,
//! HFS+), it records so (`backup.no_clone_at`) until a weekly snapshot is taken, and Settings
//! and `cal doctor` say that snapshots are not automatic there.
//!
//! Disk: nothing is ever deleted to make room. No automatic snapshot is taken while the
//! index's volume has less free space than the index itself, except a weekly one when the
//! index already has a weekly snapshot: the new clone is free when taken, and retention then
//! drops the old one once the new one is verified, which gives back what the old one held.
//! Shrink index and Reclaim space (a VACUUM moves every page) drop the index's automatic
//! snapshots, and the 0036 vector partition (which rewrites about twice the vector table)
//! drops its pre-migration one.
//!
//! What this module deletes or moves, and nothing else: inside `<index dir>/snapshots/` (a
//! real folder, never a symbolic link) only folders whose name it gives them, and in those
//! only the files named after the index (`<name>`, `-wal`, `-shm`, `-journal`), then the
//! folder itself if that left it empty; the two one-byte files of its clone probe
//! (`.clone-probe-<pid>` and `-copy`), which it creates there itself; and next to the index
//! the regular files `<name>.restoring-<UTC date-time>` and their `-wal` and `-shm`, the staged
//! copy only a restore makes, which an interrupted restore leaves behind (the next restore
//! removes them, and the app's minute tick once they are a day old). A restore moves the
//! live index's own three files aside (`<name>.damaged-<UTC date-time>`, its `-wal` and
//! `-shm`, never deleted) and puts a staged copy of the snapshot in their place. Snapshots
//! hold the same unredacted text as the index: their folders are 0700 and their files 0600
//! on Unix. In the index itself it writes only the `backup.*` keys of `app_config`.
//!
//! Trust: only a weekly snapshot was checked (`PRAGMA quick_check` on the index first); a
//! pre-migration or manual one is a copy of whatever the file held, damage included. So a
//! pre-migration snapshot replaces the older one only once the upgrade went through (one that
//! failed is removed: the index is still as it holds it), and is not offered by the launch
//! that took it; while the last integrity check had failed, the weekly snapshot outlives every
//! rewrite until a check passes; and a restore checks the copy it is about to put in place,
//! falling back to the next older snapshot when it fails.

use crate::db;
use crate::knowledge::app_config;
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The snapshots folder, next to the index.
pub const SNAPSHOTS_DIR: &str = "snapshots";
/// `app_config`: unix time of the last weekly snapshot.
pub const LAST_AT_KEY: &str = "backup.last_at";
/// `app_config`: the last weekly integrity check, as JSON ([`CheckRecord`]).
pub const LAST_CHECK_KEY: &str = "backup.last_check";
/// `app_config`: the index's identity ([`index_id`]), 8 lowercase hex digits.
pub const ID_KEY: &str = "backup.index_id";
/// `app_config`: unix time the weekly run found that the index's disk cannot clone files;
/// removed when a weekly snapshot is taken.
pub const NO_CLONE_KEY: &str = "backup.no_clone_at";
const ID_LEN: usize = 8;

/// How long a pre-migration snapshot is kept: long enough to notice a bad upgrade, short
/// enough that its clone does not grow toward a full copy as the index changes.
const PRE_MIGRATION_KEEP_SECS: i64 = 7 * 86_400;
/// How often the app takes a weekly snapshot.
const WEEKLY_EVERY_SECS: i64 = 7 * 86_400;
/// After an integrity check, how long until the next one may run: a failed check or a
/// snapshot that could not be taken retries daily, not on every minute tick (the check reads
/// the whole file: 65.6 s cold, 27.9 s warm, on a 5.3 GB index).
const CHECK_RETRY_SECS: i64 = 86_400;
/// How far ahead of the clock a stored time may be and still count as written now ([`expired`]).
const CLOCK_SLACK_SECS: i64 = 3_600;
/// A staged restore copy this old is left over from an interrupted restore: a running restore
/// holds `rewrite.lock` for minutes (a minute-long integrity check), never a day.
const STAGED_STALE_SECS: i64 = 86_400;
/// Folder-name time stamp, UTC.
const STAMP: &str = "%Y%m%dT%H%M%SZ";
const STAMP_LEN: usize = 16;
/// A full copy needs the index's size free on the target volume plus this share.
const FULL_COPY_HEADROOM_PERCENT: u64 = 10;
/// Pages per online-backup step: 4 MiB at the 4 KiB page size, so progress moves often.
const BACKUP_STEP_PAGES: i32 = 1024;
/// How long the weekly snapshot waits for the write lock before trying another day (short in
/// tests, which hold the lock on purpose).
const WEEKLY_LOCK_WAIT: Duration = if cfg!(test) {
    Duration::from_millis(300)
} else {
    Duration::from_secs(5)
};

/// The pre-migration snapshots this process's own upgrades took (canonical folder paths). A
/// launch that ends on the damaged-index screen took that copy minutes earlier from the very
/// file it then failed to read, so [`restore_candidates`] leaves it out.
static TAKEN_BY_UPGRADE: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Per index (canonical path), when this process last started a weekly run. The check record
/// in `app_config` is what normally spaces runs a day apart, but writing it needs the write
/// lock, which another process (`cal embed`, `cal index`, the 0036 rebuild) can hold for longer
/// than [`WEEKLY_LOCK_WAIT`]; without this the minute tick would start the whole-file check
/// again a minute later, and keep doing so while that process works.
static WEEKLY_TRIED: Mutex<Vec<(PathBuf, i64)>> = Mutex::new(Vec::new());

/// Per index, the duration of a weekly integrity check this process saw fail and no check has
/// passed since. The failed record in `app_config` normally carries that state, but writing it
/// needs the write lock, which another process can hold past [`WEEKLY_LOCK_WAIT`]; without this
/// the next Shrink index would drop the only checked copy ([`after_rewrite`]).
static CHECK_FAILED: Mutex<Vec<(PathBuf, u64)>> = Mutex::new(Vec::new());

/// `path` resolved (symbolic links, `..`), else as given: SQLite reports a connection's path
/// resolved, the app and `cal` pass `db::default_index_path()` as configured.
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn per_index<T: Copy>(slot: &Mutex<Vec<(PathBuf, T)>>, index: &Path) -> Option<T> {
    let key = canonical(index);
    let entries = slot.lock().ok()?;
    entries.iter().find(|(p, _)| *p == key).map(|(_, v)| *v)
}

fn set_per_index<T>(slot: &Mutex<Vec<(PathBuf, T)>>, index: &Path, value: Option<T>) {
    let key = canonical(index);
    if let Ok(mut entries) = slot.lock() {
        entries.retain(|(p, _)| *p != key);
        if let Some(v) = value {
            entries.push((key, v));
        }
    }
}

fn weekly_tried(index: &Path) -> Option<i64> {
    per_index(&WEEKLY_TRIED, index)
}

fn set_weekly_tried(index: &Path, at: Option<i64>) {
    set_per_index(&WEEKLY_TRIED, index, at);
}

/// Whether a time stored at `t` is `span` old at `now`, or lies more than [`CLOCK_SLACK_SECS`]
/// ahead of it: a stamp written while the clock ran ahead (set by hand, a bad sync) would
/// otherwise stop the weekly snapshot and every expiry until real time caught up, months later.
fn expired(t: i64, now: i64, span: i64) -> bool {
    t > now + CLOCK_SLACK_SECS || now - t >= span
}

/// Why a snapshot was taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reason {
    PreMigration,
    Weekly,
    Manual,
}

impl Reason {
    const ALL: [Reason; 3] = [Reason::PreMigration, Reason::Weekly, Reason::Manual];

    pub fn as_str(self) -> &'static str {
        match self {
            Reason::PreMigration => "pre-migration",
            Reason::Weekly => "weekly",
            Reason::Manual => "manual",
        }
    }

    /// Taken and removed by Callimachus itself; `manual` ones are the user's.
    fn automatic(self) -> bool {
        self != Reason::Manual
    }
}

/// One snapshot folder of an index.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    #[serde(skip)]
    pub dir: PathBuf,
    /// The folder's name, `<UTC date-time>-<reason>[-<index id>][-<n>]`.
    pub name: String,
    /// Unix time it was taken (from the name).
    pub taken_at: i64,
    pub reason: Reason,
    /// The identity of the index it was taken from (from the name, [`index_id`]); None for a
    /// manual one of an index that had none yet, or a folder named before identities existed.
    #[serde(skip)]
    pub lineage: Option<String>,
    /// The size of its files.
    pub bytes: u64,
    /// What it costs on disk now: the bytes no other file shares (an APFS clone starts at 0
    /// and grows as the live index changes). None where the system does not say.
    pub private_bytes: Option<u64>,
    /// An automatic snapshot that is not the live index's (an earlier index's, after a fresh
    /// start, or one without an identity): never removed automatically. Set by [`status`] only.
    pub other_index: bool,
}

/// The last weekly integrity check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckRecord {
    pub at: i64,
    pub ok: bool,
    pub ms: u64,
}

/// What Settings shows about the snapshots of the index.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    /// Automatic snapshots are taken here: this system has cheap clones, and the weekly run
    /// has not found that the index's disk cannot clone ([`NO_CLONE_KEY`]).
    pub automatic: bool,
    /// The snapshots folder, the home directory shown as `~`.
    pub folder: String,
    /// Newest first.
    pub snapshots: Vec<Snapshot>,
    /// What they cost on disk together, when the system reports it.
    pub bytes_on_disk: Option<u64>,
    /// `at` is 0 for a failed check whose result was carried across a Shrink index or
    /// Reclaim space rewrite ([`after_rewrite`]): no date to show.
    pub last_check: Option<CheckRecord>,
}

/// Whether this system can make a snapshot for free. A volume that is not APFS still fails
/// the clone itself; this only rules out systems without the call.
pub fn cheap_clones() -> bool {
    cfg!(target_os = "macos")
}

/// `<index dir>/snapshots`.
pub fn snapshots_dir(index: &Path) -> PathBuf {
    index
        .parent()
        .map_or_else(|| PathBuf::from(SNAPSHOTS_DIR), |d| d.join(SNAPSHOTS_DIR))
}

/// `<file name of index><suffix>`, e.g. `index.db-wal`.
fn with_suffix(name: &OsStr, suffix: &str) -> OsString {
    let mut s = name.to_os_string();
    s.push(suffix);
    s
}

/// `index` with `suffix` appended to its file name, in the same folder.
fn sibling(index: &Path, suffix: &str) -> PathBuf {
    let mut s = index.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

fn index_name(index: &Path) -> Result<&OsStr> {
    index
        .file_name()
        .with_context(|| format!("{} names no file", index.display()))
}

/// A folder, not a symbolic link to one.
fn is_real_dir(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())
}

/// A regular file, not a symbolic link to one.
fn is_real_file(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.is_file())
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn stamp(at: i64) -> String {
    chrono::DateTime::from_timestamp(at, 0)
        .unwrap_or_default()
        .format(STAMP)
        .to_string()
}

/// "November 9, 2026 at 14:30", local time, for the startup screen.
pub fn display_date(at: i64) -> String {
    chrono::DateTime::from_timestamp(at, 0)
        .unwrap_or_default()
        .with_timezone(&chrono::Local)
        .format("%B %-d, %Y at %H:%M")
        .to_string()
}

/// An index identity as this module writes it: 8 lowercase hex digits.
fn valid_id(s: &str) -> bool {
    s.len() == ID_LEN && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The identity of the index `conn` is open on ([`ID_KEY`]); None while it has none (no
/// automatic snapshot of it was ever about to be taken) or it cannot be read. Read-only.
pub fn index_id(conn: &Connection) -> Option<String> {
    app_config::get(conn, ID_KEY)
        .ok()
        .flatten()
        .filter(|v| valid_id(v))
}

/// [`index_id`] of the index at `index`, on a read-only connection of its own, for the callers
/// that hold none (`cal backup`, `cal index`).
fn stored_index_id(index: &Path) -> Option<String> {
    db::open_readonly(index).ok().and_then(|c| index_id(&c))
}

/// The identity of the index `conn` is open on, created when it has none (or an invalid one).
/// Only in autocommit, never inside the snapshot's own `BEGIN IMMEDIATE ... ROLLBACK`: it must
/// be committed for the clone taken next to carry it, so that a restored copy has the identity
/// of the snapshot it came from and of that snapshot's siblings. Insert-if-absent, then read
/// back: two processes creating it at once end with the same value.
fn ensure_index_id(conn: &Connection) -> Result<String> {
    if !conn.is_autocommit() {
        bail!("the index's identity is written outside a transaction, so the snapshot carries it");
    }
    let fresh = || uuid::Uuid::new_v4().simple().to_string()[..ID_LEN].to_string();
    conn.execute(
        "INSERT INTO app_config (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO NOTHING",
        params![ID_KEY, fresh()],
    )?;
    let stored = app_config::get(conn, ID_KEY)?.unwrap_or_default();
    if valid_id(&stored) {
        return Ok(stored);
    }
    // Replaced only while it still holds the same invalid value, so a second process doing
    // the same at once reads back the first one's replacement.
    conn.execute(
        "UPDATE app_config SET value = ?2 WHERE key = ?1 AND value = ?3",
        params![ID_KEY, fresh(), stored],
    )?;
    let stored = app_config::get(conn, ID_KEY)?.unwrap_or_default();
    if !valid_id(&stored) {
        bail!("the index's identity in app_config is not valid ({stored:?})");
    }
    Ok(stored)
}

/// What a folder name this module made says: `<stamp>-<reason>`, then `-<index id>` (8
/// lowercase hex digits) when the index had an identity, then `-<n>` for a second snapshot in
/// the same second. The time, the reason and the identity; None for any other name.
fn parse_name(name: &str) -> Option<(i64, Reason, Option<String>)> {
    let at = chrono::NaiveDateTime::parse_from_str(name.get(..STAMP_LEN)?, STAMP)
        .ok()?
        .and_utc()
        .timestamp();
    let rest = name.get(STAMP_LEN..)?.strip_prefix('-')?;
    Reason::ALL.into_iter().find_map(|r| {
        let tail = rest.strip_prefix(r.as_str())?;
        let (lineage, tail) = match tail.strip_prefix('-') {
            Some(t)
                if t.get(..ID_LEN).is_some_and(valid_id)
                    && matches!(t.as_bytes().get(ID_LEN), None | Some(b'-')) =>
            {
                (Some(t[..ID_LEN].to_string()), &t[ID_LEN..])
            }
            _ => (None, tail),
        };
        let numbered = |n: &str| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit());
        (tail.is_empty() || tail.strip_prefix('-').is_some_and(numbered))
            .then_some((at, r, lineage))
    })
}

/// The snapshots of `index`, newest first: folders under [`snapshots_dir`] whose name this
/// module gives and that hold a regular file named like the index. Symbolic links are never
/// followed, the snapshots folder's own included.
pub fn list(index: &Path) -> Vec<Snapshot> {
    let Some(name) = index.file_name() else {
        return Vec::new();
    };
    let root = snapshots_dir(index);
    if !is_real_dir(&root) {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out: Vec<Snapshot> = entries
        .flatten()
        .filter_map(|e| {
            let folder = e.file_name().into_string().ok()?;
            let (taken_at, reason, lineage) = parse_name(&folder)?;
            let dir = e.path();
            if !is_real_dir(&dir) || !is_real_file(&dir.join(name)) {
                return None;
            }
            let files = snapshot_files(&dir, name);
            let bytes = files
                .iter()
                .filter_map(|f| std::fs::symlink_metadata(f).ok())
                .map(|m| m.len())
                .sum();
            let private_bytes = files
                .iter()
                .map(|f| clone::private_size(f))
                .sum::<Option<u64>>();
            Some(Snapshot {
                dir,
                name: folder,
                taken_at,
                reason,
                lineage,
                bytes,
                private_bytes,
                other_index: false,
            })
        })
        .collect();
    out.sort_by(|a, b| (b.taken_at, &b.name).cmp(&(a.taken_at, &a.name)));
    out
}

/// The regular files of a snapshot folder that belong to the index `name`.
fn snapshot_files(dir: &Path, name: &OsStr) -> Vec<PathBuf> {
    ["", "-wal"]
        .iter()
        .map(|s| dir.join(with_suffix(name, s)))
        .filter(|p| is_real_file(p))
        .collect()
}

/// Remove one snapshot folder this module made: only the files named after the index, then
/// the folder if that emptied it. Anything else in it (a file the user put there, a
/// symbolic link) stays, and so does the folder.
fn remove_snapshot_dir(dir: &Path, name: &OsStr) -> bool {
    if !is_real_dir(dir) {
        return false;
    }
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let file = dir.join(with_suffix(name, suffix));
        if is_real_file(&file) {
            let _ = std::fs::remove_file(&file);
        }
    }
    std::fs::remove_dir(dir).is_ok()
}

/// Remove the snapshot folders in `snaps` (of `index`); how many went.
fn drop_listed<'a>(index: &Path, snaps: impl IntoIterator<Item = &'a Snapshot>) -> usize {
    let Some(name) = index.file_name() else {
        return 0;
    };
    snaps
        .into_iter()
        .filter(|s| remove_snapshot_dir(&s.dir, name))
        .count()
}

/// Remove the snapshots of `index` that `which` selects; how many folders went.
fn drop_where(index: &Path, which: impl Fn(&Snapshot) -> bool) -> usize {
    let snaps = list(index);
    drop_listed(index, snaps.iter().filter(|s| which(s)))
}

/// An automatic snapshot of the index whose identity is `id`: the only kind any automatic
/// removal touches. An index without an identity owns none, so it removes nothing.
fn owned(s: &Snapshot, id: Option<&str>) -> bool {
    s.reason.automatic() && id.is_some() && s.lineage.as_deref() == id
}

/// Retention among the automatic snapshots of the index whose identity is `id`: the newest
/// weekly one is kept, a pre-migration one until it is 7 days old ([`expired`]), and nothing
/// else; manual snapshots and other indexes' are never touched. A weekly snapshot stamped more
/// than [`CLOCK_SLACK_SECS`] ahead of the clock is kept only while there is no other. Returns
/// how many folders were removed.
pub fn prune(index: &Path, id: Option<&str>, now: i64) -> usize {
    let snaps = list(index);
    let mut weeklies = snaps
        .iter()
        .filter(|s| s.reason == Reason::Weekly && owned(s, id));
    let keep_weekly = weeklies
        .clone()
        .find(|s| s.taken_at <= now + CLOCK_SLACK_SECS)
        .or_else(|| weeklies.next())
        .map(|s| s.name.clone());
    // A pre-migration one goes by age only, never for not being the newest: an upgrade in
    // another process may have just taken a newer one whose outcome is not known yet
    // ([`pre_migration_done`] keeps one at a time).
    drop_listed(
        index,
        snaps.iter().filter(|s| {
            owned(s, id)
                && match s.reason {
                    Reason::Manual => false,
                    Reason::Weekly => keep_weekly.as_deref() != Some(s.name.as_str()),
                    Reason::PreMigration => expired(s.taken_at, now, PRE_MIGRATION_KEEP_SECS),
                }
        }),
    )
}

/// [`prune`] for a caller without a connection to the index (`cal backup`, `cal index`): the
/// identity is read on a read-only connection; when it cannot be, nothing is removed.
pub fn prune_read_only(index: &Path, now: i64) -> usize {
    prune(index, stored_index_id(index).as_deref(), now)
}

/// After a whole-file rewrite (Shrink index / Reclaim space) on `conn`: drop the index's
/// automatic snapshots, and clear the weekly state so the next idle tick checks the rewritten
/// file and takes a fresh one. Manual snapshots are the user's and stay (their cost shows in
/// Settings), and so do other indexes'.
///
/// One exception: while the last integrity check failed (its record says so, or this process
/// saw it fail and could not write that record), the weekly snapshot stays. It is the only copy
/// that passed a check, and a VACUUM that got through says only that every page it read
/// parsed, not that the rewritten file holds what the damaged one lost. It now costs a full
/// copy on disk, until a check of the rewritten file passes and that run's snapshot replaces
/// it ([`prune`]). The failed record is then rewritten with `at` 0 instead of removed: the next
/// tick checks at once, and a second rewrite before that check still keeps the snapshot.
pub(crate) fn after_rewrite(conn: &Connection, index: &Path) {
    let id = index_id(conn);
    let failed_ms = last_check(conn)
        .filter(|c| !c.ok)
        .map(|c| c.ms)
        .or_else(|| per_index(&CHECK_FAILED, index));
    let keep_weekly = failed_ms.is_some();
    let dropped = drop_where(index, |s| {
        owned(s, id.as_deref()) && !(keep_weekly && s.reason == Reason::Weekly)
    });
    if dropped > 0 {
        app_log!("[backup] dropped {dropped} automatic snapshot(s): the rewrite moved every page");
    }
    match failed_ms {
        Some(ms) => {
            app_log!(
                "[backup] kept the weekly snapshot through the rewrite: the last integrity check \
                 had found problems"
            );
            let rec = CheckRecord {
                at: 0,
                ok: false,
                ms,
            };
            if let Ok(json) = serde_json::to_string(&rec) {
                if let Err(e) = app_config::put(conn, LAST_CHECK_KEY, &json) {
                    app_log!("[backup] could not record the failed integrity check: {e:#}");
                }
            }
        }
        None => {
            let _ = app_config::delete(conn, LAST_CHECK_KEY);
        }
    }
    let _ = app_config::delete(conn, LAST_AT_KEY);
    set_weekly_tried(index, None);
}

/// After the one-time 0036 vector partition succeeded on `conn`: the pre-migration snapshot of
/// the index has served its purpose, and the rebuild made it cost about twice the vector table.
pub fn after_partition(conn: &Connection, index: &Path) {
    let id = index_id(conn);
    let dropped = drop_where(index, |s| {
        s.reason == Reason::PreMigration && owned(s, id.as_deref())
    });
    if dropped > 0 {
        app_log!("[backup] dropped the pre-upgrade snapshot: the vector rebuild succeeded");
    }
}

/// Create (owner-only) or tighten the snapshots folder of `index`, refusing one that is a
/// symbolic link or a file.
fn ensure_snapshots_root(index: &Path) -> Result<PathBuf> {
    let root = snapshots_dir(index);
    match std::fs::symlink_metadata(&root) {
        Ok(m) if m.is_dir() => db::tighten_mode(&root),
        Ok(_) => bail!(
            "{} is not a folder (a symbolic link or a file); leaving it alone",
            root.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            db::create_private_dir(&root).with_context(|| format!("creating {}", root.display()))?
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", root.display())),
    }
    Ok(root)
}

/// A new, empty, owner-only folder `<root>/<stamp>-<reason>[-<index id>]` (a numbered one when
/// that name is taken).
fn new_snapshot_dir(
    root: &Path,
    reason: Reason,
    at: i64,
    lineage: Option<&str>,
) -> Result<PathBuf> {
    let base = match lineage {
        Some(id) => format!("{}-{}-{id}", stamp(at), reason.as_str()),
        None => format!("{}-{}", stamp(at), reason.as_str()),
    };
    for n in 1..=9 {
        let name = if n == 1 {
            base.clone()
        } else {
            format!("{base}-{n}")
        };
        let dir = root.join(name);
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("creating {}", dir.display())),
        }
    }
    bail!("too many snapshots named {base} in {}", root.display())
}

/// Refuse an index whose file, `-wal` or `-shm` is a symbolic link: a clone or a rename
/// would act on the link, not on what the index really is.
fn refuse_symlinks(index: &Path) -> Result<()> {
    for p in [
        index.to_path_buf(),
        sibling(index, "-wal"),
        sibling(index, "-shm"),
    ] {
        if std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink()) {
            bail!(
                "{} is a symbolic link; snapshots and restores leave such an index alone",
                p.display()
            );
        }
    }
    Ok(())
}

/// What a copy must show to count as the index it was taken from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Expect {
    user_version: i64,
    threads: i64,
}

fn read_expect(conn: &Connection) -> Result<Expect> {
    let user_version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let threads = if db::has_table(conn, "threads") {
        conn.query_row("SELECT count(*) FROM threads", [], |r| r.get(0))?
    } else {
        0
    };
    Ok(Expect {
        user_version,
        threads,
    })
}

/// Open a copy (a snapshot, a staged restore) without ever writing to it: no CREATE, queries
/// only, and no checkpoint when it closes, which would fold the copied WAL into the copied
/// file and make an APFS clone diverge from the index it shares pages with.
fn open_copy(db_path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("opening {}", db_path.display()))?;
    conn.set_db_config(
        rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
        true,
    )?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "query_only", "ON")?;
    Ok(conn)
}

/// [`open_copy`], read `Expect`, close, and remove the `-shm` the open made.
fn read_copy(db_path: &Path) -> Result<Expect> {
    let got = {
        let conn = open_copy(db_path)?;
        read_expect(&conn)?
    };
    let shm = sibling(db_path, "-shm");
    if is_real_file(&shm) {
        let _ = std::fs::remove_file(shm);
    }
    Ok(got)
}

/// A clone taken under the write lock, not yet verified.
pub(crate) struct Taken {
    index: PathBuf,
    dir: PathBuf,
    name: OsString,
    expect: Expect,
    taken_at: i64,
    reason: Reason,
    lineage: Option<String>,
}

impl Taken {
    /// Open the copy and compare; remove the folder when it does not match.
    fn verify(self) -> Result<Snapshot> {
        let db_path = self.dir.join(&self.name);
        let checked = read_copy(&db_path).and_then(|got| {
            if got == self.expect {
                Ok(())
            } else {
                bail!(
                    "the copy reads as schema {} with {} threads, not {} with {}",
                    got.user_version,
                    got.threads,
                    self.expect.user_version,
                    self.expect.threads
                )
            }
        });
        if let Err(e) = checked {
            remove_snapshot_dir(&self.dir, &self.name);
            return Err(e.context("the snapshot did not open as the index it was taken from"));
        }
        let bytes = snapshot_files(&self.dir, &self.name)
            .iter()
            .filter_map(|f| std::fs::metadata(f).ok())
            .map(|m| m.len())
            .sum();
        Ok(Snapshot {
            name: self
                .dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            dir: self.dir,
            taken_at: self.taken_at,
            reason: self.reason,
            lineage: self.lineage,
            bytes,
            private_bytes: Some(0),
            other_index: false,
        })
    }
}

/// Clone `index` and its `-wal` into a new snapshot folder, named with the index's identity
/// `lineage`. The caller holds a write transaction on `conn`, a connection to `index`: see the
/// module docs for why that makes the pair consistent.
fn take_locked(
    conn: &Connection,
    index: &Path,
    reason: Reason,
    at: i64,
    lineage: Option<&str>,
) -> Result<Taken> {
    refuse_symlinks(index)?;
    let name = index_name(index)?.to_os_string();
    let expect = read_expect(conn)?;
    let root = ensure_snapshots_root(index)?;
    let dir = new_snapshot_dir(&root, reason, at, lineage)?;
    let cloned = (|| -> std::io::Result<()> {
        clone::clone_file(index, &dir.join(&name))?;
        let wal = sibling(index, "-wal");
        if is_real_file(&wal) {
            clone::clone_file(&wal, &dir.join(with_suffix(&name, "-wal")))?;
        }
        for f in snapshot_files(&dir, &name) {
            db::tighten_mode(&f);
        }
        Ok(())
    })();
    if let Err(e) = cloned {
        remove_snapshot_dir(&dir, &name);
        let what = if clone::is_no_clone(&e) {
            "this disk cannot clone files (automatic snapshots need APFS on macOS)"
        } else {
            "cloning the index"
        };
        return Err(anyhow::Error::from(e).context(what));
    }
    Ok(Taken {
        index: index.to_path_buf(),
        dir,
        name,
        expect,
        taken_at: at,
        reason,
        lineage: lineage.map(str::to_string),
    })
}

/// A writable connection to the live index that never creates it and never migrates it.
fn open_live(index: &Path, busy: Duration) -> Result<Connection> {
    if !index.exists() {
        bail!("no index at {}", index.display());
    }
    refuse_interrupted_restore(index)?;
    let conn = Connection::open_with_flags(
        index,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("opening {}", index.display()))?;
    // No checkpoint when it closes: as the index's only connection (`cal backup` with the app
    // closed), SQLite would fold the WAL into the index file and delete it, rewriting pages
    // the clone just taken shares (up to the whole WAL, 38 MB measured) for nothing. The WAL
    // stays for the next connection to use.
    conn.set_db_config(
        rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
        true,
    )?;
    conn.busy_timeout(busy)?;
    Ok(conn)
}

/// Take a snapshot of `index` now, on a connection of its own, waiting at most `busy` for the
/// write lock, named with the index's identity when it has one (never writing one). Fails
/// where files cannot be cloned.
pub fn snapshot(index: &Path, reason: Reason, busy: Duration) -> Result<Snapshot> {
    // Before opening: a connection through the link would checkpoint the file it points at.
    refuse_symlinks(index)?;
    let conn = open_live(index, busy)?;
    let lineage = index_id(&conn);
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let taken = take_locked(&conn, index, reason, now(), lineage.as_deref());
    conn.execute_batch("ROLLBACK;")?;
    taken?.verify()
}

/// Whether an automatic snapshot (`reason`) of the index whose identity is `id` may be taken,
/// given the free space on its volume. Nothing is ever deleted to make room: on a disk
/// shorter than the index, dropping clones gives back only their private bytes, and dropping
/// them left the user with no snapshot at all. Yes when free space is unknown (the clone
/// itself costs nothing) or at least the index's size; also yes for a weekly snapshot while
/// the index already has one: the new one is free when taken, and [`prune`] then drops the
/// old one, giving back what it held.
fn room_for(index: &Path, reason: Reason, id: &str, free: &dyn Fn(&Path) -> Option<u64>) -> bool {
    let dir = index.parent().unwrap_or(Path::new("."));
    match free(dir) {
        None => true,
        Some(f) if f >= index_size(index) => true,
        Some(_) => {
            reason == Reason::Weekly
                && list(index)
                    .iter()
                    .any(|s| s.reason == Reason::Weekly && owned(s, Some(id)))
        }
    }
}

/// The index file plus its WAL, in bytes.
fn index_size(index: &Path) -> u64 {
    [index.to_path_buf(), sibling(index, "-wal")]
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum()
}

/// An automatic snapshot under the caller's write lock, of the index whose identity is `id`:
/// only where files clone for free and the volume has room ([`room_for`]).
fn auto_take_locked(
    conn: &Connection,
    index: &Path,
    reason: Reason,
    at: i64,
    id: &str,
    free: &dyn Fn(&Path) -> Option<u64>,
) -> Result<Taken> {
    if !cheap_clones() {
        bail!("this system has no free file clones; `cal backup` makes a full copy");
    }
    refuse_symlinks(index)?;
    if !room_for(index, reason, id, free) {
        bail!("less free disk space than the index's own size");
    }
    take_locked(conn, index, reason, at, Some(id))
}

#[cfg(test)]
thread_local! {
    /// Tests migrate old-schema indexes in the shared temp dir all the time; only the ones
    /// that test snapshots turn automatic snapshots on, for their own thread.
    static AUTOMATIC_IN_TESTS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn automatic_enabled() -> bool {
    #[cfg(test)]
    {
        AUTOMATIC_IN_TESTS.with(std::cell::Cell::get)
    }
    #[cfg(not(test))]
    {
        true
    }
}

/// `db::migrate`'s first step, before its write lock, when a snapshot would be attempted at
/// all (automatic snapshots on, cheap clones here, a file-backed index that holds something:
/// a new one is at version 0): the index's identity, created if needed and committed so the
/// snapshot carries it ([`ensure_index_id`]). None skips the snapshot: an index older than
/// the `app_config` table, or a write lock that never freed, costs one log line and never the
/// upgrade.
pub(crate) fn pre_migration_prepare(conn: &Connection) -> Option<String> {
    if !automatic_enabled() || !cheap_clones() {
        return None;
    }
    conn.path().filter(|p| !p.is_empty())?;
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .ok()?;
    if version <= 0 {
        return None;
    }
    match ensure_index_id(conn) {
        Ok(id) => Some(id),
        Err(e) => {
            app_log!(
                "[backup] no snapshot before the schema upgrade: recording the index's identity \
                 failed: {e:#}"
            );
            None
        }
    }
}

/// `db::migrate`'s snapshot of the index whose identity is `id` ([`pre_migration_prepare`]),
/// called holding the write lock just before the upgrade. Only when a migration is still
/// pending (another process may have finished it while this one waited). Never fails the
/// upgrade: what goes wrong is one log line.
pub(crate) fn pre_migration_locked(conn: &Connection, id: &str) -> Option<Taken> {
    let index = PathBuf::from(conn.path().filter(|p| !p.is_empty())?);
    let pending = db::schema_pending(conn).ok()?;
    if pending <= 0 {
        return None;
    }
    match auto_take_locked(
        conn,
        &index,
        Reason::PreMigration,
        now(),
        id,
        &crate::cleanup::free_bytes,
    ) {
        Ok(taken) => Some(taken),
        Err(e) => {
            app_log!("[backup] no snapshot before the schema upgrade: {e:#}");
            None
        }
    }
}

/// A verified pre-migration snapshot, waiting for the upgrade's outcome
/// ([`pre_migration_done`]).
pub(crate) struct PreMigration {
    index: PathBuf,
    snapshot: Snapshot,
}

/// After the lock is released: verify the pre-migration snapshot. Older pre-migration
/// snapshots stay until [`pre_migration_done`] learns that the upgrade went through.
pub(crate) fn pre_migration_verify(taken: Option<Taken>) -> Option<PreMigration> {
    let taken = taken?;
    let index = taken.index.clone();
    match taken.verify() {
        Ok(snapshot) => Some(PreMigration { index, snapshot }),
        Err(e) => {
            app_log!("[backup] no snapshot before the schema upgrade: {e:#}");
            None
        }
    }
}

/// After the upgrade ran. When it went through, the new snapshot replaces this index's older
/// pre-migration ones: only those strictly older (by time, then name), never a newer or
/// same-second one, which another process upgrading at the same moment took and may still be
/// waiting on. When it failed, the index is still as the snapshot holds it (the upgrade is one
/// transaction), and the failure may be damage the copy shares (SQLITE_CORRUPT on a page the
/// verification never read): the copy is removed, so it neither replaces an older, sound
/// pre-migration snapshot nor becomes the restore the startup screen offers.
pub(crate) fn pre_migration_done(pre: Option<PreMigration>, upgraded: bool) {
    let Some(PreMigration { index, snapshot: s }) = pre else {
        return;
    };
    if upgraded {
        drop_where(&index, |o| {
            o.reason == Reason::PreMigration
                && owned(o, s.lineage.as_deref())
                && (o.taken_at, &o.name) < (s.taken_at, &s.name)
        });
        if let Ok(mut taken) = TAKEN_BY_UPGRADE.lock() {
            taken.push(canonical(&s.dir));
        }
        app_log!(
            "[backup] snapshot before the schema upgrade: {}",
            s.dir.display()
        );
    } else {
        if let Some(name) = index.file_name() {
            remove_snapshot_dir(&s.dir, name);
        }
        app_log!(
            "[backup] the schema upgrade failed: removed the snapshot taken just before it ({}), \
             a copy of the same file",
            s.name
        );
    }
}

/// The snapshots of `index` a restore may use, newest first: all of them except the ones this
/// process's own schema upgrade took ([`TAKEN_BY_UPGRADE`]).
pub fn restore_candidates(index: &Path) -> Vec<Snapshot> {
    let mine = TAKEN_BY_UPGRADE
        .lock()
        .map(|v| v.clone())
        .unwrap_or_default();
    list(index)
        .into_iter()
        .filter(|s| mine.is_empty() || !mine.contains(&canonical(&s.dir)))
        .collect()
}

// ---- the weekly snapshot -------------------------------------------------------------------

/// The integrity check in flight, so a Shrink index / Reclaim space can stop it: it holds a
/// read transaction for a minute, which keeps a VACUUM's closing checkpoint from finishing.
static CHECK: Mutex<Option<rusqlite::InterruptHandle>> = Mutex::new(None);

/// Stop a running weekly integrity check (it is retried later). A no-op when none runs.
pub fn interrupt_check() {
    if let Ok(slot) = CHECK.lock() {
        if let Some(handle) = slot.as_ref() {
            handle.interrupt();
        }
    }
}

fn last_check(conn: &Connection) -> Option<CheckRecord> {
    app_config::get(conn, LAST_CHECK_KEY)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok())
}

/// Whether the weekly snapshot is due at `now`: a week since the last one, and a day since
/// the last integrity check (so a failed check, or a snapshot the disk refused, is retried
/// daily) and since this process last tried (a run whose record could not be written, a
/// volume that cannot clone). A time stored ahead of the clock counts as past ([`expired`]).
/// Always false where files cannot be cloned for free.
pub fn weekly_due(conn: &Connection, now: i64) -> bool {
    if !cheap_clones() {
        return false;
    }
    let last_at = app_config::get(conn, LAST_AT_KEY)
        .ok()
        .flatten()
        .and_then(|v| v.parse::<i64>().ok());
    let week_passed = last_at.is_none_or(|t| expired(t, now, WEEKLY_EVERY_SECS));
    let check_allowed = last_check(conn).is_none_or(|c| expired(c.at, now, CHECK_RETRY_SECS));
    let tried = conn
        .path()
        .filter(|p| !p.is_empty())
        .and_then(|p| weekly_tried(Path::new(p)));
    let retry_allowed = tried.is_none_or(|t| expired(t, now, CHECK_RETRY_SECS));
    week_passed && check_allowed && retry_allowed
}

/// What one weekly run did.
#[derive(Debug, PartialEq, Eq)]
pub enum Weekly {
    Taken(String),
    /// `PRAGMA quick_check` found problems: every existing snapshot is kept, none is taken.
    CheckFailed,
    /// Stopped by [`interrupt_check`]; retried on a later tick.
    Interrupted,
    /// The snapshot could not be taken (see the log line).
    Skipped,
}

/// `PRAGMA quick_check` on `reader`, cancellable through [`interrupt_check`]. Ok(true) when
/// the file is sound.
fn quick_check(reader: &Connection) -> rusqlite::Result<bool> {
    if let Ok(mut slot) = CHECK.lock() {
        *slot = Some(reader.get_interrupt_handle());
    }
    let rows = reader
        .prepare("PRAGMA quick_check(5)")
        .and_then(|mut stmt| {
            stmt.query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<String>>>()
        });
    if let Ok(mut slot) = CHECK.lock() {
        *slot = None;
    }
    let rows = rows?;
    let ok = rows.len() == 1 && rows[0] == "ok";
    if !ok {
        app_log!("[backup] quick_check: {}", rows.join("; "));
    }
    Ok(ok)
}

/// The weekly snapshot, off the writer: `PRAGMA quick_check` on a read-only connection of
/// its own (about 66 s on a 5.3 GB index, so not on a pooled reader the UI needs), and only
/// when it passed, a snapshot under a short write lock, then retention. A failed check keeps
/// every existing snapshot and takes none. Blocking; the caller runs it on its own thread.
pub fn run_weekly(index: &Path, now: i64) -> Weekly {
    run_weekly_with(index, now, &crate::cleanup::free_bytes, &clone_probe)
}

/// Whether files clone on the volume of `index`'s snapshots folder: clone a one-byte file
/// there, then remove both. A few file operations, where learning it from the snapshot itself
/// comes after the integrity check (66 s cold on a 5.3 GB index), which a Mac whose index sits
/// on an exFAT or HFS+ drive would otherwise run every day for no snapshot.
fn clone_probe(index: &Path) -> std::io::Result<()> {
    let root = ensure_snapshots_root(index).map_err(std::io::Error::other)?;
    let pid = std::process::id();
    let src = root.join(format!(".clone-probe-{pid}"));
    let dst = root.join(format!(".clone-probe-{pid}-copy"));
    let remove = || {
        for p in [&src, &dst] {
            if is_real_file(p) {
                let _ = std::fs::remove_file(p);
            }
        }
    };
    remove();
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        std::io::Write::write_all(&mut options.open(&src)?, b"x")?;
        clone::clone_file(&src, &dst)
    })();
    remove();
    result
}

fn run_weekly_with(
    index: &Path,
    now: i64,
    free: &dyn Fn(&Path) -> Option<u64>,
    probe: &dyn Fn(&Path) -> std::io::Result<()>,
) -> Weekly {
    // Whatever happens below, the next run waits a day ([`WEEKLY_TRIED`]), unless this one is
    // interrupted.
    set_weekly_tried(index, Some(now));
    if let Err(e) = refuse_symlinks(index) {
        app_log!("[backup] no weekly snapshot: {e:#}");
        return Weekly::Skipped;
    }
    if let Err(e) = probe(index) {
        if clone::is_no_clone(&e) {
            app_log!(
                "[backup] no weekly snapshot: this disk cannot clone files (automatic snapshots \
                 need APFS on macOS); `cal backup --to DIR` makes a full copy"
            );
            match open_live(index, WEEKLY_LOCK_WAIT) {
                Ok(conn) => record_no_clone(&conn, now),
                Err(e) => app_log!("[backup] could not record that the disk cannot clone: {e:#}"),
            }
        } else {
            app_log!("[backup] no weekly snapshot: trying a clone failed: {e:#}");
        }
        return Weekly::Skipped;
    }
    let started = Instant::now();
    let checked = db::open_readonly(index).and_then(|r| Ok(quick_check(&r)?));
    let ms = started.elapsed().as_millis() as u64;
    let ok = match checked {
        Ok(ok) => ok,
        Err(e) if is_interrupt(&e) => {
            set_weekly_tried(index, None);
            return Weekly::Interrupted;
        }
        Err(e) => {
            app_log!("[backup] weekly integrity check could not run: {e:#}");
            false
        }
    };
    // Before the record, which can fail: a rewrite in this process keeps the weekly snapshot
    // while a check failed, whether or not the failure reached `app_config`.
    set_per_index(&CHECK_FAILED, index, (!ok).then_some(ms));
    let record = |conn: &Connection| {
        let rec = CheckRecord { at: now, ok, ms };
        if let Ok(json) = serde_json::to_string(&rec) {
            if let Err(e) = app_config::put(conn, LAST_CHECK_KEY, &json) {
                app_log!("[backup] could not record the integrity check: {e:#}");
            }
        }
    };
    let conn = match open_live(index, WEEKLY_LOCK_WAIT) {
        Ok(c) => c,
        Err(e) => {
            app_log!("[backup] weekly snapshot: {e:#}");
            return Weekly::Skipped;
        }
    };
    record(&conn);
    if !ok {
        app_log!(
            "[backup] the weekly integrity check found problems ({} ms): kept every snapshot, took none",
            ms
        );
        return Weekly::CheckFailed;
    }
    // In autocommit, before the snapshot's transaction: the clone must carry it.
    let id = match ensure_index_id(&conn) {
        Ok(id) => id,
        Err(e) => {
            app_log!("[backup] no weekly snapshot: recording the index's identity failed: {e:#}");
            return Weekly::Skipped;
        }
    };
    let taken = conn
        .execute_batch("BEGIN IMMEDIATE;")
        .map_err(anyhow::Error::from)
        .and_then(|()| {
            let t = auto_take_locked(&conn, index, Reason::Weekly, now, &id, free);
            conn.execute_batch("ROLLBACK;")?;
            t
        })
        .and_then(Taken::verify);
    match taken {
        Ok(s) => {
            let _ = app_config::put(&conn, LAST_AT_KEY, &now.to_string());
            let _ = app_config::delete(&conn, NO_CLONE_KEY);
            // Only now that the new one is verified: on a disk shorter than the index this is
            // what gives back the old one's blocks ([`room_for`]).
            prune(index, Some(&id), now);
            app_log!(
                "[backup] weekly snapshot {} (integrity check {} ms)",
                s.name,
                ms
            );
            Weekly::Taken(s.name)
        }
        Err(e) => {
            if no_clone_error(&e) {
                record_no_clone(&conn, now);
            }
            app_log!("[backup] no weekly snapshot: {e:#}");
            Weekly::Skipped
        }
    }
}

/// Remember that the index's disk cannot clone ([`NO_CLONE_KEY`]), so Settings and `cal doctor`
/// stop promising automatic snapshots there.
fn record_no_clone(conn: &Connection, now: i64) {
    if let Err(e) = app_config::put(conn, NO_CLONE_KEY, &now.to_string()) {
        app_log!("[backup] could not record that the disk cannot clone: {e:#}");
    }
}

/// The snapshot failed because the volume cannot clone (as opposed to a real error).
fn no_clone_error(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(clone::is_no_clone)
    })
}

fn is_interrupt(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<rusqlite::Error>()
            .and_then(rusqlite::Error::sqlite_error_code)
            == Some(rusqlite::ErrorCode::OperationInterrupted)
    })
}

/// What Settings shows. `conn` reads the index's identity (which marks the snapshots of
/// another index), the last check and whether the disk was found unable to clone (None skips
/// all three).
pub fn status(index: &Path, conn: Option<&Connection>) -> Status {
    let mut snapshots = list(index);
    if let Some(conn) = conn {
        let id = index_id(conn);
        for s in &mut snapshots {
            s.other_index = s.reason.automatic() && !owned(s, id.as_deref());
        }
    }
    let bytes_on_disk = snapshots
        .iter()
        .map(|s| s.private_bytes)
        .sum::<Option<u64>>();
    let no_clone = conn.is_some_and(|c| app_config::get(c, NO_CLONE_KEY).ok().flatten().is_some());
    Status {
        automatic: cheap_clones() && !no_clone,
        folder: home_as_tilde(&snapshots_dir(index)),
        snapshots,
        bytes_on_disk,
        last_check: conn.and_then(last_check),
    }
}

/// A regular file next to the index named `<name>.<tag>-<UTC date-time>`, or that name with
/// `-wal` or `-shm` after it (`sidecar`).
struct Stamped {
    path: PathBuf,
    at: i64,
    sidecar: bool,
    len: u64,
}

/// The files a restore leaves next to `index` under `tag` (`damaged`, `restoring`): regular
/// files only, never a folder, a symbolic link, another index's file or a name whose time
/// stamp does not parse. Read-only.
fn stamped_siblings(index: &Path, tag: &str) -> Vec<Stamped> {
    let Some(name) = index.file_name().and_then(OsStr::to_str) else {
        return Vec::new();
    };
    let dir = match index.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let prefix = format!("{name}.{tag}-");
    let stamped = |s: &str| {
        chrono::NaiveDateTime::parse_from_str(s, STAMP)
            .ok()
            .map(|t| t.and_utc().timestamp())
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let file = entry.file_name();
            let rest = file.to_str()?.strip_prefix(&prefix)?;
            let meta = std::fs::symlink_metadata(entry.path()).ok()?;
            if !meta.is_file() {
                return None;
            }
            let (at, sidecar) = match stamped(rest) {
                Some(at) => (at, false),
                None => (
                    rest.strip_suffix("-wal")
                        .or_else(|| rest.strip_suffix("-shm"))
                        .and_then(stamped)?,
                    true,
                ),
            };
            Some(Stamped {
                path: entry.path(),
                at,
                sidecar,
                len: meta.len(),
            })
        })
        .collect()
}

/// How many of `files` are copies (not sidecars), and the bytes all of them hold.
fn count_copies(files: &[Stamped]) -> (usize, u64) {
    (
        files.iter().filter(|f| !f.sidecar).count(),
        files.iter().map(|f| f.len).sum(),
    )
}

/// The damaged indexes that restores moved aside and nobody removed since
/// (`<name>.damaged-<UTC date-time>`, regular files only): how many, and the bytes they hold
/// together with their `-wal` and `-shm`. A restore never deletes them, so `cal doctor`
/// names what they still cost. Read-only.
pub fn kept_damaged(index: &Path) -> (usize, u64) {
    count_copies(&stamped_siblings(index, "damaged"))
}

/// The staged copies an interrupted restore left (`<name>.restoring-<UTC date-time>`, regular
/// files only): how many, and the bytes they hold with their `-wal` and `-shm`. Each can be a
/// full copy of the index, so `cal doctor` names them until the next restore or the app's
/// tick removes them ([`remove_stale_staged`]). Read-only.
pub fn staged_leftovers(index: &Path) -> (usize, u64) {
    count_copies(&stamped_siblings(index, "restoring"))
}

/// Remove what an interrupted restore staged next to `index` (`<name>.restoring-<UTC
/// date-time>`, its `-wal` and `-shm`, regular files whose stamp parses: only [`restore`]
/// makes these, and none is ever the live index) when `stale` accepts the stamp. How many
/// files went.
fn remove_staged_where(index: &Path, stale: impl Fn(i64) -> bool) -> usize {
    stamped_siblings(index, "restoring")
        .into_iter()
        .filter(|f| stale(f.at))
        .filter(|f| std::fs::remove_file(&f.path).is_ok())
        .count()
}

/// The app's minute tick: remove the staged copies interrupted restores left that are a day
/// old ([`STAGED_STALE_SECS`]); a younger one may belong to a restore still running in another
/// process. How many files went.
pub fn remove_stale_staged(index: &Path, now: i64) -> usize {
    let removed = remove_staged_where(index, |at| now - at >= STAGED_STALE_SECS);
    if removed > 0 {
        app_log!("[backup] removed {removed} file(s) an interrupted restore had staged");
    }
    removed
}

/// `path` with the home directory shown as `~`.
pub fn home_as_tilde(path: &Path) -> String {
    match dirs::home_dir().and_then(|h| path.strip_prefix(&h).ok().map(Path::to_path_buf)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

// ---- restore ---------------------------------------------------------------------------------

/// What [`restore`] did.
#[derive(Debug)]
pub struct Restored {
    /// The damaged index, moved aside (its `-wal` and `-shm` next to it with those suffixes).
    pub damaged: Option<PathBuf>,
    /// The snapshot now in place.
    pub snapshot: Snapshot,
    /// Newer snapshots tried first, from the one asked for, that could not be restored (one
    /// that failed its integrity check, say); newest first.
    pub skipped: Vec<Snapshot>,
}

/// Every lock a process that writes the index holds.
const WRITER_LOCKS: [&str; 5] = [
    crate::locks::GUI_LOCK,
    crate::locks::WATCH_LOCK,
    crate::locks::DISTILL_LOCK,
    crate::locks::INDEX_LOCK,
    crate::locks::EMBED_LOCK,
];

/// Put the snapshot `name` of `index` in place of the index, for a damaged index (the app's
/// startup screen). Refuses while another process holds any lock next to the index or has it
/// open (`lsof`; where nothing can tell, it refuses too, except on Windows, where the renames
/// themselves fail under an open handle), and holds `rewrite.lock` meanwhile so `cal index`,
/// `watch`, `embed` and `distill --auto` do not start. The copy must pass `PRAGMA quick_check`
/// first; when it does not, the next older snapshot is tried. The damaged files are moved aside
/// as `<name>.damaged-<UTC date-time>` (and its `-wal`, `-shm`), never deleted; after an
/// interrupted restore ([`interrupted_restore`]) the swap it began is finished instead. Staged
/// copies an interrupted restore left are removed first. Everything else is checked before the
/// live files are touched, and a failure on the way puts them back.
pub fn restore(index: &Path, snapshot_name: &str) -> Result<Restored> {
    restore_with(
        index,
        snapshot_name,
        now(),
        &crate::cleanup::free_bytes,
        &open_elsewhere,
    )
}

/// Other processes with `index` (or its `-wal` / `-shm`) open, from `lsof`; None when that
/// cannot be told (no `lsof`, which some Linux images lack). The lock checks cannot see them:
/// the MCP server, hooks and `cal remember` / `star` take no lock. One that holds the damaged
/// file open across a restore keeps writing into the moved-aside copy, and when its last
/// connection closes SQLite deletes `<index>-wal` and `-shm` by name: the restored index's own.
fn open_elsewhere(index: &Path) -> Option<Vec<u32>> {
    let paths: Vec<PathBuf> = ["", "-wal", "-shm"]
        .iter()
        .map(|s| sibling(index, s))
        .filter(|p| std::fs::symlink_metadata(p).is_ok())
        .collect();
    if paths.is_empty() {
        return Some(Vec::new());
    }
    // A GUI app's PATH may not reach /usr/sbin, where macOS keeps lsof.
    let out = ["/usr/sbin/lsof", "/usr/bin/lsof", "lsof"]
        .iter()
        .find_map(|exe| {
            std::process::Command::new(exe)
                .args(["-w", "-t", "--"])
                .args(&paths)
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .ok()
        })?;
    // Killed before it answered: cannot tell.
    out.status.code()?;
    // The pids it printed count whatever its exit status says. lsof exits 1 as soon as one
    // of the files it was given is open nowhere, also while it lists the processes that hold
    // another: a connection that has opened the index and not read from it yet holds
    // `index.db` alone, and reading that answer as "cannot tell" let the restore go ahead
    // under it. Exit 1 with no output is its "nobody has these open".
    let me = std::process::id();
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.trim().parse::<u32>().ok())
            .filter(|&pid| pid != me)
            .collect(),
    )
}

type OthersFn<'a> = &'a dyn Fn(&Path) -> Option<Vec<u32>>;

/// Refuse a restore while another process has `index` open (`others`), or where nothing can
/// tell, except on Windows. The refusal names `snapshot` for the steps to restore by hand.
fn refuse_while_open(index: &Path, snapshot: &Snapshot, others: OthersFn) -> Result<()> {
    match others(index) {
        Some(pids) if !pids.is_empty() => {
            let list = pids
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "another process has the index open (pid {list}): quit the agent sessions that \
                 use Callimachus (its MCP server and hooks open the index) and try again; the \
                 index was not touched"
            );
        }
        Some(_) => Ok(()),
        // SQLite on Windows opens its files without delete sharing: a rename under another
        // process's open handle fails, and the swap undoes itself.
        None if cfg!(windows) => Ok(()),
        // Elsewhere a process that keeps the old file open writes into the copy moved aside,
        // and its last close deletes the restored index's `-wal` and `-shm` by name.
        None => {
            let dir = match index.parent() {
                Some(d) if !d.as_os_str().is_empty() => d.display().to_string(),
                _ => ".".into(),
            };
            let n = index_name(index)?.to_string_lossy();
            bail!(
                "cannot tell whether another process has the index open (`lsof` is not available \
                 here), so nothing was restored; the index was not touched. To restore by hand: \
                 quit every agent session and app that uses Callimachus, move {n}, {n}-wal and \
                 {n}-shm out of {dir} into a new folder, then copy {n} (and {n}-wal, if there is \
                 one) from {} into {dir}",
                snapshot.dir.display()
            );
        }
    }
}

fn restore_with(
    index: &Path,
    snapshot_name: &str,
    at: i64,
    free: &dyn Fn(&Path) -> Option<u64>,
    others: OthersFn,
) -> Result<Restored> {
    refuse_symlinks(index)?;
    let name = index_name(index)?.to_os_string();
    // The one asked for, then the older ones in turn should its copy not pass.
    let candidates: Vec<Snapshot> = restore_candidates(index)
        .into_iter()
        .skip_while(|s| s.name != snapshot_name)
        .collect();
    if candidates.is_empty() {
        bail!("no snapshot {snapshot_name} of {}", index.display());
    }
    let _rewrite = match crate::locks::acquire(index, crate::locks::REWRITE_LOCK)? {
        crate::locks::Acquire::Acquired(lock) => lock,
        crate::locks::Acquire::Held(pid) => bail!(
            "another process is rewriting the index{}; quit it first",
            crate::locks::pid_note(pid)
        ),
    };
    // Holding rewrite.lock, no other restore runs: every staged copy is a leftover of one that
    // was interrupted (the splash closed during its minute-long check), each up to a full copy.
    let leftovers = remove_staged_where(index, |_| true);
    if leftovers > 0 {
        app_log!("[backup] removed {leftovers} file(s) an interrupted restore had staged");
    }
    for lock in WRITER_LOCKS {
        if let Some(pid) = crate::locks::holder(index, lock) {
            bail!(
                "another Callimachus process holds {}{}; quit it (or stop `cal`) and try again",
                crate::locks::lock_path(index, lock).display(),
                crate::locks::pid_note(pid)
            );
        }
    }
    // Before staging too, which on a large index copies and checks the snapshot for a minute (a
    // full copy where the disk cannot clone): where nothing can tell, the check after it would
    // refuse every time, and an open index is most likely still open then.
    refuse_while_open(index, &candidates[0], others)?;
    let stamp = stamp(at);
    // An interrupted swap is finished under its first aside name, which keeps the damaged
    // file's WAL; a new name would leave that pair split.
    let resume = interrupted_restore(index);
    let aside = resume
        .clone()
        .unwrap_or_else(|| sibling(index, &format!(".damaged-{stamp}")));
    let staged = sibling(index, &format!(".restoring-{stamp}"));
    let fresh: &[&PathBuf] = if resume.is_some() {
        &[&staged]
    } else {
        &[&aside, &staged]
    };
    for p in fresh {
        for suffix in ["", "-wal", "-shm"] {
            if std::fs::symlink_metadata(sibling(p, suffix)).is_ok() {
                bail!(
                    "{} already exists; try again in a second",
                    sibling(p, suffix).display()
                );
            }
        }
    }
    let mut skipped: Vec<(Snapshot, anyhow::Error)> = Vec::new();
    let mut chosen = None;
    for s in candidates {
        match stage(&s, &name, &staged, free) {
            Ok(()) => {
                chosen = Some(s);
                break;
            }
            Err(e) => {
                remove_staged(&staged);
                app_log!("[backup] snapshot {} cannot be restored: {e:#}", s.name);
                skipped.push((s, e));
            }
        }
    }
    let Some(snapshot) = chosen else {
        let (_, first) = skipped.swap_remove(0);
        return Err(first.context("preparing the snapshot; the index was not touched"));
    };
    // Again right before the swap: staging (the integrity check above all) takes a minute on a
    // large index, and a process that opened the index meanwhile counts too.
    if let Err(e) = refuse_while_open(index, &snapshot, others) {
        remove_staged(&staged);
        return Err(e);
    }
    let swapped = match &resume {
        Some(first) => {
            // The state must still be the one found under rewrite.lock: a process that opened
            // the index during the check would have given it a `-wal` of its own.
            if interrupted_restore(index).as_ref() != Some(first) {
                remove_staged(&staged);
                bail!(
                    "the index's files changed while the snapshot was checked; nothing was moved: \
                     try again"
                );
            }
            finish_swap(index, &staged, first).map(|()| true)
        }
        None => swap_in(index, &staged, &aside, restore_link),
    };
    match swapped {
        Ok(moved) => {
            db::tighten_mode(index);
            Ok(Restored {
                damaged: moved.then_some(aside),
                snapshot,
                skipped: skipped.into_iter().map(|(s, _)| s).collect(),
            })
        }
        Err(e) => {
            remove_staged(&staged);
            Err(e)
        }
    }
}

/// Copy the snapshot to `staged` next to the index (a clone, else a full copy when the volume
/// has room), fold its WAL in, so one rename puts a complete database in place, and check it:
/// a pre-migration or manual snapshot is a copy of whatever the index held, damage included.
fn stage(
    snapshot: &Snapshot,
    name: &OsStr,
    staged: &Path,
    free: &dyn Fn(&Path) -> Option<u64>,
) -> Result<()> {
    let pairs = [
        (snapshot.dir.join(name), staged.to_path_buf()),
        (
            snapshot.dir.join(with_suffix(name, "-wal")),
            sibling(staged, "-wal"),
        ),
    ];
    for (from, to) in &pairs {
        if !is_real_file(from) {
            continue;
        }
        match clone::clone_file(from, to) {
            Ok(()) => {}
            Err(e) if clone::is_no_clone(&e) => {
                let need = std::fs::metadata(from).map_or(0, |m| m.len());
                let dir = to.parent().unwrap_or(Path::new("."));
                if free(dir).is_some_and(|f| f < need + need / 10) {
                    bail!("not enough free disk space to copy the snapshot next to the index");
                }
                std::fs::copy(from, to).with_context(|| format!("copying {}", from.display()))?;
            }
            Err(e) => return Err(e).with_context(|| format!("cloning {}", from.display())),
        }
        db::tighten_mode(to);
    }
    {
        let conn = Connection::open_with_flags(
            staged,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI,
        )?;
        conn.busy_timeout(Duration::from_secs(5))?;
        let got = read_expect(&conn)?;
        if got.user_version <= 0 {
            bail!("the snapshot holds no schema");
        }
        let (busy, log, done): (i64, i64, i64) =
            conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?;
        if busy != 0 || done < log {
            bail!("the snapshot's WAL could not be folded in");
        }
        // The check the weekly snapshot runs before it is taken (27.9 s warm, 65.6 s cold on
        // a 5.3 GB index): a copy taken from an already damaged file opens and counts its
        // threads fine, and putting it in place would bring the damage back.
        if !quick_check(&conn)? {
            bail!("the snapshot did not pass an integrity check (PRAGMA quick_check)");
        }
    }
    let wal = sibling(staged, "-wal");
    if std::fs::metadata(&wal).is_ok_and(|m| m.len() > 0) {
        bail!("the snapshot's WAL is still there after folding it in");
    }
    for suffix in ["-wal", "-shm"] {
        let p = sibling(staged, suffix);
        if is_real_file(&p) {
            std::fs::remove_file(&p)?;
        }
    }
    Ok(())
}

/// Remove a staged copy (ours, made by [`stage`]) and its sidecars.
fn remove_staged(staged: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let p = sibling(staged, suffix);
        if is_real_file(&p) {
            let _ = std::fs::remove_file(p);
        }
    }
}

type LinkFn = fn(&Path, &Path) -> std::io::Result<()>;

/// How [`restore`] gives the index its aside name: a hard link, except on Windows. There a
/// rename under another process's open handle fails and is undone (the restore goes ahead where
/// nothing can tell who has the index open), but a name the link added cannot be removed while
/// that handle is open (sharing is checked per file, not per name), so each refused attempt
/// would leave a second name of the live index that reads as a damaged copy. And the state the
/// link lets [`interrupted_restore`] find cannot be told there anyway (no inode to compare).
fn restore_link(index: &Path, aside: &Path) -> std::io::Result<()> {
    if cfg!(windows) {
        Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
    } else {
        std::fs::hard_link(index, aside)
    }
}

/// Replace the index with `staged`, keeping the damaged files at `aside` (and its `-wal`,
/// `-shm`). Every step that ran is undone when a later one fails, and after the last rename
/// the folder is synced, so a power loss right after "Restored" cannot leave only some of the
/// renames on disk. Returns whether there was a live index to move aside.
///
/// Where the filesystem has hard links (`link`), the steps, and what an interruption (a kill,
/// a power loss) right after each one leaves:
/// 1. `aside` becomes a second name of the index file. The live pair is whole; the extra name
///    is ignored by the next restore, since the index still has its `-wal` (or never had one).
/// 2. The `-wal` moves to `aside`'s. The index file has no WAL next to it: its latest commits
///    are only in `aside`'s `-wal`, and opening it would lose them and write into the file
///    `aside` shares. [`interrupted_restore`] finds exactly this state, every open outside the
///    running app then refuses the index ([`refuse_interrupted_restore`]; the app shows the
///    damaged-index screen), and restoring again finishes the swap.
/// 3. The `-shm` moves aside too: the same as after 2.
/// 4. The copy is renamed over the index: done, and `aside` with its `-wal` is the damaged pair.
///
/// Without hard links (exFAT, SMB, and Windows: [`restore_link`]), the `-wal` and `-shm` move
/// first and the index follows (a leftover `-wal` must never sit next to a missing or new index
/// file, which would replay it). An interruption after the `-wal` moved leaves the index without
/// its WAL, which is at `aside`'s `-wal` with no `aside` file next to it (nothing detects this:
/// put it back as `<name>-wal` by hand before anything opens the index); after the index moved,
/// no index (the next launch builds a new, empty one) and the damaged pair whole at `aside`;
/// after the last rename, done.
fn swap_in(index: &Path, staged: &Path, aside: &Path, link: LinkFn) -> Result<bool> {
    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
    let undo = |moved: &[(PathBuf, PathBuf)], linked: bool| {
        for (from, to) in moved.iter().rev() {
            let _ = std::fs::rename(to, from);
        }
        // The index is untouched: drop its extra name, on Unix only while it still is the same
        // file as the index. Elsewhere files cannot be compared, and `link` succeeding is proof
        // enough: it fails on a name that exists, and the index never moves on this path.
        if linked && (!cfg!(unix) || same_file(index, aside)) {
            let _ = std::fs::remove_file(aside);
        }
    };
    let had_index = std::fs::symlink_metadata(index).is_ok();
    let linked = had_index && link(index, aside).is_ok();
    for suffix in ["-wal", "-shm"] {
        let (live, to) = (sibling(index, suffix), sibling(aside, suffix));
        if std::fs::symlink_metadata(&live).is_err() {
            continue;
        }
        if let Err(e) = std::fs::rename(&live, &to) {
            undo(&moved, linked);
            return Err(e).with_context(|| format!("moving {} aside", live.display()));
        }
        moved.push((live, to));
    }
    if had_index && !linked {
        if let Err(e) = std::fs::rename(index, aside) {
            undo(&moved, linked);
            return Err(e).with_context(|| format!("moving {} aside", index.display()));
        }
        moved.push((index.to_path_buf(), aside.to_path_buf()));
    }
    if let Err(e) = std::fs::rename(staged, index) {
        undo(&moved, linked);
        return Err(e).context("putting the snapshot in place");
    }
    sync_dir(index);
    Ok(had_index)
}

/// Finish the swap an interrupted restore left ([`interrupted_restore`]): the index is still
/// a second name of the damaged file `aside`, whose `-wal` holds its latest commits. A `-shm`
/// still at the index moves aside too (unless `aside` has one already: a `-shm` is rebuilt by
/// the first connection when no WAL is next to it), then the copy is renamed over the index,
/// which takes that name off the damaged file and leaves `aside` and its `-wal` one pair.
fn finish_swap(index: &Path, staged: &Path, aside: &Path) -> Result<()> {
    let (shm, aside_shm) = (sibling(index, "-shm"), sibling(aside, "-shm"));
    let moved_shm = is_real_file(&shm) && std::fs::symlink_metadata(&aside_shm).is_err();
    if moved_shm {
        std::fs::rename(&shm, &aside_shm)
            .with_context(|| format!("moving {} aside", shm.display()))?;
    }
    if let Err(e) = std::fs::rename(staged, index) {
        if moved_shm {
            let _ = std::fs::rename(&aside_shm, &shm);
        }
        return Err(e).context("putting the snapshot in place");
    }
    sync_dir(index);
    Ok(())
}

/// The state a restore interrupted halfway through its swap leaves ([`swap_in`], after step 2
/// or 3): Some(aside) when a `<name>.damaged-<UTC date-time>` is the same file as the index
/// (device and inode: the hard link the swap makes) while the index has no `-wal` and that
/// aside name has one. Only that exact state: it proves the last rename never happened and
/// that the index's latest commits are in the aside `-wal`. No other combination of files is
/// read as one (files a user moved or deleted by hand are never "recovered"). Read-only; Unix
/// only (elsewhere it cannot compare files and says None).
pub fn interrupted_restore(index: &Path) -> Option<PathBuf> {
    // Only a file with a second name can be in that state (the swap's hard link), so one
    // `lstat` settles every ordinary open without listing the index's folder. This runs on
    // every open of the index, and the folder can be crowded: a `CALLIMACHUS_DB` in a shared
    // folder, or the test suite's temp dir, where a listing per open of a fresh index (no
    // `-wal` yet) adds up over thousands of opens.
    if !has_second_name(index) || std::fs::symlink_metadata(sibling(index, "-wal")).is_ok() {
        return None;
    }
    stamped_siblings(index, "damaged")
        .into_iter()
        .filter(|f| !f.sidecar)
        .map(|f| f.path)
        .find(|aside| same_file(index, aside) && is_real_file(&sibling(aside, "-wal")))
}

/// A regular file with more than one name (a hard link). False where that cannot be told (off
/// Unix, where [`same_file`] cannot compare files either).
fn has_second_name(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file() && m.nlink() > 1)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

/// What an open of the index returns in the state [`interrupted_restore`] finds:
/// `SQLITE_CORRUPT`, so the app's startup lands on the damaged-index screen, whose restore
/// finishes the swap. Hooks exit quietly on any error; `cal` and the MCP server print it.
pub fn interrupted_restore_error(aside: &Path) -> anyhow::Error {
    anyhow::Error::from(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
        Some(format!(
            "a restore was interrupted before it finished: the index's latest changes are in {}, \
             not next to the index; nothing writes to the index until that restore is finished \
             (open the Callimachus app, which offers it)",
            sibling(aside, "-wal").display()
        )),
    ))
}

/// Refuse to open `index` in the state [`interrupted_restore`] finds. An open reads the file
/// without its latest commits (they are in the aside `-wal`) and gives it a new `-wal`, a
/// read-only one too while the `-shm` is still beside it (measured: it read 2 of 6 threads).
/// A writer's commits then land in the file the kept copy shares, and while that `-wal` stays
/// (an MCP server's as long as it runs, `cal backup`'s for good: it closes without a
/// checkpoint) the state no longer reads as an interrupted restore, so the app's next launch
/// would open the damaged file as the live index instead of offering the restore that finishes
/// the swap. Every open of the live index outside the running app calls it: `db::open_with`
/// (the app's writer, `cal` write commands, `cal index`), `db::open_sidecar_current_with`
/// (hooks, the MCP server), `db::open_readonly` (`cal hook`, `cal doctor`, the queries) and
/// [`open_live`] (`cal backup`, the snapshots). Costs one `lstat` of the index: only a file
/// with a second name is looked at further.
pub fn refuse_interrupted_restore(index: &Path) -> Result<()> {
    match interrupted_restore(index) {
        Some(aside) => Err(interrupted_restore_error(&aside)),
        None => Ok(()),
    }
}

/// Flush the folder holding `path` to disk, so the renames in it survive a power loss. Best
/// effort: Windows cannot open a folder as a file, and a failure leaves things as they were.
fn sync_dir(path: &Path) {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    if let Ok(f) = std::fs::File::open(dir) {
        let _ = f.sync_all();
    }
}

/// Whether two paths name one file (Unix: device and inode). False when unknown.
fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (a, b);
        false
    }
}

/// Claude Code sessions in the index whose transcript is gone (`ledger::preserved_claude_threads`),
/// which a rebuild from the agents' history cannot bring back: read from the damaged index
/// when it still answers, else from `fallback` (its newest snapshot). None when neither does.
pub fn sessions_a_rebuild_loses(index: &Path, fallback: Option<&Snapshot>) -> Option<i64> {
    let root = crate::indexer::claude::projects_root()?;
    // Not after an interrupted restore: opening the index would give it a new `-wal`, and the
    // restore that finishes the swap recognizes that state only while it has none.
    if interrupted_restore(index).is_none() {
        if let Ok(conn) = db::open_readonly(index) {
            if let Ok(n) = crate::ledger::preserved_claude_threads(&conn, &root) {
                return Some(n);
            }
        }
    }
    let s = fallback?;
    let db_path = s.dir.join(index.file_name()?);
    let n = {
        let conn = open_copy(&db_path).ok()?;
        crate::ledger::preserved_claude_threads(&conn, &root).ok()
    };
    let shm = sibling(&db_path, "-shm");
    if is_real_file(&shm) {
        let _ = std::fs::remove_file(shm);
    }
    n
}

// ---- cal backup ------------------------------------------------------------------------------

/// How `cal backup` made its copy.
#[derive(Debug, PartialEq, Eq)]
pub enum Method {
    Clone,
    FullCopy,
}

/// What `cal backup` made.
#[derive(Debug)]
pub struct Made {
    pub dir: PathBuf,
    pub method: Method,
    pub bytes: u64,
}

/// `cal backup [--to DIR]`: a copy of the index in `<index dir>/snapshots/<stamp>-manual[-<index
/// id>]/` or under DIR. A clone when the system and volume allow one, else a full copy through SQLite's
/// online backup API, refused when the target volume has less free space than the index's
/// size plus 10%. Never migrates or creates the index. Progress goes to stderr; the line
/// returned is the summary `cli_core` prints (stdout stays with `cli_core`, which owns it).
pub fn cmd_backup(args: &[String]) -> Result<String> {
    let mut to: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--to" => to = Some(PathBuf::from(it.next().context("--to needs a folder")?)),
            other => bail!(
                "unknown argument '{other}' for `cal backup` (it takes --to DIR). Run `cal help`."
            ),
        }
    }
    let index = db::default_index_path();
    let mut last_tenth = -1i64;
    let made = backup_to(
        &index,
        to.as_deref(),
        clone::clone_file,
        &crate::cleanup::free_bytes,
        &mut |done, total| {
            let tenth = (done * 10 / total.max(1)) as i64;
            if tenth != last_tenth {
                last_tenth = tenth;
                eprintln!("cal backup: {}% copied", tenth * 10);
            }
        },
    )?;
    if to.is_none() {
        prune_read_only(&index, now());
    }
    let gb = made.bytes as f64 / 1_073_741_824.0;
    Ok(match made.method {
        Method::Clone => format!(
            "cloned the index ({gb:.1} GB) into {}: free on disk until the index changes",
            made.dir.display()
        ),
        Method::FullCopy => format!("copied the index ({gb:.1} GB) into {}", made.dir.display()),
    })
}

type CloneFn = fn(&Path, &Path) -> std::io::Result<()>;

/// [`cmd_backup`]'s work, with the clone call, the free-space probe and the progress sink
/// injected for tests.
fn backup_to(
    index: &Path,
    to: Option<&Path>,
    clone_with: CloneFn,
    free: &dyn Fn(&Path) -> Option<u64>,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<Made> {
    if !index.exists() {
        bail!(
            "no index at {}; nothing to back up (set CALLIMACHUS_DB to back up another)",
            index.display()
        );
    }
    refuse_symlinks(index)?;
    let name = index_name(index)?.to_os_string();
    let root = match to {
        // A folder the user chose: created if missing, its mode never changed. The snapshot
        // folder made inside it is owner-only like any other.
        Some(dir) => {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            dir.to_path_buf()
        }
        None => ensure_snapshots_root(index)?,
    };
    let at = now();
    // Named with the index's identity when it has one; `cal backup` never writes one.
    let lineage = stored_index_id(index);
    let dir = new_snapshot_dir(&root, Reason::Manual, at, lineage.as_deref())?;
    let result = (|| -> Result<Method> {
        // A clone first, under the write lock (the sidecar's wait: a long app write ends).
        let conn = open_live(index, db::SIDECAR_BUSY_TIMEOUT)?;
        conn.execute_batch("BEGIN IMMEDIATE;")?;
        let cloned = (|| -> Result<Option<Expect>> {
            let expect = read_expect(&conn)?;
            match clone_with(index, &dir.join(&name)) {
                Ok(()) => {}
                Err(e) if clone::is_no_clone(&e) => return Ok(None),
                Err(e) => return Err(e).context("cloning the index"),
            }
            let wal = sibling(index, "-wal");
            if is_real_file(&wal) {
                clone_with(&wal, &dir.join(with_suffix(&name, "-wal")))
                    .context("cloning the index's WAL")?;
            }
            Ok(Some(expect))
        })();
        conn.execute_batch("ROLLBACK;")?;
        drop(conn);
        if let Some(expect) = cloned? {
            for f in snapshot_files(&dir, &name) {
                db::tighten_mode(&f);
            }
            check_copy(&dir.join(&name), expect)?;
            return Ok(Method::Clone);
        }
        // No clone here: a full copy, if the target volume has room for it.
        let size = index_size(index);
        let need = size + size * FULL_COPY_HEADROOM_PERCENT / 100;
        if let Some(f) = free(&root) {
            if f < need {
                bail!(
                    "not enough free disk space for a full copy: it needs about {:.1} GB on the \
                     target's disk, which has {:.1} GB free",
                    need as f64 / 1_073_741_824.0,
                    f as f64 / 1_073_741_824.0
                );
            }
        }
        let expect = full_copy(index, &dir.join(&name), progress)?;
        check_copy(&dir.join(&name), expect)?;
        Ok(Method::FullCopy)
    })();
    match result {
        Ok(method) => Ok(Made {
            bytes: snapshot_files(&dir, &name)
                .iter()
                .filter_map(|f| std::fs::metadata(f).ok())
                .map(|m| m.len())
                .sum(),
            dir,
            method,
        }),
        Err(e) => {
            remove_snapshot_dir(&dir, &name);
            Err(e)
        }
    }
}

/// [`read_copy`] and compare with what the index showed.
fn check_copy(db_path: &Path, expect: Expect) -> Result<()> {
    let got = read_copy(db_path)?;
    if got != expect {
        bail!(
            "the copy reads as schema {} with {} threads, not {} with {}",
            got.user_version,
            got.threads,
            expect.user_version,
            expect.threads
        );
    }
    Ok(())
}

/// Copy `index` into the new file `dest` with SQLite's online backup API, off the writer: one
/// read transaction pins the snapshot for the whole copy, so the app's commits meanwhile
/// neither restart it nor end up half in it. `progress(done, total)` in pages.
fn full_copy(index: &Path, dest: &Path, progress: &mut dyn FnMut(u64, u64)) -> Result<Expect> {
    let src = db::open_readonly(index)?;
    src.execute_batch("BEGIN;")?;
    let expect = read_expect(&src)?;
    let mut dst = Connection::open_with_flags(
        dest,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
    )
    .with_context(|| format!("creating {}", dest.display()))?;
    db::tighten_mode(dest);
    {
        let backup = rusqlite::backup::Backup::new(&src, &mut dst)?;
        loop {
            match backup.step(BACKUP_STEP_PAGES)? {
                rusqlite::backup::StepResult::Done => break,
                rusqlite::backup::StepResult::More => {}
                _ => std::thread::sleep(Duration::from_millis(50)),
            }
            let p = backup.progress();
            let total = p.pagecount.max(0) as u64;
            progress(total.saturating_sub(p.remaining.max(0) as u64), total);
        }
        let p = backup.progress();
        progress(p.pagecount.max(0) as u64, p.pagecount.max(0) as u64);
    }
    drop(dst);
    let _ = src.execute_batch("COMMIT;");
    Ok(expect)
}

// ---- clones -----------------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod clone {
    use std::ffi::CString;
    use std::os::raw::{c_char, c_void};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    /// `struct attrlist` from <sys/attr.h>.
    #[repr(C)]
    struct AttrList {
        bitmapcount: u16,
        reserved: u16,
        commonattr: u32,
        volattr: u32,
        dirattr: u32,
        fileattr: u32,
        forkattr: u32,
    }

    extern "C" {
        fn clonefile(src: *const c_char, dst: *const c_char, flags: u32) -> i32;
        fn getattrlist(
            path: *const c_char,
            list: *mut AttrList,
            buf: *mut c_void,
            size: usize,
            options: u32,
        ) -> i32;
    }

    // <sys/clonefile.h> and <sys/attr.h> (macOS SDK).
    const CLONE_NOFOLLOW: u32 = 0x0001;
    const ATTR_BIT_MAP_COUNT: u16 = 5;
    const ATTR_CMNEXT_PRIVATESIZE: u32 = 0x0000_0008;
    const FSOPT_NOFOLLOW: u32 = 0x0000_0001;
    const FSOPT_ATTR_CMN_EXTENDED: u32 = 0x0000_0020;

    fn c_path(p: &Path) -> std::io::Result<CString> {
        CString::new(p.as_os_str().as_bytes())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in path"))
    }

    /// `clonefile(2)`: a copy-on-write clone of `src` at the new path `dst` (same volume).
    pub(super) fn clone_file(src: &Path, dst: &Path) -> std::io::Result<()> {
        let (s, d) = (c_path(src)?, c_path(dst)?);
        // SAFETY: both are NUL-terminated paths that live across the call; clonefile only
        // reads them.
        if unsafe { clonefile(s.as_ptr(), d.as_ptr(), CLONE_NOFOLLOW) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    /// The bytes of `path` no other file shares (`ATTR_CMNEXT_PRIVATESIZE`): what deleting it
    /// would give back. A fresh clone reads 0.
    pub(super) fn private_size(path: &Path) -> Option<u64> {
        let p = c_path(path).ok()?;
        let mut list = AttrList {
            bitmapcount: ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: 0,
            volattr: 0,
            dirattr: 0,
            fileattr: 0,
            forkattr: ATTR_CMNEXT_PRIVATESIZE,
        };
        // A u32 length, then the off_t, packed (attribute data is 4-byte aligned).
        let mut buf = [0u8; 16];
        // SAFETY: `list` is a valid attrlist, `buf` is writable for the length passed, and the
        // path is NUL-terminated; getattrlist writes at most `buf.len()` bytes.
        let rc = unsafe {
            getattrlist(
                p.as_ptr(),
                &mut list,
                buf.as_mut_ptr().cast(),
                buf.len(),
                FSOPT_ATTR_CMN_EXTENDED | FSOPT_NOFOLLOW,
            )
        };
        let len = u32::from_ne_bytes(buf[0..4].try_into().ok()?);
        if rc != 0 || len < 12 {
            return None;
        }
        let size = i64::from_ne_bytes(buf[4..12].try_into().ok()?);
        u64::try_from(size).ok()
    }

    /// The clone failed because this volume or path cannot clone (not APFS, another volume),
    /// as opposed to a real error.
    pub(super) fn is_no_clone(e: &std::io::Error) -> bool {
        // ENOTSUP 45, EXDEV 18 (<sys/errno.h>).
        matches!(e.raw_os_error(), Some(45 | 18))
            || matches!(
                e.kind(),
                std::io::ErrorKind::Unsupported | std::io::ErrorKind::CrossesDevices
            )
    }
}

#[cfg(not(target_os = "macos"))]
mod clone {
    use std::path::Path;

    /// No cheap clone here yet (Linux FICLONE, Windows block cloning: untested).
    pub(super) fn clone_file(_src: &Path, _dst: &Path) -> std::io::Result<()> {
        Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
    }

    pub(super) fn private_size(_path: &Path) -> Option<u64> {
        None
    }

    pub(super) fn is_no_clone(e: &std::io::Error) -> bool {
        matches!(
            e.kind(),
            std::io::ErrorKind::Unsupported | std::io::ErrorKind::CrossesDevices
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_support::TestDir;
    use crate::indexer::{source_id, upsert_thread, ParsedMessage, ParsedThread};

    const DAY: i64 = 86_400;
    /// 2026-11-01T00:00:00Z.
    const T0: i64 = 1_793_491_200;

    fn thread(ext: &str) -> ParsedThread {
        ParsedThread {
            external_id: ext.into(),
            title: Some(ext.into()),
            updated_at: Some(1_000),
            created_at: Some(1_000),
            messages: vec![ParsedMessage {
                role: "user".into(),
                text: format!("{ext} body"),
                tool_name: None,
                ts: Some(1_000),
            }],
            ..Default::default()
        }
    }

    /// A migrated index at `index` with `n` threads, its connection still open.
    fn index_with(index: &Path, n: usize) -> Connection {
        let mut conn = db::open(index).unwrap();
        let sid = source_id(&conn, "claude_code").unwrap();
        for i in 0..n {
            upsert_thread(&mut conn, sid, &thread(&format!("t{i}"))).unwrap();
        }
        conn
    }

    fn threads_in(db_path: &Path) -> i64 {
        read_copy(db_path).unwrap().threads
    }

    /// A folder that looks like a snapshot to [`list`] (file presence only), for retention
    /// and safety tests that need no real database.
    fn plant(index: &Path, folder: &str, files: &[&str]) -> PathBuf {
        let dir = snapshots_dir(index).join(folder);
        std::fs::create_dir_all(&dir).unwrap();
        for f in files {
            std::fs::write(dir.join(f), b"planted").unwrap();
        }
        dir
    }

    fn names(index: &Path) -> Vec<String> {
        list(index).into_iter().map(|s| s.name).collect()
    }

    #[cfg(unix)]
    fn mode(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777
    }

    /// The live index's identity in tests, and another index's.
    const ID: &str = "0a1b2c3d";
    const OTHER: &str = "9f8e7d6c";

    /// A folder name as [`new_snapshot_dir`] gives it.
    fn named(at: i64, r: Reason, id: Option<&str>) -> String {
        match id {
            Some(id) => format!("{}-{}-{id}", stamp(at), r.as_str()),
            None => format!("{}-{}", stamp(at), r.as_str()),
        }
    }

    /// Give the index `conn` is open on the identity `id`, as its first snapshot would.
    fn set_id(conn: &Connection, id: &str) {
        app_config::put(conn, ID_KEY, id).unwrap();
    }

    #[test]
    fn folder_names_round_trip_and_strangers_do_not_parse() {
        let at = T0 + 3_723;
        let root = TestDir::new("backup_names");
        let root = root.parent().unwrap().join("snapshots");
        std::fs::create_dir_all(&root).unwrap();
        for r in Reason::ALL {
            for id in [None, Some(ID), Some("12345678")] {
                let name = named(at, r, id);
                let want = Some((at, r, id.map(str::to_string)));
                assert_eq!(parse_name(&name), want, "{name}");
                assert_eq!(parse_name(&format!("{name}-2")), want, "{name}-2");
                // What new_snapshot_dir makes reads back as what it was given.
                let made = new_snapshot_dir(&root, r, at, id).unwrap();
                let again = new_snapshot_dir(&root, r, at, id).unwrap();
                for dir in [made, again] {
                    let folder = dir.file_name().unwrap().to_str().unwrap().to_string();
                    assert_eq!(parse_name(&folder), want, "{folder}");
                }
            }
        }
        assert_eq!(stamp(at), "20261101T010203Z");
        assert_eq!(
            parse_name("20261101T010203Z-pre-migration-0a1b2c3d-3"),
            Some((at, Reason::PreMigration, Some(ID.to_string())))
        );
        for stranger in [
            "notes",
            "20261101T010203Z",
            "20261101T010203Z-",
            "20261101T010203Z-weeklyx",
            "20261101T010203Z-weekly-",
            "20261101T010203Z-weekly-2a",
            "2026-11-01-weekly",
            "20261301T010203Z-weekly",
            "x20261101T010203Z-weekly",
            // An identity is exactly 8 lowercase hex digits, before any `-<n>`.
            "20261101T010203Z-weekly-0A1B2C3D",
            "20261101T010203Z-weekly-0a1b2c3",
            "20261101T010203Z-weekly-0a1b2c3d4",
            "20261101T010203Z-weekly-0a1b2c3d-",
            "20261101T010203Z-weekly-0a1b2c3d-x",
            "20261101T010203Z-weekly-2-0a1b2c3d",
            "20261101T010203Z-weekly-0a1b2c3g",
        ] {
            assert_eq!(parse_name(stranger), None, "{stranger}");
        }
    }

    /// The identity is created once, committed, and read back the same by every connection;
    /// two connections creating it at the same moment agree on one; an invalid stored value
    /// is replaced; and the read-only reader never writes one.
    #[test]
    fn the_index_identity_is_created_once_and_agreed_on() {
        let dir = TestDir::new("backup_identity");
        drop(index_with(&dir, 0));
        assert_eq!(
            stored_index_id(&dir),
            None,
            "none until a snapshot needs one"
        );
        let path = dir.to_path_buf();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let racers: Vec<_> = (0..2)
            .map(|_| {
                let (path, barrier) = (path.clone(), barrier.clone());
                std::thread::spawn(move || {
                    let conn = open_live(&path, Duration::from_secs(5)).unwrap();
                    barrier.wait();
                    ensure_index_id(&conn).unwrap()
                })
            })
            .collect();
        let ids: Vec<String> = racers.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(ids[0], ids[1]);
        assert!(valid_id(&ids[0]), "{}", ids[0]);
        assert_eq!(stored_index_id(&dir).as_deref(), Some(ids[0].as_str()));
        let conn = open_live(&dir, Duration::from_secs(5)).unwrap();
        assert_eq!(ensure_index_id(&conn).unwrap(), ids[0], "kept once made");
        // Inside a transaction it refuses: the snapshot's clone would not carry it.
        conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
        assert!(ensure_index_id(&conn).is_err());
        conn.execute_batch("ROLLBACK;").unwrap();
        app_config::put(&conn, ID_KEY, "not-an-id").unwrap();
        assert_eq!(index_id(&conn), None);
        let replaced = ensure_index_id(&conn).unwrap();
        assert!(valid_id(&replaced) && replaced != ids[0], "{replaced}");
        assert_eq!(index_id(&conn).as_deref(), Some(replaced.as_str()));
    }

    /// The clone is taken under the write lock with commits still only in the WAL: the
    /// snapshot folder opens as the same database, owner-only, and the live index is not
    /// checkpointed or changed by it.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_snapshot_holds_what_only_the_wal_had() {
        let dir = TestDir::new("backup_wal");
        let conn = index_with(&dir, 0);
        // Keep every commit in the WAL: no autocheckpoint, and the connection stays open.
        conn.pragma_update(None, "wal_autocheckpoint", 0_i64)
            .unwrap();
        let mut conn = conn;
        let sid = source_id(&conn, "claude_code").unwrap();
        for i in 0..7 {
            upsert_thread(&mut conn, sid, &thread(&format!("w{i}"))).unwrap();
        }
        let wal = sibling(&dir, "-wal");
        let wal_before = std::fs::metadata(&wal).unwrap().len();
        assert!(wal_before > 0, "the commits are in the WAL");

        let s = snapshot(&dir, Reason::Manual, Duration::from_secs(5)).unwrap();
        assert_eq!(s.reason, Reason::Manual);
        assert_eq!(names(&dir), std::slice::from_ref(&s.name));
        assert_eq!(threads_in(&s.dir.join("index.db")), 7);
        let mut files: Vec<_> = std::fs::read_dir(&s.dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        files.sort();
        assert_eq!(files, ["index.db", "index.db-wal"], "no -shm left behind");
        assert_eq!(mode(&snapshots_dir(&dir)), 0o700);
        assert_eq!(mode(&s.dir), 0o700);
        assert_eq!(mode(&s.dir.join("index.db")), 0o600);
        assert_eq!(mode(&s.dir.join("index.db-wal")), 0o600);
        // The live pair is as it was: same WAL, same rows.
        assert_eq!(std::fs::metadata(&wal).unwrap().len(), wal_before);
        let live: i64 = conn
            .query_row("SELECT count(*) FROM threads", [], |r| r.get(0))
            .unwrap();
        assert_eq!(live, 7);
    }

    /// Retention keeps the index's newest weekly, its pre-migration ones younger than 7 days
    /// and every manual one; nothing it did not name is touched: a stranger's file or folder,
    /// a folder the user added a file to, another index file's snapshots, snapshots of an
    /// earlier index or without an identity, or a symbolic link (followed nowhere: its target
    /// keeps its files). An index without an identity removes nothing.
    #[test]
    fn retention_removes_only_its_own_files_and_folders() {
        let dir = TestDir::new("backup_prune");
        let outside = TestDir::new("backup_prune_outside");
        std::fs::write(&*outside, b"not yours").unwrap();
        let mine = |at: i64, r: Reason| named(at, r, Some(ID));
        let index_files = ["index.db", "index.db-wal"];
        let old_weekly = mine(T0, Reason::Weekly);
        let new_weekly = mine(T0 + 7 * DAY, Reason::Weekly);
        let stale_pre = mine(T0 - 30 * DAY, Reason::PreMigration);
        // Two pre-migration ones younger than 7 days: both stay, none goes for not being
        // the newest (that is an upgrade's own job, `pre_migration_done`).
        let fresh_pre = mine(T0 + 6 * DAY, Reason::PreMigration);
        let fresher_pre = mine(T0 + 7 * DAY, Reason::PreMigration);
        let manual = mine(T0 - 90 * DAY, Reason::Manual);
        for name in [
            &old_weekly,
            &new_weekly,
            &stale_pre,
            &fresh_pre,
            &fresher_pre,
            &manual,
        ] {
            plant(&dir, name, &index_files);
        }
        // Decoys.
        let root = snapshots_dir(&dir);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("notes.txt"), b"mine").unwrap();
        plant(&dir, "my-copy", &["index.db"]);
        let other_file = plant(&dir, &mine(T0 - DAY, Reason::Weekly), &["index.dev.db"]);
        let with_extra = plant(&dir, &mine(T0 - 2 * DAY, Reason::Weekly), &["index.db"]);
        std::fs::write(with_extra.join("keep.txt"), b"mine").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            outside.parent().unwrap(),
            root.join(mine(T0 - 3 * DAY, Reason::Weekly)),
        )
        .unwrap();
        let earlier = [
            named(T0 - 40 * DAY, Reason::Weekly, Some(OTHER)),
            named(T0 - 40 * DAY, Reason::PreMigration, Some(OTHER)),
            named(T0 - 50 * DAY, Reason::Weekly, None),
            named(T0 - 50 * DAY, Reason::PreMigration, None),
        ];
        for name in &earlier {
            plant(&dir, name, &index_files);
        }

        // Nothing at all without the live index's identity.
        assert_eq!(prune(&dir, None, T0 + 365 * DAY), 0);
        // Two folders go (the older weekly, the stale pre-migration one). The weekly the user
        // added a file to loses its index copy, but the folder and their file stay.
        let removed = prune(&dir, Some(ID), T0 + 8 * DAY);
        assert_eq!(removed, 2);
        assert!(!with_extra.join("index.db").exists());
        let mut left = names(&dir);
        left.sort();
        let mut want: Vec<String> = [&new_weekly, &fresh_pre, &fresher_pre, &manual]
            .into_iter()
            .chain(&earlier)
            .cloned()
            .collect();
        want.sort();
        assert_eq!(left, want);
        assert!(root.join("notes.txt").exists());
        assert!(root.join("my-copy/index.db").exists());
        assert!(
            other_file.join("index.dev.db").exists(),
            "another index file's snapshot"
        );
        assert!(
            with_extra.join("keep.txt").exists(),
            "a file the user put there"
        );
        assert!(with_extra.exists(), "so its folder stays too");
        assert_eq!(std::fs::read(&*outside).unwrap(), b"not yours");
        // A week later the pre-migration ones have expired; the manual one never does, and an
        // earlier index's are never this index's to expire.
        prune(&dir, Some(ID), T0 + 7 * DAY + PRE_MIGRATION_KEEP_SECS);
        let mut left = names(&dir);
        left.sort();
        let mut want: Vec<String> = [&new_weekly, &manual]
            .into_iter()
            .chain(&earlier)
            .cloned()
            .collect();
        want.sort();
        assert_eq!(left, want);
    }

    /// Stamps written while the clock ran far ahead: a pre-migration snapshot stamped ahead
    /// expires instead of living until real time passes it, and the newest weekly kept is the
    /// newest one not ahead of the clock (the one ahead goes); alone, one ahead is kept.
    #[test]
    fn retention_does_not_trust_stamps_ahead_of_the_clock() {
        let dir = TestDir::new("backup_prune_clock");
        let ahead = T0 + 210 * DAY;
        let future_weekly = named(ahead, Reason::Weekly, Some(ID));
        let future_pre = named(ahead, Reason::PreMigration, Some(ID));
        plant(&dir, &future_weekly, &["index.db"]);
        plant(&dir, &future_pre, &["index.db"]);
        assert_eq!(prune(&dir, Some(ID), T0), 1);
        assert_eq!(
            names(&dir),
            std::slice::from_ref(&future_weekly),
            "kept while alone"
        );
        // Within the hour of slack, a stamp counts as now.
        let soon = named(T0 + 1_800, Reason::PreMigration, Some(ID));
        plant(&dir, &soon, &["index.db"]);
        let now_weekly = named(T0, Reason::Weekly, Some(ID));
        plant(&dir, &now_weekly, &["index.db"]);
        assert_eq!(prune(&dir, Some(ID), T0), 1);
        let mut left = names(&dir);
        left.sort();
        let mut want = vec![soon, now_weekly];
        want.sort();
        assert_eq!(left, want);
        assert!(expired(T0 + 3_601, T0, DAY) && !expired(T0 + 3_600, T0, DAY));
        assert!(expired(T0 - DAY, T0, DAY) && !expired(T0 - DAY + 1, T0, DAY));
    }

    /// A snapshots folder that is a symbolic link is neither listed nor written through.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_snapshots_folder_is_left_alone() {
        let dir = TestDir::new("backup_root_link");
        let elsewhere = TestDir::new("backup_root_target");
        let target = elsewhere.parent().unwrap().to_path_buf();
        let planted = target.join(format!("{}-weekly", stamp(T0)));
        std::fs::create_dir_all(&planted).unwrap();
        std::fs::write(planted.join("index.db"), b"theirs").unwrap();
        drop(index_with(&dir, 1));
        std::os::unix::fs::symlink(&target, snapshots_dir(&dir)).unwrap();
        assert!(list(&dir).is_empty());
        assert_eq!(prune(&dir, Some(ID), T0 + 365 * DAY), 0);
        let conn = open_live(&dir, Duration::from_secs(1)).unwrap();
        conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
        let err = take_locked(&conn, &dir, Reason::Manual, T0, None)
            .err()
            .unwrap();
        conn.execute_batch("ROLLBACK;").unwrap();
        assert!(format!("{err:#}").contains("not a folder"), "{err:#}");
        assert_eq!(std::fs::read(planted.join("index.db")).unwrap(), b"theirs");
    }

    /// After Shrink index / Reclaim space: the index's automatic snapshots go, the manual ones
    /// and an earlier index's stay, and the weekly state is reset so a fresh one follows the
    /// next check.
    #[test]
    fn a_rewrite_drops_the_automatic_snapshots_only() {
        let dir = TestDir::new("backup_rewrite");
        let conn = index_with(&dir, 1);
        set_id(&conn, ID);
        app_config::put(&conn, LAST_AT_KEY, "1").unwrap();
        app_config::put(&conn, LAST_CHECK_KEY, r#"{"at":1,"ok":true,"ms":5}"#).unwrap();
        plant(&dir, &named(T0, Reason::Weekly, Some(ID)), &["index.db"]);
        plant(
            &dir,
            &named(T0, Reason::PreMigration, Some(ID)),
            &["index.db", "index.db-wal"],
        );
        let manual = plant(&dir, &named(T0, Reason::Manual, Some(ID)), &["index.db"]);
        let earlier = [
            named(T0 - DAY, Reason::Weekly, Some(OTHER)),
            named(T0 - DAY, Reason::PreMigration, Some(OTHER)),
            named(T0 - 2 * DAY, Reason::Weekly, None),
            named(T0 - 2 * DAY, Reason::PreMigration, None),
        ];
        for name in &earlier {
            plant(&dir, name, &["index.db"]);
        }
        let sorted = |mut v: Vec<String>| {
            v.sort();
            v
        };
        after_rewrite(&conn, &dir);
        assert_eq!(
            sorted(names(&dir)),
            sorted(
                std::iter::once(named(T0, Reason::Manual, Some(ID)))
                    .chain(earlier.iter().cloned())
                    .collect()
            )
        );
        assert!(manual.join("index.db").exists());
        assert_eq!(app_config::get(&conn, LAST_AT_KEY).unwrap(), None);
        assert_eq!(last_check(&conn), None);
        // The 0036 rebuild drops only the index's pre-migration kind.
        plant(
            &dir,
            &named(T0 + DAY, Reason::Weekly, Some(ID)),
            &["index.db"],
        );
        plant(
            &dir,
            &named(T0 + DAY, Reason::PreMigration, Some(ID)),
            &["index.db"],
        );
        after_partition(&conn, &dir);
        assert_eq!(
            sorted(names(&dir)),
            sorted(
                [
                    named(T0, Reason::Manual, Some(ID)),
                    named(T0 + DAY, Reason::Weekly, Some(ID)),
                ]
                .into_iter()
                .chain(earlier.iter().cloned())
                .collect()
            )
        );
        // An index without an identity removes nothing either way.
        app_config::delete(&conn, ID_KEY).unwrap();
        let before = names(&dir);
        after_rewrite(&conn, &dir);
        after_partition(&conn, &dir);
        assert_eq!(names(&dir), before);
    }

    /// While the last integrity check failed, the weekly snapshot (the one checked copy)
    /// outlives a rewrite; the weekly state is cleared all the same, so the next tick checks
    /// the rewritten file, and the failed record stays (without its date) so a second rewrite
    /// keeps the snapshot too.
    #[test]
    fn a_rewrite_after_a_failed_check_keeps_the_weekly_snapshot() {
        let dir = TestDir::new("backup_rewrite_badcheck");
        let conn = index_with(&dir, 1);
        set_id(&conn, ID);
        app_config::put(&conn, LAST_AT_KEY, "1").unwrap();
        app_config::put(&conn, LAST_CHECK_KEY, r#"{"at":9,"ok":false,"ms":5}"#).unwrap();
        let weekly = named(T0, Reason::Weekly, Some(ID));
        plant(&dir, &weekly, &["index.db"]);
        plant(
            &dir,
            &named(T0 + DAY, Reason::PreMigration, Some(ID)),
            &["index.db"],
        );
        after_rewrite(&conn, &dir);
        assert_eq!(names(&dir), std::slice::from_ref(&weekly));
        assert_eq!(app_config::get(&conn, LAST_AT_KEY).unwrap(), None);
        let carried = CheckRecord {
            at: 0,
            ok: false,
            ms: 5,
        };
        assert_eq!(last_check(&conn), Some(carried));
        after_rewrite(&conn, &dir);
        assert_eq!(names(&dir), [weekly]);
        assert_eq!(last_check(&conn), Some(carried));
        if cheap_clones() {
            assert!(
                weekly_due(&conn, T0),
                "the rewritten file is checked at once"
            );
        }
    }

    /// The real sequence: a check fails, two rewrites follow with no check in between, and the
    /// weekly snapshot is still there; a check that then passes (on the repaired file) and a
    /// rewrite remove it. A failed check whose record could not be written (another process
    /// held the write lock) keeps it through a rewrite in this process too.
    #[test]
    fn a_failed_check_keeps_the_weekly_snapshot_until_a_check_passes() {
        let dir = TestDir::new("backup_keep_until_pass");
        {
            let conn = index_with(&dir, 30);
            set_id(&conn, ID);
        }
        let weekly = named(T0, Reason::Weekly, Some(ID));
        plant(&dir, &weekly, &["index.db"]);
        let repair = damage_messages(&dir);
        assert_eq!(
            run_weekly_with(&dir, T0 + 8 * DAY, &|_| None, &|_| Ok(())),
            Weekly::CheckFailed
        );
        let conn = Connection::open(&*dir).unwrap();
        after_rewrite(&conn, &dir);
        after_rewrite(&conn, &dir);
        assert_eq!(
            names(&dir),
            std::slice::from_ref(&weekly),
            "kept through both rewrites"
        );
        drop(conn);
        repair();
        let passed = run_weekly_with(&dir, T0 + 9 * DAY, &|_| None, &|_| Ok(()));
        assert!(
            matches!(passed, Weekly::Taken(_) | Weekly::Skipped),
            "{passed:?}"
        );
        let conn = Connection::open(&*dir).unwrap();
        assert!(last_check(&conn).is_some_and(|c| c.ok && c.at == T0 + 9 * DAY));
        after_rewrite(&conn, &dir);
        assert!(
            !names(&dir).contains(&weekly),
            "a check passed: the rewrite drops it"
        );
        assert_eq!(names(&dir), Vec::<String>::new());

        // The failed record cannot be written: only this process knows.
        let weekly = named(T0 + 10 * DAY, Reason::Weekly, Some(ID));
        plant(&dir, &weekly, &["index.db"]);
        drop(conn);
        let repair = damage_messages(&dir);
        let holder = Connection::open(&*dir).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE;").unwrap();
        assert_eq!(
            run_weekly_with(&dir, T0 + 11 * DAY, &|_| None, &|_| Ok(())),
            Weekly::CheckFailed
        );
        holder.execute_batch("ROLLBACK;").unwrap();
        let conn = Connection::open(&*dir).unwrap();
        assert!(
            last_check(&conn).is_none_or(|c| c.ok),
            "the failure never reached app_config"
        );
        after_rewrite(&conn, &dir);
        assert_eq!(names(&dir), [weekly]);
        assert_eq!(last_check(&conn).map(|c| (c.at, c.ok)), Some((0, false)));
        drop((conn, holder));
        repair();
    }

    /// Short on disk, nothing is ever deleted to make room. A weekly snapshot is still taken
    /// while the index has one already (the new clone is free), and the old one goes only once
    /// the new one is verified; with none, or before an upgrade, none is taken and every
    /// snapshot stays.
    #[test]
    fn low_disk_space_deletes_nothing_and_takes_only_a_replacing_weekly() {
        let dir = TestDir::new("backup_room");
        let conn = index_with(&dir, 1);
        set_id(&conn, ID);
        drop(conn);
        let older_pre = named(T0 - DAY, Reason::PreMigration, Some(ID));
        let manual = named(T0 - DAY, Reason::Manual, Some(ID));
        let earlier = named(T0 - 2 * DAY, Reason::Weekly, Some(OTHER));
        for name in [&older_pre, &manual, &earlier] {
            plant(&dir, name, &["index.db"]);
        }
        let full = |_: &Path| Some(0);
        let attempt = |reason: Reason, at: i64| {
            let conn = open_live(&dir, Duration::from_secs(1)).unwrap();
            conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
            let taken = auto_take_locked(&conn, &dir, reason, at, ID, &full);
            conn.execute_batch("ROLLBACK;").unwrap();
            taken
        };
        let mut before = names(&dir);
        before.sort();
        let sorted_names = || {
            let mut v = names(&dir);
            v.sort();
            v
        };
        // No weekly snapshot of this index (an earlier index's does not count).
        assert!(attempt(Reason::Weekly, T0).is_err());
        assert_eq!(sorted_names(), before, "none taken, none deleted");
        assert!(attempt(Reason::PreMigration, T0).is_err());
        assert_eq!(sorted_names(), before, "none before an upgrade either");
        assert_eq!(
            run_weekly_with(&dir, T0, &full, &|_| Ok(())),
            Weekly::Skipped
        );
        assert_eq!(sorted_names(), before);
        // With room it is taken; then short again, the next weekly replaces it.
        if cheap_clones() {
            let room = TestDir::new("backup_room_weekly");
            let conn = index_with(&room, 1);
            set_id(&conn, ID);
            drop(conn);
            let decoys = [
                named(T0 - DAY, Reason::Manual, Some(ID)),
                named(T0 - 2 * DAY, Reason::Weekly, Some(OTHER)),
            ];
            for name in &decoys {
                plant(&room, name, &["index.db"]);
            }
            let Weekly::Taken(first) = run_weekly_with(&room, T0, &|_| None, &clone_probe) else {
                panic!("taken with room");
            };
            let conn = open_live(&room, Duration::from_secs(1)).unwrap();
            conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
            let taken = auto_take_locked(&conn, &room, Reason::Weekly, T0 + 7 * DAY, ID, &full);
            conn.execute_batch("ROLLBACK;").unwrap();
            let taken = taken.expect("replacing one is free");
            assert!(
                names(&room).contains(&first),
                "kept until the new one is verified"
            );
            let new = taken.verify().unwrap();
            prune(&room, Some(ID), T0 + 7 * DAY);
            assert!(!names(&room).contains(&first));
            // The whole run does the same.
            let Weekly::Taken(third) = run_weekly_with(&room, T0 + 14 * DAY, &full, &clone_probe)
            else {
                panic!("taken on a full disk while one exists");
            };
            assert!(!names(&room).contains(&new.name));
            let mut want: Vec<String> = decoys.to_vec();
            want.push(third);
            want.sort();
            let mut left = names(&room);
            left.sort();
            assert_eq!(left, want);
        }
    }

    /// No other process has the index open.
    fn nobody(_: &Path) -> Option<Vec<u32>> {
        Some(Vec::new())
    }

    /// Overwrite the `messages` table's root page of the closed index `db_path` (its WAL
    /// folded in first): the file still opens, reads its schema version and counts its
    /// threads, but `PRAGMA quick_check` finds the damage. Returns what writes the page back
    /// (on the closed index, `messages` untouched since), which repairs it.
    fn damage_messages(db_path: &Path) -> impl FnOnce() {
        let (root, page): (i64, i64) = {
            let c = Connection::open(db_path).unwrap();
            c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
                .unwrap();
            (
                c.query_row(
                    "SELECT rootpage FROM sqlite_master WHERE name = 'messages'",
                    [],
                    |r| r.get(0),
                )
                .unwrap(),
                c.query_row("PRAGMA page_size", [], |r| r.get(0)).unwrap(),
            )
        };
        use std::io::{Read, Seek, SeekFrom, Write};
        let at = ((root - 1) * page) as u64;
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(db_path)
            .unwrap();
        let mut original = vec![0u8; page as usize];
        f.seek(SeekFrom::Start(at)).unwrap();
        f.read_exact(&mut original).unwrap();
        f.seek(SeekFrom::Start(at)).unwrap();
        f.write_all(&vec![0xa5_u8; page as usize]).unwrap();
        let path = db_path.to_path_buf();
        move || {
            let mut f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
            f.seek(SeekFrom::Start(at)).unwrap();
            f.write_all(&original).unwrap();
        }
    }

    /// A snapshot folder holding a clean copy of an index with `n` threads (no WAL), as
    /// `name` under the snapshots folder of `index`.
    fn real_snapshot(index: &Path, folder: &str, n: usize) -> PathBuf {
        let src = TestDir::new("backup_src");
        drop(index_with(&src, n));
        let dir = plant(index, folder, &[]);
        std::fs::copy(&*src, dir.join(index.file_name().unwrap())).unwrap();
        dir
    }

    /// A live index at `index` with `n` threads, closed, then damaged: its bytes overwritten
    /// and a `-wal` and `-shm` beside it. Returns the three files' bytes.
    fn damaged_index(index: &Path, n: usize) -> [Vec<u8>; 3] {
        drop(index_with(index, n));
        std::fs::write(index, vec![0x5a_u8; 16_384]).unwrap();
        std::fs::write(sibling(index, "-wal"), b"wal bytes").unwrap();
        std::fs::write(sibling(index, "-shm"), b"shm bytes").unwrap();
        [
            std::fs::read(index).unwrap(),
            std::fs::read(sibling(index, "-wal")).unwrap(),
            std::fs::read(sibling(index, "-shm")).unwrap(),
        ]
    }

    /// Everything in the index folder except the snapshots, by name.
    fn folder_listing(index: &Path) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(index.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n != SNAPSHOTS_DIR)
            .collect();
        out.sort();
        out
    }

    /// The restore puts the snapshot in place and keeps all three damaged files under the
    /// `.damaged-<stamp>` names, byte for byte; decoys next to them and the snapshot itself
    /// are untouched, and no staged file is left behind.
    #[test]
    fn a_restore_brings_the_snapshot_back_and_keeps_the_damaged_files() {
        let dir = TestDir::new("backup_restore");
        let folder = format!("{}-weekly", stamp(T0));
        let snap = real_snapshot(&dir, &folder, 3);
        let snap_bytes = std::fs::read(snap.join("index.db")).unwrap();
        let [db_bytes, wal_bytes, shm_bytes] = damaged_index(&dir, 5);
        let parent = dir.parent().unwrap();
        std::fs::write(parent.join("index.db.damaged-older"), b"decoy").unwrap();
        std::fs::write(parent.join("index.dev.db"), b"decoy dev").unwrap();
        std::fs::write(parent.join("notes.txt"), b"decoy notes").unwrap();

        let at = T0 + DAY;
        let restored = restore_with(&dir, &folder, at, &|_| None, &nobody).unwrap();
        let aside = sibling(&dir, &format!(".damaged-{}", stamp(at)));
        assert_eq!(restored.damaged.as_deref(), Some(aside.as_path()));
        assert_eq!(threads_in(&dir), 3, "the snapshot is the index now");
        assert_eq!(std::fs::read(&aside).unwrap(), db_bytes);
        assert_eq!(std::fs::read(sibling(&aside, "-wal")).unwrap(), wal_bytes);
        assert_eq!(std::fs::read(sibling(&aside, "-shm")).unwrap(), shm_bytes);
        assert_eq!(
            std::fs::read(parent.join("index.db.damaged-older")).unwrap(),
            b"decoy"
        );
        assert_eq!(
            std::fs::read(parent.join("index.dev.db")).unwrap(),
            b"decoy dev"
        );
        assert_eq!(
            std::fs::read(parent.join("notes.txt")).unwrap(),
            b"decoy notes"
        );
        assert_eq!(std::fs::read(snap.join("index.db")).unwrap(), snap_bytes);
        assert!(
            !folder_listing(&dir)
                .iter()
                .any(|n| n.contains(".restoring-")),
            "{:?}",
            folder_listing(&dir)
        );
        #[cfg(unix)]
        assert_eq!(mode(&dir), 0o600);
        // It opens as a normal, writable index afterwards.
        let conn = db::open(&dir).unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM threads", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 3);
    }

    /// Refusals happen before a live file is touched: another process's lock, another process
    /// with the index open, a snapshot that does not open (with no older one to fall back on),
    /// a name that is not a snapshot.
    #[test]
    fn a_refused_restore_leaves_every_file_as_it_was() {
        let dir = TestDir::new("backup_refuse");
        let good = format!("{}-weekly", stamp(T0));
        real_snapshot(&dir, &good, 2);
        // Older than `good`, so restoring it has nothing older to fall back on.
        let bad = format!("{}-manual", stamp(T0 - DAY));
        plant(&dir, &bad, &["index.db"]);
        let bytes = damaged_index(&dir, 4);
        let unchanged = |what: &str, before: &[String]| {
            assert_eq!(folder_listing(&dir), before, "{what}");
            assert_eq!(std::fs::read(&*dir).unwrap(), bytes[0], "{what}");
            assert_eq!(
                std::fs::read(sibling(&dir, "-wal")).unwrap(),
                bytes[1],
                "{what}"
            );
            assert_eq!(
                std::fs::read(sibling(&dir, "-shm")).unwrap(),
                bytes[2],
                "{what}"
            );
        };
        let before = folder_listing(&dir);

        let crate::locks::Acquire::Acquired(watch) =
            crate::locks::acquire(&dir, crate::locks::WATCH_LOCK).unwrap()
        else {
            panic!("the lock is free");
        };
        let err = restore_with(&dir, &good, T0 + 2 * DAY, &|_| None, &nobody).unwrap_err();
        assert!(format!("{err:#}").contains("watch.lock"), "{err:#}");
        let with_locks: Vec<String> = before
            .iter()
            .cloned()
            .chain(["index.db.rewrite.lock".into(), "index.db.watch.lock".into()])
            .collect();
        unchanged("another process's lock", &{
            let mut v = with_locks.clone();
            v.sort();
            v
        });
        drop(watch);
        assert!(crate::locks::wait_until_free(
            &dir,
            crate::locks::WATCH_LOCK,
            Duration::from_secs(5)
        ));
        // From here the lock files are part of the folder.
        let before = folder_listing(&dir);
        // A process with the index open but no lock (an MCP server): refused before staging,
        // which takes a minute on a large index. A snapshot that cannot be staged shows it:
        // staging first would refuse it for its own error.
        for snap in [&good, &bad] {
            let err = restore_with(&dir, snap, T0 + 2 * DAY, &|_| None, &|_| Some(vec![4242]))
                .unwrap_err();
            assert!(format!("{err:#}").contains("pid 4242"), "{snap}: {err:#}");
            unchanged("another process has the index open", &before);
        }
        // Checked again right before the swap, for a process that opened the index during
        // staging: refused, and the staged copy is gone.
        let calls = std::cell::Cell::new(0);
        let opened_meanwhile = |_: &Path| {
            calls.set(calls.get() + 1);
            Some(if calls.get() == 1 {
                Vec::new()
            } else {
                vec![4343]
            })
        };
        let err =
            restore_with(&dir, &good, T0 + 2 * DAY, &|_| None, &opened_meanwhile).unwrap_err();
        assert!(format!("{err:#}").contains("pid 4343"), "{err:#}");
        assert_eq!(calls.get(), 2);
        unchanged("a process opened the index during staging", &before);
        // Nothing can tell whether one has (no lsof): refused too, before staging, saying how
        // to restore by hand, except on Windows, where the renames fail under an open handle.
        if !cfg!(windows) {
            for snap in [&good, &bad] {
                let err = restore_with(&dir, snap, T0 + 2 * DAY, &|_| None, &|_| None).unwrap_err();
                let text = format!("{err:#}");
                assert!(text.contains("cannot tell"), "{snap}: {text}");
                assert!(text.contains("not touched"), "{text}");
                assert!(
                    text.contains(&snapshots_dir(&dir).join(snap).display().to_string()),
                    "{text}"
                );
                unchanged("nothing can tell who has the index open", &before);
            }
        }
        let err = restore_with(&dir, &bad, T0 + 2 * DAY, &|_| None, &nobody).unwrap_err();
        assert!(format!("{err:#}").contains("not touched"), "{err:#}");
        unchanged("a snapshot that does not open", &before);
        assert!(restore_with(&dir, "20261101T000000Z-nope", T0, &|_| None, &nobody).is_err());
        unchanged("no such snapshot", &before);
        // Short on disk where it has to copy instead of clone.
        if !cheap_clones() {
            assert!(restore_with(&dir, &good, T0 + 3 * DAY, &|_| Some(0), &nobody).is_err());
            unchanged("no room for the copy", &before);
        }
    }

    /// A snapshot that opens and counts its threads but fails `PRAGMA quick_check` (a copy of
    /// an already damaged index) is never put in place: the restore falls back to the next
    /// older one and says which it skipped, and with none left it refuses.
    #[test]
    fn a_restore_skips_a_damaged_snapshot_for_an_older_sound_one() {
        let dir = TestDir::new("backup_fallback");
        let sound = format!("{}-weekly", stamp(T0));
        real_snapshot(&dir, &sound, 2);
        let copied = format!("{}-pre-migration", stamp(T0 + DAY));
        let snap = real_snapshot(&dir, &copied, 6);
        let _ = damage_messages(&snap.join("index.db"));
        assert_eq!(
            threads_in(&snap.join("index.db")),
            6,
            "it still reads as an index"
        );
        let bytes = damaged_index(&dir, 4);

        // Nothing older than the damaged copy: refused, nothing touched.
        let alone = TestDir::new("backup_fallback_alone");
        let only = real_snapshot(&alone, &copied, 6);
        let _ = damage_messages(&only.join("index.db"));
        let alone_bytes = damaged_index(&alone, 1);
        let err = restore_with(&alone, &copied, T0 + 2 * DAY, &|_| None, &nobody).unwrap_err();
        assert!(format!("{err:#}").contains("quick_check"), "{err:#}");
        assert_eq!(std::fs::read(&*alone).unwrap(), alone_bytes[0]);
        assert!(!folder_listing(&alone)
            .iter()
            .any(|n| n.contains(".restoring-") || n.contains(".damaged-")));

        let restored = restore_with(&dir, &copied, T0 + 2 * DAY, &|_| None, &nobody).unwrap();
        assert_eq!(restored.snapshot.name, sound);
        assert_eq!(
            restored
                .skipped
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            [copied.as_str()]
        );
        assert_eq!(threads_in(&dir), 2);
        let aside = restored.damaged.unwrap();
        assert_eq!(std::fs::read(&aside).unwrap(), bytes[0]);
        assert!(!folder_listing(&dir)
            .iter()
            .any(|n| n.contains(".restoring-")));
    }

    /// Every automatic snapshot carries a `-wal`: the restore folds it in, so commits that
    /// only the snapshot's WAL held are in the restored index, and no WAL is left beside it.
    #[test]
    fn a_snapshot_with_a_wal_restores_with_its_wal_commits() {
        let dir = TestDir::new("backup_restore_wal");
        let folder = format!("{}-weekly", stamp(T0));
        let snap_dir = {
            let src = TestDir::new("backup_restore_wal_src");
            let mut conn = index_with(&src, 2);
            // Commits from here on stay in the WAL: no autocheckpoint, connection open.
            conn.pragma_update(None, "wal_autocheckpoint", 0_i64)
                .unwrap();
            let sid = source_id(&conn, "claude_code").unwrap();
            for i in 0..5 {
                upsert_thread(&mut conn, sid, &thread(&format!("w{i}"))).unwrap();
            }
            let wal = sibling(&src, "-wal");
            assert!(std::fs::metadata(&wal).unwrap().len() > 0);
            let d = plant(&dir, &folder, &[]);
            std::fs::copy(&*src, d.join("index.db")).unwrap();
            std::fs::copy(&wal, d.join("index.db-wal")).unwrap();
            // As copied, without the WAL, the file holds fewer threads than with it.
            let bare = TestDir::new("backup_restore_wal_bare");
            std::fs::copy(&*src, &*bare).unwrap();
            assert!(threads_in(&bare) < 7, "the WAL holds the newest commits");
            d
        };
        let snap_wal = std::fs::read(snap_dir.join("index.db-wal")).unwrap();
        damaged_index(&dir, 1);

        let restored = restore_with(&dir, &folder, T0 + DAY, &|_| None, &nobody).unwrap();
        assert!(restored.skipped.is_empty());
        assert!(!sibling(&dir, "-wal").exists(), "the WAL was folded in");
        assert!(!sibling(&dir, "-shm").exists());
        assert_eq!(threads_in(&dir), 7, "with the WAL's commits");
        assert_eq!(
            std::fs::read(snap_dir.join("index.db-wal")).unwrap(),
            snap_wal,
            "the snapshot's own WAL is untouched"
        );
    }

    /// The swap without hard links (two renames) ends like the one with them, and a swap
    /// whose last step fails puts every live file back, with or without hard links.
    #[test]
    fn a_swap_without_hard_links_or_with_a_failing_rename_is_whole_or_undone() {
        let no_link: LinkFn = |_, _| Err(std::io::Error::from(std::io::ErrorKind::Unsupported));
        let hard_link: LinkFn = |a, b| std::fs::hard_link(a, b);
        let setup = |tag: &str| {
            let dir = TestDir::new(tag);
            for (suffix, body) in [("", "live db"), ("-wal", "live wal"), ("-shm", "live shm")] {
                std::fs::write(sibling(&dir, suffix), body).unwrap();
            }
            let staged = sibling(&dir, ".restoring-x");
            let aside = sibling(&dir, ".damaged-x");
            (dir, staged, aside)
        };

        let (dir, staged, aside) = setup("backup_swap_nolink");
        std::fs::write(&staged, b"snapshot").unwrap();
        assert!(swap_in(&dir, &staged, &aside, no_link).unwrap());
        assert_eq!(std::fs::read(&*dir).unwrap(), b"snapshot");
        assert!(!staged.exists());
        for (suffix, body) in [("", "live db"), ("-wal", "live wal"), ("-shm", "live shm")] {
            assert_eq!(
                std::fs::read(sibling(&aside, suffix)).unwrap(),
                body.as_bytes()
            );
            if !suffix.is_empty() {
                assert!(!sibling(&dir, suffix).exists());
            }
        }

        for (tag, link) in [
            ("backup_swap_undo_link", hard_link),
            ("backup_swap_undo_nolink", no_link),
        ] {
            // No staged file: the final rename fails.
            let (dir, staged, aside) = setup(tag);
            let before = folder_listing(&dir);
            assert!(swap_in(&dir, &staged, &aside, link).is_err(), "{tag}");
            assert_eq!(folder_listing(&dir), before, "{tag}");
            for (suffix, body) in [("", "live db"), ("-wal", "live wal"), ("-shm", "live shm")] {
                assert_eq!(
                    std::fs::read(sibling(&dir, suffix)).unwrap(),
                    body.as_bytes(),
                    "{tag}"
                );
            }
        }

        // A move aside that fails after the link was made (on Windows: another process's
        // handle on the `-wal`; here a folder in the way) leaves no second name of the index,
        // which would read as a damaged copy a restore kept.
        for (tag, link) in [
            ("backup_swap_wal_link", hard_link),
            ("backup_swap_wal_nolink", no_link),
        ] {
            let (dir, staged, aside) = setup(tag);
            std::fs::write(&staged, b"snapshot").unwrap();
            let in_the_way = sibling(&aside, "-wal");
            std::fs::create_dir_all(in_the_way.join("inside")).unwrap();
            let before = folder_listing(&dir);
            assert!(swap_in(&dir, &staged, &aside, link).is_err(), "{tag}");
            assert_eq!(folder_listing(&dir), before, "{tag}");
            assert!(std::fs::symlink_metadata(&aside).is_err(), "{tag}");
            assert!(in_the_way.join("inside").is_dir(), "{tag}");
            for (suffix, body) in [("", "live db"), ("-wal", "live wal"), ("-shm", "live shm")] {
                assert_eq!(
                    std::fs::read(sibling(&dir, suffix)).unwrap(),
                    body.as_bytes(),
                    "{tag}"
                );
            }
        }
    }

    /// A pre-migration snapshot replaces the older one only once the upgrade went through,
    /// and the launch that took it never offers it for a restore; one whose upgrade failed is
    /// removed and the older one stays. An earlier index's pre-migration snapshot is never
    /// replaced.
    #[test]
    fn a_pre_migration_snapshot_waits_for_the_upgrade_before_replacing_the_older_one() {
        let dir = TestDir::new("backup_pre_done");
        let older = named(T0, Reason::PreMigration, Some(ID));
        real_snapshot(&dir, &older, 1);
        let earlier = named(T0 - DAY, Reason::PreMigration, Some(OTHER));
        real_snapshot(&dir, &earlier, 1);
        let pending = |at: i64| {
            let name = named(at, Reason::PreMigration, Some(ID));
            real_snapshot(&dir, &name, 2);
            let snapshot = list(&dir).into_iter().find(|s| s.name == name).unwrap();
            PreMigration {
                index: dir.to_path_buf(),
                snapshot,
            }
        };

        let failed = pending(T0 + DAY);
        pre_migration_done(Some(failed), false);
        assert_eq!(
            names(&dir),
            [older.clone(), earlier.clone()],
            "the failed upgrade's copy went"
        );

        let done = pending(T0 + 2 * DAY);
        let done_name = done.snapshot.name.clone();
        pre_migration_done(Some(done), true);
        assert_eq!(
            names(&dir),
            [done_name, earlier.clone()],
            "it replaced the older one, not the earlier index's"
        );
        let offered: Vec<String> = restore_candidates(&dir)
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(
            offered,
            std::slice::from_ref(&earlier),
            "this process took it: not offered"
        );
        let other = named(T0 + 3 * DAY, Reason::Weekly, Some(ID));
        real_snapshot(&dir, &other, 1);
        let offered: Vec<String> = restore_candidates(&dir)
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(offered, [other, earlier]);
    }

    /// Two processes upgrading at once each took a pre-migration snapshot in the same second
    /// (A, then A-2 for the second folder): whichever finishes first, the newest one and only
    /// it remains once both are done, and the older one goes.
    #[test]
    fn concurrent_upgrades_keep_the_newest_pre_migration_snapshot() {
        for first_done in [0, 1] {
            let dir = TestDir::new("backup_pre_race");
            let old = named(T0 - DAY, Reason::PreMigration, Some(ID));
            real_snapshot(&dir, &old, 1);
            let a = named(T0, Reason::PreMigration, Some(ID));
            let a2 = format!("{a}-2");
            for name in [&a, &a2] {
                real_snapshot(&dir, name, 2);
            }
            let pre = |name: &str| PreMigration {
                index: dir.to_path_buf(),
                snapshot: list(&dir).into_iter().find(|s| s.name == name).unwrap(),
            };
            let mut both = vec![pre(&a), pre(&a2)];
            let second = both.remove(1 - first_done);
            let first = both.remove(0);
            pre_migration_done(Some(first), true);
            pre_migration_done(Some(second), true);
            assert_eq!(
                names(&dir),
                std::slice::from_ref(&a2),
                "first done: {first_done}"
            );
        }
    }

    /// What a restore left aside is counted with its sidecars; look-alikes and another
    /// index's files are not.
    #[test]
    fn damaged_copies_kept_by_a_restore_are_counted_with_their_sidecars() {
        let dir = TestDir::new("backup_kept_damaged");
        assert_eq!(kept_damaged(&dir), (0, 0));
        let folder = dir.parent().unwrap();
        let aside = format!("index.db.damaged-{}", stamp(T0));
        for (file, len) in [
            (aside.clone(), 10usize),
            (format!("{aside}-wal"), 4),
            (format!("{aside}-shm"), 2),
            // Not ours: no time stamp, another index's copy, a note the user left.
            ("index.db.damaged-notes.txt".to_string(), 100),
            (format!("index.dev.db.damaged-{}", stamp(T0)), 100),
            (format!("{aside}.txt"), 100),
        ] {
            std::fs::write(folder.join(file), vec![0u8; len]).unwrap();
        }
        assert_eq!(kept_damaged(&dir), (1, 16));
        assert_eq!(staged_leftovers(&dir), (0, 0), "a different kind");
        let staged = format!("index.db.restoring-{}", stamp(T0));
        std::fs::write(folder.join(&staged), [0u8; 7]).unwrap();
        std::fs::write(folder.join(format!("{staged}-wal")), [0u8; 3]).unwrap();
        assert_eq!(staged_leftovers(&dir), (1, 10));
        assert_eq!(kept_damaged(&dir), (1, 16));
    }

    /// Leftovers of an interrupted restore, and the look-alikes next to them that are not ones
    /// (a symbolic link among them points at `outside`, a file of another folder). Returns the
    /// leftovers' paths and the decoys' (path, bytes).
    fn plant_staged(
        index: &Path,
        at: i64,
        outside: &Path,
    ) -> (Vec<PathBuf>, Vec<(PathBuf, Vec<u8>)>) {
        let folder = index.parent().unwrap();
        let staged = sibling(index, &format!(".restoring-{}", stamp(at)));
        let leftovers: Vec<PathBuf> = ["", "-wal", "-shm"]
            .iter()
            .map(|s| sibling(&staged, s))
            .collect();
        for p in &leftovers {
            std::fs::write(p, b"staged").unwrap();
        }
        let outside = outside.to_path_buf();
        let mut decoys = vec![
            (
                folder.join(format!("index.dev.db.restoring-{}", stamp(at))),
                b"another index".to_vec(),
            ),
            (
                folder.join("index.db.restoring-notes"),
                b"no stamp".to_vec(),
            ),
            (
                folder.join(format!("index.db.restoring-{}.txt", stamp(at))),
                b"not a sidecar".to_vec(),
            ),
            (outside.clone(), b"not yours".to_vec()),
        ];
        for (p, bytes) in &decoys {
            std::fs::write(p, bytes).unwrap();
        }
        // A folder and a symbolic link under names that do parse.
        let as_folder = folder.join(format!("index.db.restoring-{}", stamp(at - 1)));
        std::fs::create_dir_all(&as_folder).unwrap();
        std::fs::write(as_folder.join("inside"), b"kept").unwrap();
        decoys.push((as_folder.join("inside"), b"kept".to_vec()));
        #[cfg(unix)]
        {
            let link = folder.join(format!("index.db.restoring-{}", stamp(at - 2)));
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            decoys.push((link, b"not yours".to_vec()));
        }
        (leftovers, decoys)
    }

    /// A restore first removes the staged copy (and its `-wal`, `-shm`) that an interrupted
    /// restore left, and nothing that only looks like one: another index's, a name whose stamp
    /// does not parse, a folder, a symbolic link (its target untouched).
    #[test]
    fn a_restore_removes_what_an_interrupted_one_staged() {
        let dir = TestDir::new("backup_staged");
        let outside = TestDir::new("backup_staged_outside");
        let folder = named(T0, Reason::Weekly, None);
        real_snapshot(&dir, &folder, 3);
        damaged_index(&dir, 5);
        let (leftovers, decoys) = plant_staged(&dir, T0 + DAY, &outside);
        let restored = restore_with(&dir, &folder, T0 + 2 * DAY, &|_| None, &nobody).unwrap();
        assert_eq!(threads_in(&dir), 3);
        assert!(restored.damaged.is_some());
        for p in &leftovers {
            assert!(std::fs::symlink_metadata(p).is_err(), "{}", p.display());
        }
        for (p, bytes) in &decoys {
            assert_eq!(&std::fs::read(p).unwrap(), bytes, "{}", p.display());
        }
        assert_eq!(staged_leftovers(&dir), (0, 0));
    }

    /// The app's tick removes a staged copy only once it is a day old: a younger one may be a
    /// restore still running in another process. Decoys stay either way.
    #[test]
    fn the_tick_removes_only_day_old_staged_copies() {
        let dir = TestDir::new("backup_staged_tick");
        let outside = TestDir::new("backup_staged_tick_outside");
        let (leftovers, decoys) = plant_staged(&dir, T0, &outside);
        assert_eq!(remove_stale_staged(&dir, T0 + DAY - 1), 0);
        for p in &leftovers {
            assert!(p.exists(), "{}", p.display());
        }
        assert_eq!(staged_leftovers(&dir), (1, 18));
        assert_eq!(remove_stale_staged(&dir, T0 + DAY), 3);
        for p in &leftovers {
            assert!(std::fs::symlink_metadata(p).is_err(), "{}", p.display());
        }
        for (p, bytes) in &decoys {
            assert_eq!(&std::fs::read(p).unwrap(), bytes, "{}", p.display());
        }
    }

    /// Put the live files of `index` in the state a swap interrupted after moving the `-wal`
    /// aside leaves: `aside` is a second name of the index file and holds the `-wal`; the
    /// `-shm` moved too when `shm_moved`.
    fn interrupt_swap(index: &Path, aside: &Path, shm_moved: bool) {
        std::fs::hard_link(index, aside).unwrap();
        std::fs::rename(sibling(index, "-wal"), sibling(aside, "-wal")).unwrap();
        if shm_moved {
            std::fs::rename(sibling(index, "-shm"), sibling(aside, "-shm")).unwrap();
        }
    }

    /// Exactly the state a swap interrupted after its `-wal` moved reads as an interrupted
    /// restore: not after a completed restore or one that had no index to move, not while the
    /// index still has its `-wal` next to it, not for files merely named like an aside copy.
    #[cfg(unix)]
    #[test]
    fn an_interrupted_restore_is_found_only_in_its_exact_state() {
        let dir = TestDir::new("backup_interrupted");
        assert_eq!(interrupted_restore(&dir), None, "no index");
        damaged_index(&dir, 2);
        assert_eq!(interrupted_restore(&dir), None, "a live pair");
        let aside = sibling(&dir, &format!(".damaged-{}", stamp(T0)));
        // A hard link while the `-wal` is still in place: the first step only.
        std::fs::hard_link(&*dir, &aside).unwrap();
        assert_eq!(
            interrupted_restore(&dir),
            None,
            "the -wal is still in place"
        );
        std::fs::rename(sibling(&dir, "-wal"), sibling(&aside, "-wal")).unwrap();
        assert_eq!(interrupted_restore(&dir), Some(aside.clone()));
        std::fs::rename(sibling(&dir, "-shm"), sibling(&aside, "-shm")).unwrap();
        assert_eq!(interrupted_restore(&dir), Some(aside.clone()));
        // A copy (not the same file) under the aside name, with a -wal: not ours to guess.
        let copied = TestDir::new("backup_interrupted_copy");
        damaged_index(&copied, 1);
        let copy_aside = sibling(&copied, &format!(".damaged-{}", stamp(T0)));
        std::fs::copy(&*copied, &copy_aside).unwrap();
        std::fs::rename(sibling(&copied, "-wal"), sibling(&copy_aside, "-wal")).unwrap();
        assert_eq!(
            interrupted_restore(&copied),
            None,
            "a copy, not a hard link"
        );
        // A completed restore.
        let done = TestDir::new("backup_interrupted_done");
        let folder = named(T0, Reason::Weekly, None);
        real_snapshot(&done, &folder, 3);
        damaged_index(&done, 1);
        restore_with(&done, &folder, T0 + DAY, &|_| None, &nobody).unwrap();
        assert_eq!(
            interrupted_restore(&done),
            None,
            "after a completed restore"
        );
        // A restore that had no index to move aside.
        let empty = TestDir::new("backup_interrupted_none");
        real_snapshot(&empty, &folder, 3);
        let restored = restore_with(&empty, &folder, T0 + DAY, &|_| None, &nobody).unwrap();
        assert_eq!(restored.damaged, None);
        assert_eq!(
            interrupted_restore(&empty),
            None,
            "after a restore that moved nothing"
        );
    }

    /// Restoring again from an interrupted swap finishes it: the snapshot is in place and the
    /// damaged files are one complete pair under the first aside name, byte for byte, with no
    /// second aside name made.
    #[cfg(unix)]
    #[test]
    fn restoring_again_finishes_an_interrupted_swap() {
        for shm_moved in [false, true] {
            let dir = TestDir::new("backup_resume");
            let folder = named(T0, Reason::Weekly, None);
            real_snapshot(&dir, &folder, 3);
            let [db_bytes, wal_bytes, shm_bytes] = damaged_index(&dir, 5);
            let aside = sibling(&dir, &format!(".damaged-{}", stamp(T0 + DAY)));
            interrupt_swap(&dir, &aside, shm_moved);
            assert_eq!(interrupted_restore(&dir), Some(aside.clone()));
            let restored = restore_with(&dir, &folder, T0 + 2 * DAY, &|_| None, &nobody).unwrap();
            assert_eq!(restored.damaged.as_deref(), Some(aside.as_path()));
            assert_eq!(threads_in(&dir), 3, "the snapshot is the index now");
            assert_eq!(std::fs::read(&aside).unwrap(), db_bytes);
            assert_eq!(std::fs::read(sibling(&aside, "-wal")).unwrap(), wal_bytes);
            assert_eq!(std::fs::read(sibling(&aside, "-shm")).unwrap(), shm_bytes);
            assert!(!same_file(&dir, &aside));
            let listing = folder_listing(&dir);
            assert_eq!(
                listing.iter().filter(|n| n.contains(".damaged-")).count(),
                3,
                "{listing:?}"
            );
            assert!(
                !listing.iter().any(|n| n.contains(".restoring-")),
                "{listing:?}"
            );
            assert_eq!(interrupted_restore(&dir), None);
        }
    }

    /// Every open outside the running app refuses an index a restore left halfway through its
    /// swap, as `SQLITE_CORRUPT`, and leaves that state as it was: the app (`db::open`), `cal`
    /// write commands and `cal index` (`db::open_sidecar`), hooks and the MCP server
    /// (`db::open_sidecar_current_with`), `cal hook`, `cal doctor` and the queries
    /// (`db::open_readonly`), `cal backup` and snapshots ([`open_live`]). Opening would read
    /// the file without the commits only the aside `-wal` holds and give it a new `-wal`, which
    /// hides the state; a read-only open did too while the `-shm` was beside the index (it read
    /// 2 of the 6 threads). Restoring then finishes the swap, the damaged pair whole.
    #[cfg(unix)]
    #[test]
    fn every_writable_open_refuses_an_interrupted_restore() {
        for shm_moved in [false, true] {
            let dir = TestDir::new("backup_interrupted_opens");
            let folder = named(T0, Reason::Manual, None);
            real_snapshot(&dir, &folder, 3);
            drop(index_with(&dir, 2));
            // Commits only the WAL holds, kept on close as a process killed mid-run keeps them.
            {
                let mut conn = db::open(&dir).unwrap();
                conn.set_db_config(
                    rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
                    true,
                )
                .unwrap();
                let sid = source_id(&conn, "claude_code").unwrap();
                for i in 0..4 {
                    upsert_thread(&mut conn, sid, &thread(&format!("w{i}"))).unwrap();
                }
            }
            assert!(std::fs::metadata(sibling(&dir, "-wal")).unwrap().len() > 0);
            let aside = sibling(&dir, &format!(".damaged-{}", stamp(T0 + DAY)));
            interrupt_swap(&dir, &aside, shm_moved);
            let aside_wal = sibling(&aside, "-wal");
            let wal = std::fs::read(&aside_wal).unwrap();
            let still = |what: &str| {
                assert_eq!(interrupted_restore(&dir), Some(aside.clone()), "{what}");
                assert!(
                    std::fs::symlink_metadata(sibling(&dir, "-wal")).is_err(),
                    "{what}"
                );
                assert_eq!(std::fs::read(&aside_wal).unwrap(), wal, "{what}");
            };
            let refused = |what: &str, opened: Result<Connection>| {
                let err = opened
                    .err()
                    .unwrap_or_else(|| panic!("{what} opened the index"));
                let code = err
                    .chain()
                    .find_map(|c| c.downcast_ref::<rusqlite::Error>())
                    .and_then(rusqlite::Error::sqlite_error_code);
                assert_eq!(
                    code,
                    Some(rusqlite::ErrorCode::DatabaseCorrupt),
                    "{what}: {err:#}"
                );
                let text = format!("{err:#}");
                assert!(text.contains(&aside_wal.display().to_string()), "{text}");
                still(what);
            };
            let wait = Duration::from_millis(200);
            refused("db::open", db::open(&dir));
            refused("db::open_sidecar", db::open_sidecar(&dir));
            refused(
                "db::open_sidecar_current_with",
                db::open_sidecar_current_with(&dir, wait),
            );
            refused("open_live", open_live(&dir, wait));
            refused("db::open_readonly", db::open_readonly(&dir));
            assert!(snapshot(&dir, Reason::Manual, wait).is_err());
            still("a snapshot");
            assert!(backup_to(&dir, None, clone::clone_file, &|_| None, &mut |_, _| {}).is_err());
            still("cal backup");
            assert_eq!(
                names(&dir),
                std::slice::from_ref(&folder),
                "no copy of the WAL-less file"
            );

            let restored = restore_with(&dir, &folder, T0 + 2 * DAY, &|_| None, &nobody).unwrap();
            assert_eq!(restored.damaged.as_deref(), Some(aside.as_path()));
            assert_eq!(threads_in(&dir), 3, "the snapshot is the index now");
            assert_eq!(
                threads_in(&aside),
                6,
                "the damaged pair holds its WAL's commits"
            );
            drop(db::open(&dir).unwrap());
        }
    }

    /// `lsof` names another process that has the index open; without `lsof` the probe says it
    /// cannot tell. The process here holds only `index.db` while a `-wal` and `-shm` nobody
    /// has open sit next to it, the answer lsof gives with exit status 1.
    #[cfg(unix)]
    #[test]
    fn another_process_with_the_index_open_is_found() {
        let dir = TestDir::new("backup_lsof");
        drop(index_with(&dir, 1));
        for suffix in ["-wal", "-shm"] {
            std::fs::write(sibling(&dir, suffix), b"").unwrap();
        }
        assert_eq!(open_elsewhere(&dir).filter(|p| !p.is_empty()), None);
        let mut child = std::process::Command::new("sh")
            .args(["-c", "exec 3<\"$1\"; exec sleep 30", "sh"])
            .arg(&*dir)
            .spawn()
            .unwrap();
        let pid = child.id();
        let deadline = Instant::now() + Duration::from_secs(10);
        let found = loop {
            match open_elsewhere(&dir) {
                None => break None,
                Some(p) if p.contains(&pid) => break Some(p),
                Some(_) if Instant::now() > deadline => break Some(Vec::new()),
                Some(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        };
        let _ = child.kill();
        let _ = child.wait();
        match found {
            None => println!("skipped: no lsof here"),
            Some(p) => assert!(p.contains(&pid), "{p:?} lacks {pid}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_index_is_never_snapshotted_or_restored() {
        let dir = TestDir::new("backup_symlink");
        let real = TestDir::new("backup_symlink_target");
        drop(index_with(&real, 1));
        let bytes = std::fs::read(&*real).unwrap();
        std::os::unix::fs::symlink(&*real, &*dir).unwrap();
        let folder = format!("{}-weekly", stamp(T0));
        real_snapshot(&dir, &folder, 2);
        assert!(snapshot(&dir, Reason::Manual, Duration::from_secs(1)).is_err());
        assert!(restore_with(&dir, &folder, T0, &|_| None, &nobody).is_err());
        assert_eq!(std::fs::read(&*real).unwrap(), bytes);
        assert!(std::fs::symlink_metadata(&*dir)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    /// `db::migrate` snapshots an index that has something in it before upgrading it (where
    /// files clone), and the upgrade runs either way.
    #[test]
    fn an_upgrade_snapshots_the_index_first() {
        AUTOMATIC_IN_TESTS.with(|c| c.set(true));
        let dir = TestDir::new("backup_upgrade");
        let latest = {
            let fresh = TestDir::new("backup_upgrade_latest");
            let conn = db::open(&fresh).unwrap();
            conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap()
        };
        // A brand-new index has nothing to protect.
        drop(db::open(&dir).unwrap());
        assert!(list(&dir).is_empty());
        std::fs::remove_file(&*dir).unwrap();
        for s in ["-wal", "-shm"] {
            let _ = std::fs::remove_file(sibling(&dir, s));
        }
        {
            let mut conn = Connection::open(&*dir).unwrap();
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
            db::migrations::MIGRATIONS
                .to_version(&mut conn, (latest - 1) as usize)
                .unwrap();
        }
        let conn = db::open(&dir).unwrap();
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, latest, "upgraded");
        let snaps = list(&dir);
        if cheap_clones() {
            assert_eq!(snaps.len(), 1, "{snaps:?}");
            assert_eq!(snaps[0].reason, Reason::PreMigration);
            let copy = read_copy(&snaps[0].dir.join("index.db")).unwrap();
            assert_eq!(copy.user_version, latest - 1, "taken before the upgrade");
            // The identity was committed before the clone: the copy, the folder name and the
            // live index all carry the same one.
            let id = index_id(&conn).expect("the upgrade gave the index an identity");
            assert_eq!(snaps[0].lineage.as_deref(), Some(id.as_str()));
            let copy_conn = open_copy(&snaps[0].dir.join("index.db")).unwrap();
            assert_eq!(index_id(&copy_conn).as_deref(), Some(id.as_str()));
            drop(copy_conn);
            let _ = std::fs::remove_file(snaps[0].dir.join("index.db-shm"));
            assert!(
                restore_candidates(&dir).is_empty(),
                "the launch that took it never offers it"
            );
        } else {
            assert!(snaps.is_empty(), "no clones here: no automatic snapshot");
            assert_eq!(
                index_id(&conn),
                None,
                "no snapshot attempted, no identity written"
            );
        }
        AUTOMATIC_IN_TESTS.with(|c| c.set(false));
    }

    /// The fresh index the startup screen's "start fresh" leads to, at the same path as the
    /// damaged one: its weekly snapshots and retention never remove the old index's weekly
    /// and pre-migration snapshots (the only copy of the history that was lost), through two
    /// weekly runs, `prune`, a rewrite and the vector partition.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_fresh_index_never_removes_an_earlier_index_s_snapshots() {
        let dir = TestDir::new("backup_lineage");
        drop(index_with(&dir, 4));
        let Weekly::Taken(old_weekly) = run_weekly_with(&dir, T0, &|_| None, &clone_probe) else {
            panic!("the old index's weekly snapshot");
        };
        let old_id = stored_index_id(&dir).unwrap();
        let old_pre = named(T0, Reason::PreMigration, Some(&old_id));
        plant(&dir, &old_pre, &["index.db"]);
        // Start fresh: the three files move away, a new index is built at the same path.
        let moved = TestDir::new("backup_lineage_moved");
        for s in ["", "-wal", "-shm"] {
            let _ = std::fs::rename(sibling(&dir, s), sibling(&moved, s));
        }
        let conn = index_with(&dir, 1);
        assert_eq!(index_id(&conn), None);
        let old = [old_weekly.clone(), old_pre.clone()];
        let still_there = |what: &str| {
            let left = names(&dir);
            for name in &old {
                assert!(left.contains(name), "{what}: {name} gone from {left:?}");
            }
        };
        // Before the new index has an identity, nothing of the old one is its to remove.
        assert_eq!(prune(&dir, index_id(&conn).as_deref(), T0 + 30 * DAY), 0);
        let Weekly::Taken(first) = run_weekly_with(&dir, T0 + DAY, &|_| None, &clone_probe) else {
            panic!("the new index's first weekly snapshot");
        };
        let new_id = index_id(&conn).unwrap();
        assert_ne!(new_id, old_id);
        still_there("the first weekly run");
        let Weekly::Taken(second) = run_weekly_with(&dir, T0 + 9 * DAY, &|_| None, &clone_probe)
        else {
            panic!("the second weekly snapshot");
        };
        still_there("the second weekly run");
        assert!(!names(&dir).contains(&first), "its own older weekly goes");
        prune(&dir, Some(&new_id), T0 + 60 * DAY);
        still_there("prune");
        after_rewrite(&conn, &dir);
        still_there("a rewrite");
        assert!(!names(&dir).contains(&second), "its own weekly goes");
        after_partition(&conn, &dir);
        still_there("the vector partition");
        // Settings and doctor mark them.
        let s = status(&dir, Some(&conn));
        let marked: Vec<&str> = s
            .snapshots
            .iter()
            .filter(|s| s.other_index)
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(marked.len(), 2, "{marked:?}");
        assert!(old.iter().all(|n| marked.contains(&n.as_str())));
        assert!(status(&dir, None).snapshots.iter().all(|s| !s.other_index));
    }

    /// A restored index has the identity of the snapshot it came from, so its own retention
    /// replaces that snapshot once a newer weekly one is verified.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_restored_index_keeps_the_identity_of_its_snapshot() {
        let dir = TestDir::new("backup_restored_lineage");
        drop(index_with(&dir, 3));
        let Weekly::Taken(first) = run_weekly_with(&dir, T0, &|_| None, &clone_probe) else {
            panic!("taken");
        };
        let id = stored_index_id(&dir).unwrap();
        damaged_index(&dir, 1);
        let restored = restore_with(&dir, &first, T0 + DAY, &|_| None, &nobody).unwrap();
        assert_eq!(restored.snapshot.lineage.as_deref(), Some(id.as_str()));
        assert_eq!(stored_index_id(&dir).as_deref(), Some(id.as_str()));
        let Weekly::Taken(next) = run_weekly_with(
            &dir,
            T0 + 2 * DAY + WEEKLY_EVERY_SECS,
            &|_| None,
            &clone_probe,
        ) else {
            panic!("taken after the restore");
        };
        assert_eq!(
            names(&dir),
            [next],
            "the restored-from snapshot was replaced"
        );
    }

    /// Stamps written while the clock ran seven months ahead (the last snapshot time, the
    /// check record, this process's try, the folder) do not stop the next weekly snapshot once
    /// the clock is back, and the snapshot taken then is the one kept.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_clock_that_ran_ahead_does_not_stop_the_weekly_snapshot() {
        let dir = TestDir::new("backup_clock");
        let conn = index_with(&dir, 2);
        let ahead = T0 + 210 * DAY;
        let Weekly::Taken(future) = run_weekly_with(&dir, ahead, &|_| None, &clone_probe) else {
            panic!("taken while ahead");
        };
        assert!(
            !weekly_due(&conn, ahead + DAY),
            "not due again at that clock"
        );
        assert!(weekly_due(&conn, T0), "due once the clock is back");
        let Weekly::Taken(now_one) = run_weekly_with(&dir, T0, &|_| None, &clone_probe) else {
            panic!("taken after the clock came back");
        };
        assert_eq!(names(&dir), [now_one], "the one ahead ({future}) went");
        assert!(!weekly_due(&conn, T0 + DAY));
    }

    /// A disk that cannot clone (the injected probe) is recorded, so Settings and `cal doctor`
    /// stop saying snapshots are automatic; a later weekly snapshot clears it.
    #[test]
    fn a_disk_that_cannot_clone_is_recorded_until_a_snapshot_is_taken() {
        let dir = TestDir::new("backup_noclone_record");
        let conn = index_with(&dir, 1);
        assert_eq!(status(&dir, Some(&conn)).automatic, cheap_clones());
        let no_clone = |_: &Path| -> std::io::Result<()> {
            Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
        };
        assert_eq!(
            run_weekly_with(&dir, T0, &|_| None, &no_clone),
            Weekly::Skipped
        );
        assert_eq!(
            app_config::get(&conn, NO_CLONE_KEY).unwrap().as_deref(),
            Some(T0.to_string().as_str())
        );
        assert!(!status(&dir, Some(&conn)).automatic);
        assert_eq!(
            status(&dir, None).automatic,
            cheap_clones(),
            "no index read"
        );
        // Another failure, a real error rather than a disk without clones, records nothing.
        app_config::delete(&conn, NO_CLONE_KEY).unwrap();
        let broken = |_: &Path| -> std::io::Result<()> {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        };
        run_weekly_with(&dir, T0 + DAY, &|_| None, &broken);
        assert_eq!(app_config::get(&conn, NO_CLONE_KEY).unwrap(), None);
        run_weekly_with(&dir, T0 + 2 * DAY, &|_| None, &no_clone);
        assert!(!status(&dir, Some(&conn)).automatic);
        if cheap_clones() {
            assert!(matches!(
                run_weekly_with(&dir, T0 + 3 * DAY, &|_| None, &clone_probe),
                Weekly::Taken(_)
            ));
            assert_eq!(app_config::get(&conn, NO_CLONE_KEY).unwrap(), None);
            assert!(status(&dir, Some(&conn)).automatic);
        }
    }

    /// The weekly run checks the file, snapshots it, and keeps one weekly snapshot.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_weekly_run_checks_snapshots_and_keeps_one() {
        let dir = TestDir::new("backup_weekly");
        let conn = index_with(&dir, 2);
        assert!(weekly_due(&conn, T0));
        assert!(matches!(
            run_weekly_with(&dir, T0, &|_| None, &clone_probe),
            Weekly::Taken(_)
        ));
        assert!(!weekly_due(&conn, T0 + DAY), "a week has not passed");
        let check = last_check(&conn).unwrap();
        assert!(check.ok && check.at == T0, "{check:?}");
        let later = T0 + WEEKLY_EVERY_SECS;
        assert!(weekly_due(&conn, later));
        let Weekly::Taken(name) = run_weekly_with(&dir, later, &|_| None, &clone_probe) else {
            panic!("taken");
        };
        assert_eq!(names(&dir), std::slice::from_ref(&name));
        assert_eq!(threads_in(&list(&dir)[0].dir.join("index.db")), 2);
        let entries: Vec<String> = std::fs::read_dir(snapshots_dir(&dir))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(entries, [name], "the clone probe left nothing behind");
    }

    /// The due rule: a week since the last snapshot and a day since the last check.
    #[test]
    fn the_weekly_snapshot_is_due_after_a_week_and_retries_daily() {
        let dir = TestDir::new("backup_due");
        let conn = index_with(&dir, 0);
        let due = |now| cheap_clones() && weekly_due(&conn, now);
        assert_eq!(weekly_due(&conn, T0), cheap_clones(), "never taken");
        app_config::put(&conn, LAST_AT_KEY, &T0.to_string()).unwrap();
        let rec = |at: i64, ok: bool| {
            let json = serde_json::to_string(&CheckRecord { at, ok, ms: 1 }).unwrap();
            app_config::put(&conn, LAST_CHECK_KEY, &json).unwrap();
        };
        rec(T0, true);
        assert!(!due(T0 + 6 * DAY));
        assert_eq!(weekly_due(&conn, T0 + 7 * DAY), cheap_clones());
        // A failed check (or a refused snapshot) a day ago, and no snapshot since: daily.
        rec(T0 + 7 * DAY, false);
        assert!(!due(T0 + 7 * DAY + 3_600));
        assert_eq!(weekly_due(&conn, T0 + 8 * DAY), cheap_clones());
    }

    /// A check that finds damage takes no snapshot and drops none, and says so in the
    /// record.
    #[test]
    fn a_failed_integrity_check_keeps_every_snapshot_and_takes_none() {
        let dir = TestDir::new("backup_badcheck");
        let root = {
            let conn = index_with(&dir, 40);
            conn.query_row(
                "SELECT rootpage FROM sqlite_master WHERE name = 'messages'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
        };
        // Fold the WAL in, then overwrite the messages table's root page.
        drop(db::open(&dir).unwrap());
        {
            use std::io::{Seek, SeekFrom, Write};
            let page: u64 = {
                let c = Connection::open(&*dir).unwrap();
                c.query_row("PRAGMA page_size", [], |r| r.get::<_, i64>(0))
                    .unwrap() as u64
            };
            let mut f = std::fs::OpenOptions::new().write(true).open(&*dir).unwrap();
            f.seek(SeekFrom::Start((root as u64 - 1) * page)).unwrap();
            f.write_all(&vec![0xa5_u8; page as usize]).unwrap();
        }
        let kept = format!("{}-weekly", stamp(T0));
        plant(&dir, &kept, &["index.db"]);
        assert_eq!(
            run_weekly_with(&dir, T0 + 8 * DAY, &|_| None, &|_| Ok(())),
            Weekly::CheckFailed
        );
        assert_eq!(names(&dir), [kept]);
        let conn = Connection::open(&*dir).unwrap();
        let check = last_check(&conn).expect("recorded");
        assert!(!check.ok);
        assert_eq!(app_config::get(&conn, LAST_AT_KEY).unwrap(), None);
    }

    /// A volume that cannot clone (exFAT, HFS+ on a Mac) is found out before the integrity
    /// check, which then never runs, and the next try waits a day.
    #[test]
    fn a_volume_without_clones_skips_the_check_and_waits_a_day() {
        let dir = TestDir::new("backup_noclone");
        let conn = index_with(&dir, 1);
        let no_clone = |_: &Path| -> std::io::Result<()> {
            Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
        };
        assert_eq!(
            run_weekly_with(&dir, T0, &|_| None, &no_clone),
            Weekly::Skipped
        );
        assert_eq!(last_check(&conn), None, "no integrity check ran");
        assert!(list(&dir).is_empty());
        if cheap_clones() {
            assert!(!weekly_due(&conn, T0 + 3_600), "not again within the day");
            assert!(weekly_due(&conn, T0 + CHECK_RETRY_SECS));
        }
    }

    /// While another process holds the write lock past the wait, the check's record cannot
    /// be written; the next run still waits a day instead of rereading the file a minute
    /// later.
    #[test]
    fn a_check_whose_record_cannot_be_written_still_waits_a_day() {
        let dir = TestDir::new("backup_busy");
        let conn = index_with(&dir, 1);
        conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
        assert_eq!(
            run_weekly_with(&dir, T0, &|_| None, &|_| Ok(())),
            Weekly::Skipped
        );
        assert_eq!(last_check(&conn), None, "the record could not be written");
        if cheap_clones() {
            assert!(!weekly_due(&conn, T0 + 60), "not on the next minute tick");
            assert!(weekly_due(&conn, T0 + CHECK_RETRY_SECS));
        }
        conn.execute_batch("ROLLBACK;").unwrap();
        assert!(list(&dir).is_empty());
    }

    /// `cal backup` makes a full copy where it cannot clone, owner-only and complete, and
    /// refuses (leaving nothing behind) when the target's disk is short.
    #[test]
    fn cal_backup_copies_in_full_where_it_cannot_clone() {
        let dir = TestDir::new("backup_full");
        let target = TestDir::new("backup_full_target");
        let target = target.parent().unwrap().join("backups");
        let conn = index_with(&dir, 6);
        let no_clone: CloneFn = |_, _| Err(std::io::Error::from(std::io::ErrorKind::Unsupported));
        let mut last = (0, 0);
        let made = backup_to(&dir, Some(&target), no_clone, &|_| None, &mut |d, t| {
            last = (d, t)
        })
        .unwrap();
        assert_eq!(made.method, Method::FullCopy);
        assert!(
            last.1 > 0 && last.0 == last.1,
            "progress reached the end: {last:?}"
        );
        assert!(made.dir.starts_with(&target));
        assert_eq!(threads_in(&made.dir.join("index.db")), 6);
        #[cfg(unix)]
        {
            assert_eq!(mode(&made.dir), 0o700);
            assert_eq!(mode(&made.dir.join("index.db")), 0o600);
        }
        drop(conn);
        let before: Vec<_> = std::fs::read_dir(&target).unwrap().collect();
        let err =
            backup_to(&dir, Some(&target), no_clone, &|_| Some(10), &mut |_, _| {}).unwrap_err();
        assert!(
            format!("{err:#}").contains("not enough free disk space"),
            "{err:#}"
        );
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), before.len());
        assert!(backup_to(
            &dir.with_file_name("missing.db"),
            None,
            no_clone,
            &|_| None,
            &mut |_, _| {}
        )
        .is_err());
    }

    /// Timings on a copy of a real index (`CALLIMACHUS_DB`, refused in the app's data folder,
    /// as `bench::real_index_bench` does): a snapshot with its verification, then a restore
    /// of it, which moves the copy's own files aside. A debug build compiles SQLite without
    /// optimizations, so these are upper bounds; `cal backup` (release) times the snapshot.
    #[test]
    #[ignore = "snapshots and restores a copy of a real index from CALLIMACHUS_DB"]
    fn real_index_snapshot_and_restore() {
        let Some(path) = std::env::var_os("CALLIMACHUS_DB").map(PathBuf::from) else {
            println!("skipped: set CALLIMACHUS_DB to a copy of an index");
            return;
        };
        let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        assert!(
            canonical(&path).parent() != Some(canonical(&db::app_data_dir()).as_path()),
            "{} is in the app's data folder; copy it elsewhere first (cp -c on APFS)",
            path.display()
        );
        let t = Instant::now();
        let s = snapshot(&path, Reason::Manual, Duration::from_secs(60)).unwrap();
        println!(
            "snapshot {} in {:?} ({} bytes)",
            s.name,
            t.elapsed(),
            s.bytes
        );
        let t = Instant::now();
        let r = restore(&path, &s.name).unwrap();
        println!(
            "restore in {:?}; damaged copy kept at {:?}",
            t.elapsed(),
            r.damaged
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn cal_backup_clones_on_apfs() {
        let dir = TestDir::new("backup_clone");
        let _conn = index_with(&dir, 3);
        let made = backup_to(&dir, None, clone::clone_file, &|_| None, &mut |_, _| {}).unwrap();
        assert_eq!(made.method, Method::Clone);
        let snaps = list(&dir);
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].reason, Reason::Manual);
        assert_eq!(threads_in(&snaps[0].dir.join("index.db")), 3);
    }
}
