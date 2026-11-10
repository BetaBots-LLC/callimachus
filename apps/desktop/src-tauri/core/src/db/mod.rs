//! SQLite connection + schema bootstrap. A single connection guarded by a Mutex
//! lives in Tauri managed state (rusqlite::Connection is Send but not Sync).

pub mod migrations;

use anyhow::{Context, Result};
use rusqlite::{Connection, ErrorCode, OpenFlags, TransactionBehavior};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, Once, TryLockError};
use std::time::Duration;

/// How long a write on an app connection waits for another connection's write lock before
/// failing with "database is locked". The app, `cal`, the MCP server and hooks share the
/// file, and in-process the indexer and file watcher each hold their own write connection.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(15);

/// The wait for sidecar processes (the MCP server, hooks, `cal` write commands). Their
/// writes are tiny and they keep no UI responsive, so outlasting a long app write (a big
/// cleanup, a one-time index upgrade) beats failing inside the user's agent session.
pub const SIDECAR_BUSY_TIMEOUT: Duration = Duration::from_secs(60);

/// How long the app waits, when its schema is out of date, for another process that is
/// already upgrading the same index. One release's migrations (0025-0031: four index
/// builds over the whole messages table) take ~17 s on a 4.7 GB index from a cold cache
/// with the pre-read (see `preread_for_migration`), ~48 s where it is skipped.
const UPGRADE_WAIT: Duration = Duration::from_secs(120);

/// mmap window for every connection: the bundled SQLite's cap (asking for more returns
/// this). KNN reads every vec0 chunk blob, and on a large index those pages sit up to ~1 GB
/// into the file; with the old 256 MiB window three quarters of them went through pread
/// into a small page cache on every semantic query (20-25% slower). The mapping is clean,
/// file-backed pages, so it costs address space, not memory footprint.
#[cfg(not(windows))]
const MMAP_SIZE: i64 = 2_147_418_112;

/// Windows keeps the old 256 MiB window. NTFS refuses to truncate a file below the size of
/// a section another handle still has mapped, and SQLite's winTruncate ignores that error
/// and reports success. Every connection (the writer, the read pool, the watcher, MCP and
/// `cal` sidecars) holds its own view, so with a 2 GiB window a VACUUM / Shrink index on any
/// index under 2 GiB would quietly leave the file at its old size.
#[cfg(windows)]
const MMAP_SIZE: i64 = 256 << 20;

/// Managed Tauri state wrapper around the app's SQLite connection.
pub struct Db(pub Mutex<Connection>);

/// Lock a shared connection (the app's writer, the MCP server's), surviving a panic under an
/// earlier guard. A panic while the guard is held poisons the mutex, and a plain `lock()`
/// then fails forever: one panicking MCP handler, Tauri command or job broke every later
/// call until a restart. The connection itself outlives the unwind (rusqlite's `Transaction`
/// rolls back on drop), but a raw `BEGIN` has no such guard and would keep the write lock,
/// so an open transaction is rolled back here. Nothing keeps a transaction open across
/// guards (a `Transaction` borrows the guard), so one found open at acquisition is abandoned.
pub fn lock_conn(m: &Mutex<Connection>) -> MutexGuard<'_, Connection> {
    let guard = m.lock().unwrap_or_else(|poisoned| {
        // Cleared so later callers take the plain path; the rollback below runs either way.
        m.clear_poison();
        crate::app_log!("[db] recovered the connection lock after a panic under it");
        poisoned.into_inner()
    });
    roll_back_abandoned(&guard);
    guard
}

/// [`lock_conn`] without waiting: None while another thread holds the lock.
pub fn try_lock_conn(m: &Mutex<Connection>) -> Option<MutexGuard<'_, Connection>> {
    let guard = match m.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::WouldBlock) => return None,
        Err(TryLockError::Poisoned(poisoned)) => {
            m.clear_poison();
            crate::app_log!("[db] recovered the connection lock after a panic under it");
            poisoned.into_inner()
        }
    };
    roll_back_abandoned(&guard);
    Some(guard)
}

fn roll_back_abandoned(conn: &Connection) {
    if !conn.is_autocommit() {
        if let Err(e) = conn.execute_batch("ROLLBACK") {
            crate::app_log!("[db] rolling back an abandoned transaction: {e}");
        }
    }
}

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
/// otherwise a per-user, writable app-data dir for bundle id
/// `dev.shaller.callimachus`.
///
/// Resolved with `dirs::data_local_dir()` so it is correct on every platform:
/// - macOS:   `~/Library/Application Support/dev.shaller.callimachus/index.db`
///   (byte-identical to the previous hardcoded path, so existing indexes move nowhere)
/// - Windows: `%LOCALAPPDATA%\dev.shaller.callimachus\index.db`
/// - Linux:   `$XDG_DATA_HOME` (or `~/.local/share`)`/dev.shaller.callimachus/index.db`
///
/// The old implementation hardcoded the macOS `~/Library/...` layout off `$HOME`.
/// On Windows `$HOME` is normally unset, so the path collapsed to a *relative*
/// `Library/Application Support/...` resolved against the process CWD (the install
/// dir, e.g. `C:\Program Files\Callimachus`), which a standard user cannot write —
/// the app crashed unless launched as administrator. `data_local_dir` (not the
/// roaming `data_dir`) keeps the index out of the Windows roaming profile.
pub fn default_index_path() -> PathBuf {
    // Tests run `cal` commands in process against a temp index; the environment is shared by
    // every test thread, so they point this thread at it instead.
    #[cfg(test)]
    if let Some(p) = test_support::default_index_override() {
        return p;
    }
    if let Ok(p) = std::env::var("CALLIMACHUS_DB") {
        return PathBuf::from(p);
    }
    implicit_index_path()
}

/// The index nobody named: the app's own.
#[cfg(not(test))]
fn implicit_index_path() -> PathBuf {
    app_data_dir().join("index.db")
}

/// In this crate's unit tests the index nobody named is one that does not exist, never the
/// user's real one. A test that runs a command in process without `with_default_index` must
/// not reach the machine's own data folder: the tests that feed `run_and_exit` a bad hook
/// payload once recorded their "hook gave up" lines in the developer's real ledger, four per
/// test run, because the real index existed. (`CALLIMACHUS_DB` set by whoever runs the tests
/// is still honored: that is a choice, this was an accident.)
#[cfg(test)]
fn implicit_index_path() -> PathBuf {
    test_support::no_default_index()
}

/// The folder of the app's own index (and the dev build's `index.dev.db`), whatever
/// `CALLIMACHUS_DB` says.
pub(crate) fn app_data_dir() -> PathBuf {
    dirs::data_local_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("dev.shaller.callimachus")
}

/// Open (or create) the database at `path`, apply pragmas, and migrate to latest. For the
/// desktop app's own connections; sidecar processes use [`open_sidecar`].
pub fn open(path: &Path) -> Result<Connection> {
    open_with(path, BUSY_TIMEOUT, UPGRADE_WAIT)
}

/// [`open`] for the sidecar processes (MCP server, hooks, `cal` write commands): writes wait
/// up to [`SIDECAR_BUSY_TIMEOUT`] for the lock, and so does waiting out an index upgrade
/// another process is running. Callers should treat an error that [`is_busy`] recognizes
/// as "try again later", not as a failure to report inside an agent session.
pub fn open_sidecar(path: &Path) -> Result<Connection> {
    open_with_busy_timeout(path, SIDECAR_BUSY_TIMEOUT)
}

/// [`open`] with one custom wait for both writes and a concurrent index upgrade, for a
/// caller with its own deadline (a hook that the agent kills after a few seconds should
/// give up, and exit quietly on [`is_busy`], before that).
pub fn open_with_busy_timeout(path: &Path, busy: Duration) -> Result<Connection> {
    open_with(path, busy, busy)
}

/// The index's schema is not the one this binary was built for: older (an update has not
/// run its migrations yet) or newer (an older `cal` or hook binary is still installed).
/// [`open_sidecar_current`] returns it instead of migrating; hooks treat it as "stay quiet".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchemaMismatch {
    /// Migrations this binary would apply: positive when the index is behind, negative
    /// when it is ahead.
    pub pending: i32,
}

impl std::fmt::Display for SchemaMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.pending > 0 {
            write!(
                f,
                // Not a bare "run `cal index`": a `cal` linked to an older app cannot apply
                // them, and one the npm launcher downloaded would upgrade the index past the
                // installed app, which then cannot open it.
                "the index needs {} schema upgrade(s); open Callimachus, or on a machine without the app run `cal index`",
                self.pending
            )
        } else {
            write!(
                f,
                "the index was upgraded by a newer Callimachus; update this binary"
            )
        }
    }
}

impl std::error::Error for SchemaMismatch {}

/// True when `e` is (or wraps) a [`SchemaMismatch`].
pub fn is_schema_mismatch(e: &anyhow::Error) -> bool {
    e.chain()
        .any(|c| c.downcast_ref::<SchemaMismatch>().is_some())
}

/// A writable sidecar connection that never migrates and never creates the index, for the
/// hooks Claude Code runs inside the agent loop (PreCompact, SessionEnd). An upgrade can
/// hold the write lock for tens of seconds on a large index (see [`UPGRADE_WAIT`]) and a
/// SessionEnd hook has 1.5 s, so the first process to see a new schema must be the app or
/// a `cal` command the user ran, never a hook the agent kills mid-migration. Errors with
/// [`SchemaMismatch`] when the schema is behind or ahead of this binary, and with a plain
/// error when the file does not exist. Waits [`SIDECAR_BUSY_TIMEOUT`] on locks like
/// [`open_sidecar`]; a hook with its own deadline uses [`open_sidecar_current_with`].
pub fn open_sidecar_current(path: &Path) -> Result<Connection> {
    open_sidecar_current_with(path, SIDECAR_BUSY_TIMEOUT)
}

/// [`open_sidecar_current`] waiting at most `busy` on locks, from the first statement on: the
/// `user_version` read already waits on a writer that holds the lock, so a short-lived hook
/// must not inherit the sidecar's 60 s there.
pub fn open_sidecar_current_with(path: &Path, busy: Duration) -> Result<Connection> {
    // Fact deletes fire 0012's trigger into vec_facts, which needs vec0 loaded.
    register_vec();
    if !path.exists() {
        anyhow::bail!("no index at {}", path.display());
    }
    crate::backup::refuse_interrupted_restore(path)?;
    // READ_WRITE without CREATE: a vanished file is an error, not a new empty index.
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("opening database at {}", path.display()))?;
    conn.busy_timeout(busy)?;
    let pending = migrations::MIGRATIONS
        .pending_migrations(&conn)
        .context("reading the schema version")?;
    if pending != 0 {
        return Err(SchemaMismatch { pending }.into());
    }
    // The app set WAL when it built the index (it is persistent in the file); these are the
    // per-connection settings a short-lived writer needs. A small cache: a hook writes a
    // row or two and exits.
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "cache_size", -4096_i64)?; // 4 MiB
                                                        // No checkpoint from a hook's commit. SQLite's default autocheckpoint is 1000 pages and
                                                        // the app runs at 2000, so during indexing the WAL often sits between the two, and a hook's
                                                        // one-row commit would run a 4 to 8 MB PASSIVE checkpoint plus an fsync inside SessionEnd's
                                                        // 1.5 s budget. The app's own connections keep checkpointing.
    conn.pragma_update(None, "wal_autocheckpoint", 0_i64)?;
    Ok(conn)
}

/// [`open_sidecar_current`] for a long-lived sidecar, the MCP server: the same no-migrate
/// check, then the app's reader-friendly cache, mmap and checkpoint settings in place of a
/// hook's minimal ones, since it serves searches for a whole agent session and its writes
/// (record_*, complete_todo) are rare enough that checkpointing them costs nothing.
pub fn open_sidecar_current_long_lived(path: &Path) -> Result<Connection> {
    let conn = open_sidecar_current(path)?;
    conn.pragma_update(None, "cache_size", -65536_i64)?; // 64 MiB, as `open`
    conn.pragma_update(None, "mmap_size", MMAP_SIZE)?;
    conn.pragma_update(None, "wal_autocheckpoint", 2000_i64)?;
    conn.pragma_update(None, "journal_size_limit", 67_108_864_i64)?;
    Ok(conn)
}

/// Migrations this binary would apply to `conn`'s index: 0 when current, positive when the
/// index is behind, negative when a newer binary upgraded it. One `user_version` read, cheap
/// enough to run before every MCP tool call.
pub fn schema_pending(conn: &Connection) -> Result<i32> {
    migrations::MIGRATIONS
        .pending_migrations(conn)
        .context("reading the schema version")
}

/// True when `e` is SQLite giving up on a lock, SQLITE_BUSY or SQLITE_LOCKED (any extended
/// code), anywhere in its chain: under `.context()`, inside a migration error, behind an
/// `AppError`, or flattened to text by `anyhow!(e.to_string())`. The one lock check for
/// every retry loop and "try again later" message, so adding context to a write path can
/// never turn a retryable lock into a hard failure.
pub fn is_busy(e: &anyhow::Error) -> bool {
    e.chain().any(is_busy_cause)
}

/// [`is_busy`] for a bare rusqlite error.
pub fn is_busy_sqlite(e: &rusqlite::Error) -> bool {
    matches!(
        e.sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

fn is_busy_cause(cause: &(dyn std::error::Error + 'static)) -> bool {
    if let Some(e) = cause.downcast_ref::<rusqlite::Error>() {
        return is_busy_sqlite(e);
    }
    if let Some(rusqlite_migration::Error::RusqliteError { err, .. }) = cause.downcast_ref() {
        return is_busy_sqlite(err);
    }
    // A transparent wrapper (AppError) forwards source() to the inner error's source, so
    // the rusqlite::Error itself is skipped but its ffi::Error cause still shows up.
    if let Some(e) = cause.downcast_ref::<rusqlite::ffi::Error>() {
        return matches!(e.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked);
    }
    if let Some(crate::error::AppError::Other(inner)) = cause.downcast_ref() {
        return is_busy(inner);
    }
    // Only the text survived (a chain flattened with to_string): SQLite's own messages for
    // SQLITE_BUSY and the SQLITE_LOCKED family.
    let msg = cause.to_string();
    [
        "database is locked",
        "database table is locked",
        "database schema is locked",
    ]
    .iter()
    .any(|m| msg.contains(m))
}

/// Whether this connection's schema has the index `name`. For a query that names an index
/// with INDEXED BY, which is an error (not just a slower plan) when the index is missing:
/// read-only sidecars and pooled readers never migrate, so a newer binary can be reading an
/// index an older app built. Asked per query on a cached statement (the schema table is
/// tiny and always cached), so it also stays right when an upgrade lands under a
/// long-lived connection.
pub fn has_index(conn: &Connection, name: &str) -> bool {
    conn.prepare_cached("SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1")
        .and_then(|mut stmt| stmt.exists([name]))
        .unwrap_or(false)
}

/// Whether this connection's schema has the table `name`, for a query that reads a table a
/// later migration added. Same reason and cost as [`has_index`]: a read-only connection
/// can be reading an index an older app built, and an upgrade can land under it.
pub fn has_table(conn: &Connection, name: &str) -> bool {
    conn.prepare_cached("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1")
        .and_then(|mut stmt| stmt.exists([name]))
        .unwrap_or(false)
}

/// Create `dir` if it is missing, owner-only (0700) on Unix: it is about to hold the index,
/// which keeps transcripts unredacted, secrets included. Only the last component gets the
/// mode; missing parents above it are created as usual (a `CALLIMACHUS_DB` under a new
/// `/srv/shared/x/` must not make `/srv/shared` private). An existing folder is left as it is.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    if let Some(up) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(up)?;
    }
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && dir.is_dir() => Ok(()),
        other => other,
    }
}

/// Drop the group and other bits of `path`'s mode when it has any (0644 becomes 0600, 0755
/// becomes 0700), keeping the owner's. One `lstat` when nothing needs changing, so a writable
/// open costs three of them. Best effort: a file another user owns (a shared
/// `CALLIMACHUS_DB`) refuses the chmod, and that is not this open's problem to report. Never
/// through a symbolic link: the index folder's own entries only. A no-op off Unix (Windows
/// gives `%LOCALAPPDATA%` per-user ACLs).
pub(crate) fn tighten_mode(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            return;
        };
        if meta.file_type().is_symlink() {
            return;
        }
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o700));
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// The index's folder before a writable open: a missing one is created owner-only; an
/// existing one is tightened only when it is `default_dir`, the app's own data folder
/// ([`app_data_dir`]; injected so tests never touch the real one). A folder the user chose
/// for `CALLIMACHUS_DB` keeps whatever mode they gave it.
fn prepare_index_dir(parent: &Path, default_dir: &Path) -> Result<()> {
    if !parent.exists() {
        create_private_dir(parent)
            .with_context(|| format!("creating db directory {}", parent.display()))?;
    } else if parent == default_dir {
        tighten_mode(parent);
    }
    Ok(())
}

/// The index file and its WAL and shared-memory files to 0600, for an index created before
/// files were owner-only (they were 0644). SQLite gives a new `-wal` / `-shm` the database
/// file's mode, so tightening `index.db` first covers the sidecars it creates from then on.
fn restrict_index_files(path: &Path) {
    tighten_mode(path);
    for suffix in ["-wal", "-shm"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        tighten_mode(Path::new(&name));
    }
}

fn open_with(path: &Path, busy: Duration, upgrade_wait: Duration) -> Result<Connection> {
    register_vec();
    // Create the parent dir so a first run (e.g. the MCP server in a fresh
    // container, or CALLIMACHUS_DB pointing somewhere new) can create the file —
    // rusqlite makes the file but not the directory.
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        prepare_index_dir(parent, &app_data_dir())?;
    }
    // A restore halfway through its swap: this open would hide that state for good.
    crate::backup::refuse_interrupted_restore(path)?;
    let mut conn = Connection::open(path)
        .with_context(|| format!("opening database at {}", path.display()))?;
    // Writable opens only: read-only sidecars, pooled readers and hooks never change modes.
    restrict_index_files(path);

    // WAL = concurrent readers while we write; required so indexing doesn't block reads.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // Wait (don't fail) for a write lock. Combined with BEGIN IMMEDIATE on every write txn
    // (so the busy handler actually engages instead of an instant upgrade-conflict), a
    // generous timeout lets concurrent writers queue rather than error "database locked".
    conn.busy_timeout(busy)?;
    // Performance pragmas (defaults are tiny). cache_size is the biggest single win:
    // -65536 = 64 MiB page cache. mmap maps the file for read; temp_store keeps
    // sorts/CTEs in RAM; the WAL caps keep the -wal file from growing unbounded and
    // checkpoints small. See db-perf audit.
    conn.pragma_update(None, "cache_size", -65536_i64)?; // 64 MiB
    conn.pragma_update(None, "mmap_size", MMAP_SIZE)?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "wal_autocheckpoint", 2000_i64)?; // ~8 MiB
    conn.pragma_update(None, "journal_size_limit", 67_108_864_i64)?; // cap -wal at 64 MiB

    migrate(&mut conn, busy, upgrade_wait)?;
    Ok(conn)
}

/// Bring the schema up to date, safely against other processes doing the same.
///
/// After an update, whichever process opens the index first (the app, the MCP server, a
/// hook) runs the new migrations, which can hold the write lock for tens of seconds.
/// rusqlite_migration reads `user_version` before it takes that lock, so a second process
/// would fail after its normal busy timeout, or get the lock afterwards and replay
/// migrations that were just applied. So when anything is pending: wait for the write lock
/// first (an upgrade in flight finishes, and to_latest then re-reads the version and
/// finds nothing to do), and migrate in IMMEDIATE transactions with the same long wait.
/// Holding that first lock, the index is cloned into `snapshots/<UTC>-pre-migration-<id>/`
/// next to it ([`crate::backup`]), so an upgrade that goes wrong has a copy to go back to.
/// Hooks and the MCP server never get here (they never migrate).
fn migrate(conn: &mut Connection, busy: Duration, upgrade_wait: Duration) -> Result<()> {
    let m = &*migrations::MIGRATIONS;
    let pending = m
        .pending_migrations(conn)
        .context("reading the schema version")?;
    if pending == 0 {
        return Ok(());
    }
    if pending < 0 {
        // A newer build's schema: to_latest reports DatabaseTooFarAhead without locking.
        return m.to_latest(conn).context("running migrations");
    }
    // Skip the pre-read when only scan-free migrations are pending (see
    // `migrations::SCAN_FREE_MIGRATIONS`): reading 5 GB to add one column costs more than the
    // migration.
    let applied: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap_or(0);
    let applied = usize::try_from(applied).unwrap_or(0);
    let latest = applied + usize::try_from(pending).unwrap_or(0);
    if !migrations::pending_are_scan_free(applied, latest) {
        if let Some(path) = conn.path().filter(|p| !p.is_empty()) {
            preread_for_migration(Path::new(path));
        }
    }
    conn.busy_timeout(upgrade_wait)?;
    // Before the write lock, in autocommit: the index's identity, which the snapshot below must
    // carry (committed) and which retention compares; None skips the snapshot.
    let snapshot_id = crate::backup::pre_migration_prepare(conn);
    conn.set_transaction_behavior(TransactionBehavior::Immediate);
    let mut snapshot = None;
    let result = conn
        .execute_batch("BEGIN IMMEDIATE;")
        .map_err(anyhow::Error::from)
        .and_then(|()| {
            // Holding the write lock: clone the index as it is before the upgrade (instant
            // on APFS; skipped, with one log line, where it cannot be cloned or the disk is
            // short). Never fails the upgrade.
            let taken = snapshot_id
                .as_deref()
                .and_then(|id| crate::backup::pre_migration_locked(conn, id));
            conn.execute_batch("ROLLBACK;")?;
            snapshot = crate::backup::pre_migration_verify(taken);
            m.to_latest(conn).map_err(anyhow::Error::from)
        });
    conn.set_transaction_behavior(TransactionBehavior::Deferred);
    // The new snapshot replaces the older pre-migration one only when the upgrade went through
    // (here or, in the race below, in another process); a failed upgrade's copy is removed.
    let upgraded = result.is_ok() || m.pending_migrations(conn).ok() == Some(0);
    crate::backup::pre_migration_done(snapshot, upgraded);
    conn.busy_timeout(busy)?;
    match result {
        Ok(()) => Ok(()),
        // Another process took the lock in the instant between our wait and our migration
        // and finished the upgrade first; our replay failed and rolled back. Nothing to do.
        Err(_) if m.pending_migrations(conn).ok() == Some(0) => Ok(()),
        Err(e) => Err(e.context("running migrations")),
    }
}

/// Below this size a cold upgrade is quick anyway; a pre-read would only add I/O.
const PREREAD_MIN_BYTES: u64 = 256 << 20;
/// Never pre-read more than this, whatever the RAM.
const PREREAD_MAX_BYTES: u64 = 8 << 30;

/// Read the database file once, front to back, before running pending migrations. Their
/// index builds scan `messages` in b-tree order, and reaching a column stored after `text`
/// walks each long row's overflow chain: about 3 GB of random 4 KB reads on a 4.7 GB index.
/// Measured there from a cold cache: 0031's two index builds took 20.6 s, and 5.1 s after
/// the sequential read (itself ~1.2 s); the whole 24 -> 31 upgrade took 17 s, where 24 -> 30
/// alone took 27.7 s without it. Runs once per release, in whichever process (app, MCP
/// server, hook) opens the index first. Best effort: any error skips it. The file lands in
/// reclaimable page cache, not RSS, and it is skipped when the file is small or would crowd
/// RAM.
fn preread_for_migration(path: &Path) {
    let Ok(len) = std::fs::metadata(path).map(|m| m.len()) else {
        return;
    };
    if !should_preread(len, physical_memory()) {
        return;
    }
    let Ok(mut file) = std::fs::File::open(path) else {
        return;
    };
    let mut buf = vec![0u8; 8 << 20];
    while matches!(file.read(&mut buf), Ok(n) if n > 0) {}
}

/// Pre-read only a file big enough to matter and small enough to stay cached until the
/// migration's scans reach it: at most half of RAM (when known) and [`PREREAD_MAX_BYTES`].
fn should_preread(len: u64, ram: Option<u64>) -> bool {
    let cap = ram.map_or(PREREAD_MAX_BYTES, |r| (r / 2).min(PREREAD_MAX_BYTES));
    len > PREREAD_MIN_BYTES && len <= cap
}

/// RAM this process can use, in bytes, where it is cheap to ask.
#[cfg(target_os = "macos")]
fn physical_memory() -> Option<u64> {
    let out = std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    String::from_utf8(out.stdout).ok()?.trim().parse().ok()
}

/// The smaller of installed RAM and the cgroup memory limit. Inside a container (the MCP
/// server image) /proc/meminfo reports the host's RAM, while the page cache that would
/// hold the pre-read is charged to, and evicted within, the container's limit. The
/// container's own cgroup is the root of its /sys/fs/cgroup.
#[cfg(target_os = "linux")]
fn physical_memory() -> Option<u64> {
    let read = |path: &str| std::fs::read_to_string(path).ok();
    let total = read("/proc/meminfo")
        .as_deref()
        .and_then(parse_meminfo_total);
    let limit = read("/sys/fs/cgroup/memory.max") // cgroup v2
        .or_else(|| read("/sys/fs/cgroup/memory/memory.limit_in_bytes")) // cgroup v1
        .as_deref()
        .and_then(parse_cgroup_limit);
    match (total, limit) {
        (Some(t), Some(l)) => Some(t.min(l)),
        (t, l) => t.or(l),
    }
}

/// `MemTotal` from /proc/meminfo, in bytes.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_meminfo_total(info: &str) -> Option<u64> {
    let kb = info.lines().find_map(|l| l.strip_prefix("MemTotal:"))?;
    let kb: u64 = kb.trim().trim_end_matches("kB").trim().parse().ok()?;
    Some(kb * 1024)
}

/// A cgroup memory limit file: bytes, or v2's `max` for none. (v1's "no limit" is a huge
/// number, which the `min` with MemTotal discards.)
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_cgroup_limit(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

/// Installed physical RAM from GlobalMemoryStatusEx (kernel32, always linked).
#[cfg(windows)]
fn physical_memory() -> Option<u64> {
    /// MEMORYSTATUSEX from <sysinfoapi.h>.
    #[repr(C)]
    struct MemoryStatusEx {
        length: u32,
        memory_load: u32,
        total_phys: u64,
        avail_phys: u64,
        total_page_file: u64,
        avail_page_file: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_extended_virtual: u64,
    }
    const _: () = assert!(std::mem::size_of::<MemoryStatusEx>() == 64);
    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalMemoryStatusEx(buffer: *mut MemoryStatusEx) -> i32;
    }
    let mut status = MemoryStatusEx {
        length: std::mem::size_of::<MemoryStatusEx>() as u32,
        memory_load: 0,
        total_phys: 0,
        avail_phys: 0,
        total_page_file: 0,
        avail_page_file: 0,
        total_virtual: 0,
        avail_virtual: 0,
        avail_extended_virtual: 0,
    };
    // SAFETY: `status` is a live, correctly sized MEMORYSTATUSEX with `length` set, which is
    // all the call requires; it only writes into that struct.
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) } != 0;
    (ok && status.total_phys > 0).then_some(status.total_phys)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn physical_memory() -> Option<u64> {
    None
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
    // Even read-only: with the `-shm` still beside it, SQLite gives the index a new `-wal`.
    crate::backup::refuse_interrupted_restore(path)?;
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
    conn.pragma_update(None, "mmap_size", MMAP_SIZE)?;
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
        conn.pragma_update(None, "mmap_size", MMAP_SIZE)?;
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
    // One reader stays open; the rest close after two idle minutes. r2d2's default keeps
    // max_size readers forever (min_idle = max_size, so idle_timeout never fires), and each
    // holds up to a 16 MiB page cache: 6 x 16 MiB for the app's lifetime, mostly in the
    // tray. Bursty UI reads still fan out to max_size and pay ~1 ms per reopened reader.
    let pool = r2d2::Pool::builder()
        .max_size(size.max(2))
        .min_idle(Some(1))
        .idle_timeout(Some(READER_IDLE_TIMEOUT))
        .build(ReadManager {
            path: path.to_path_buf(),
        })
        .context("building read pool")?;
    Ok(ReadPool(pool))
}

/// How long a pooled reader beyond the first may sit unused before the pool closes it.
pub const READER_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Hand a connection's page cache back to the allocator (`PRAGMA shrink_memory`) after a
/// burst of work: the writers' caches are 64 MiB each and otherwise stay full for the
/// life of the process. They refill on demand. Best effort; a failure changes nothing.
pub fn shrink_memory(conn: &Connection) {
    let _ = conn.execute_batch("PRAGMA shrink_memory;");
    crate::memtrim::release_freed_memory();
}

/// Test indexes that clean up after themselves. Test helpers used to name a fresh index
/// `calli_<tag>_<pid>_<n>.db` in the temp dir and never delete it: one machine's temp dir held
/// 42,883 of them (about 19 GiB). A [`TestIndex`](test_support::TestIndex) or
/// [`TestPath`](test_support::TestPath) removes its files on drop, and the first one a test
/// process makes sweeps what earlier test processes left behind.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;

    /// The extensions of `calli_<tag>_<pid>_<n>.db` and the files made next to it: SQLite's
    /// WAL, shared memory and rollback journal, and the MCP server's ledger
    /// (`ledger::path_for`).
    const EXTENSIONS: [&str; 5] = ["db", "db-wal", "db-shm", "db-journal", "ledger.jsonl"];

    /// A fresh temp index path, `calli_<tag>_<pid>_<n>.db`, whose files (WAL and shared
    /// memory included) are removed on drop. For tests that open their own connections.
    pub struct TestPath(PathBuf);

    impl TestPath {
        pub fn new(tag: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            sweep_once();
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("calli_{tag}_{}_{n}.db", std::process::id()));
            remove_index_files(&path);
            Self(path)
        }
    }

    impl std::ops::Deref for TestPath {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl AsRef<Path> for TestPath {
        fn as_ref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestPath {
        fn drop(&mut self) {
            remove_index_files(&self.0);
        }
    }

    /// A migrated index ([`open`]) at a [`TestPath`]. Derefs to its connection; on drop the
    /// connection closes, then the files go.
    pub struct TestIndex {
        // Before `path`: fields drop in order, and Windows cannot remove an open file.
        conn: Connection,
        path: TestPath,
    }

    thread_local! {
        /// The files of test indexes whose connection was handed out
        /// ([`TestIndex::into_connection`]), removed when this thread ends.
        static HANDED_OUT: std::cell::RefCell<Vec<TestPath>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    impl TestIndex {
        pub fn new(tag: &str) -> Self {
            let path = TestPath::new(tag);
            let conn = open(&path).expect("open + migrate");
            Self { conn, path }
        }

        /// `conn`, which the caller opened at `path` (a raw open at an old schema version,
        /// say), removed with it.
        pub fn adopt(conn: Connection, path: TestPath) -> Self {
            Self { conn, path }
        }

        /// The bare connection, for a helper whose callers move it (into a `Db`, the MCP
        /// server). Its files go when the calling thread ends: libtest runs each test on a
        /// thread of its own, so that is when the test is done and its locals are dropped.
        pub fn into_connection(self) -> Connection {
            let Self { conn, path } = self;
            HANDED_OUT.with(|files| files.borrow_mut().push(path));
            conn
        }
    }

    impl std::ops::Deref for TestIndex {
        type Target = Connection;
        fn deref(&self) -> &Connection {
            &self.conn
        }
    }

    impl std::ops::DerefMut for TestIndex {
        fn deref_mut(&mut self) -> &mut Connection {
            &mut self.conn
        }
    }

    thread_local! {
        /// What [`default_index_path`] returns on this thread inside [`with_default_index`].
        static DEFAULT_INDEX: std::cell::RefCell<Option<PathBuf>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Run `f` with [`default_index_path`] naming `path` on this thread, so a test can run a
    /// whole `cal` command in process against a temp index (`CALLIMACHUS_DB` would leak into
    /// every test running beside it).
    pub fn with_default_index<R>(path: &Path, f: impl FnOnce() -> R) -> R {
        let previous = DEFAULT_INDEX.with(|p| p.replace(Some(path.to_path_buf())));
        let out = f();
        DEFAULT_INDEX.with(|p| *p.borrow_mut() = previous);
        out
    }

    pub(super) fn default_index_override() -> Option<PathBuf> {
        DEFAULT_INDEX.with(|p| p.borrow().clone())
    }

    /// What [`default_index_path`] names in a unit test that set no override: a path in a
    /// temp folder that is never created (hooks create nothing next to a missing index), so
    /// "no index" is what such a test meets instead of the developer's real one.
    pub(super) fn no_default_index() -> PathBuf {
        std::env::temp_dir()
            .join("calli_no_default_index")
            .join("index.db")
    }

    /// An index file at `path` built to schema `version` (an older app's), `seed`ed on a raw
    /// connection at that version, checkpointed so the main file holds every row, and closed:
    /// what a reader that never migrates meets after an update.
    pub fn index_at_version(path: &Path, version: usize, seed: impl FnOnce(&mut Connection)) {
        register_vec();
        let mut conn = Connection::open(path).expect("create the index");
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        migrations::MIGRATIONS
            .to_version(&mut conn, version)
            .expect("migrate to the old version");
        seed(&mut conn);
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
    }

    /// A fresh temp folder, `calli_<tag>_<pid>_<n>`, removed with all it holds on drop. Derefs
    /// to the (not yet created) `index.db` path inside it, for tests whose index needs a
    /// folder of its own: lock files and logs land next to the index. The sweep leaves
    /// folders alone, so without this each run of such a test left one behind.
    pub struct TestDir {
        dir: PathBuf,
        index: PathBuf,
    }

    impl TestDir {
        pub fn new(tag: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            sweep_once();
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("calli_{tag}_{}_{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create the test folder");
            let index = dir.join("index.db");
            Self { dir, index }
        }
    }

    impl std::ops::Deref for TestDir {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.index
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn remove_index_files(path: &Path) {
        for ext in EXTENSIONS {
            let _ = std::fs::remove_file(path.with_extension(ext));
        }
    }

    /// Sweep the temp dir once per test process, before its first test index.
    fn sweep_once() {
        static SWEPT: OnceLock<usize> = OnceLock::new();
        SWEPT.get_or_init(|| sweep_dead_test_files(&std::env::temp_dir()));
    }

    /// The pid in a test index file's name, `calli_<tag>_<pid>_<n>.db` (or a companion in
    /// [`EXTENSIONS`]), or None for any other name. Tags may hold underscores.
    pub fn test_file_pid(name: &str) -> Option<u32> {
        let stem = name.strip_prefix("calli_")?;
        let stem = EXTENSIONS
            .iter()
            .find_map(|ext| stem.strip_suffix(ext)?.strip_suffix('.'))?;
        let mut parts = stem.rsplitn(3, '_');
        let n = parts.next()?;
        let pid = parts.next()?;
        let tag = parts.next()?;
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        if !(digits(n) && digits(pid) && !tag.is_empty()) {
            return None;
        }
        pid.parse().ok()
    }

    /// Remove the test index files in `dir` whose process is gone, returning how many.
    /// Parallel test processes (cargo runs one per crate, CI runs jobs side by side) share
    /// the temp dir, so a live pid's files are never touched, and nothing is when liveness
    /// cannot be read. Unix only: Windows has no cheap pid probe without a new dependency.
    pub fn sweep_dead_test_files(dir: &Path) -> usize {
        sweep_dead_test_files_before(dir, std::time::SystemTime::now())
    }

    /// [`sweep_dead_test_files`], removing only files last written before `started` (when
    /// the sweep began). Liveness is read once, and deleting a large backlog takes seconds
    /// (31,507 such files sat in one machine's temp dir): a new test process handed a dead
    /// pid meanwhile recreates the very names on the list (its counter starts at 0), and its
    /// files, written after the sweep began, must survive.
    pub fn sweep_dead_test_files_before(dir: &Path, started: std::time::SystemTime) -> usize {
        if !cfg!(unix) {
            return 0;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        let me = std::process::id();
        let files: Vec<(PathBuf, u32)> = entries
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .filter_map(|e| {
                let pid = test_file_pid(e.file_name().to_str()?)?;
                (pid != me).then(|| (e.path(), pid))
            })
            .collect();
        let pids: Vec<u32> = files
            .iter()
            .map(|(_, pid)| *pid)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let Some(live) = live_pids(&pids) else {
            return 0;
        };
        let stale = |path: &Path| {
            std::fs::metadata(path)
                .and_then(|m| m.modified())
                .is_ok_and(|written| written < started)
        };
        files
            .into_iter()
            .filter(|(path, pid)| {
                !live.contains(pid) && stale(path) && std::fs::remove_file(path).is_ok()
            })
            .count()
    }

    /// Which of `pids` are running, from one `ps` per 500 pids (core has no libc; cleanup
    /// reads free space through `df` too). `ps` exits 1 when a pid is gone, so the answer is
    /// read from its output, and trusted only when it names this process: a `ps` that
    /// printed nothing would otherwise read as "every pid is dead".
    fn live_pids(pids: &[u32]) -> Option<HashSet<u32>> {
        let me = std::process::id();
        let mut live = HashSet::new();
        for chunk in pids.chunks(500) {
            let list = chunk
                .iter()
                .chain([&me])
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",");
            let out = std::process::Command::new("ps")
                .args(["-p", &list, "-o", "pid="])
                .output()
                .ok()?;
            let seen: HashSet<u32> = String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .filter_map(|s| s.parse().ok())
                .collect();
            if !seen.contains(&me) {
                return None;
            }
            live.extend(seen);
        }
        Some(live)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression guard: the cross-platform path switch must NOT move an existing
    /// macOS user's index. The app's own index on macOS must stay byte-identical
    /// to the original hardcoded `~/Library/Application Support/...` location. Asserted on
    /// `app_data_dir()`, which the shipped `default_index_path()` joins `index.db` to: in
    /// this crate's tests the default path itself is a stand-in that does not exist
    /// (`implicit_index_path`).
    #[test]
    #[cfg(target_os = "macos")]
    fn default_index_path_unchanged_on_macos() {
        let home = std::env::var("HOME").expect("HOME is set on macOS");
        let expected = std::path::PathBuf::from(home)
            .join("Library/Application Support/dev.shaller.callimachus/index.db");
        assert_eq!(app_data_dir().join("index.db"), expected);
    }

    use super::test_support::{
        sweep_dead_test_files_before, test_file_pid, TestDir, TestIndex, TestPath,
    };

    // A file rather than an in-memory DB: it exercises the real open() path including the
    // WAL pragmas.
    fn temp_db() -> TestIndex {
        TestIndex::new("db")
    }

    /// A test index's files go when it drops, WAL and shared memory included.
    #[test]
    fn a_test_index_removes_its_files_on_drop() {
        let index = TestIndex::new("drop");
        index
            .execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (1);")
            .unwrap();
        let path = PathBuf::from(index.path().unwrap());
        let wal = path.with_extension("db-wal");
        assert!(path.exists() && wal.exists(), "WAL mode leaves a -wal file");
        drop(index);
        assert!(!path.exists() && !wal.exists());
        assert!(!path.with_extension("db-shm").exists());

        // A handed-out connection's files go when its thread ends.
        let path = std::thread::spawn(|| {
            let conn = TestIndex::new("handed_out").into_connection();
            let path = PathBuf::from(conn.path().unwrap());
            drop(conn);
            assert!(path.exists(), "kept while the thread runs");
            path
        })
        .join()
        .unwrap();
        assert!(!path.exists());
    }

    /// A test folder goes with everything in it (an index, its WAL, lock files) on drop.
    #[test]
    fn a_test_dir_removes_itself_on_drop() {
        let index = TestDir::new("dir");
        let conn = open(&index).unwrap();
        conn.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (1);")
            .unwrap();
        std::fs::write(index.with_file_name("index.db.embed.lock"), b"1").unwrap();
        let dir = index.parent().unwrap().to_path_buf();
        assert!(dir.join("index.db-wal").exists());
        drop(conn);
        drop(index);
        assert!(!dir.exists());
    }

    /// Only `calli_<tag>_<pid>_<n>.db` and its companions name a pid; tags may hold
    /// underscores, and any other shape is left alone.
    #[test]
    fn test_file_names_give_their_pid() {
        assert_eq!(test_file_pid("calli_mcp_123_4.db"), Some(123));
        assert_eq!(test_file_pid("calli_mcp_skew_123_4.db-wal"), Some(123));
        assert_eq!(test_file_pid("calli_digest_hook_9_0.db-shm"), Some(9));
        assert_eq!(test_file_pid("calli_x_7_1.db-journal"), Some(7));
        assert_eq!(test_file_pid("calli_mcp_8_2.ledger.jsonl"), Some(8));
        for other in [
            "calli_cost_123.db",
            "calli_mcp_123_4.jsonl",
            "calli_mcp_123_4",
            "calli__123_4.db",
            "calli_mcp_12a_4.db",
            "calli_mcp_123_.db",
            "callimachus_test_123_4.db",
            "other_mcp_123_4.db",
        ] {
            assert_eq!(test_file_pid(other), None, "{other}");
        }
    }

    /// The sweep removes the files of test processes that are gone and never a live
    /// process's (this one's, or pid 1's), nor anything that is not a test index file.
    #[cfg(unix)]
    #[test]
    fn the_sweep_removes_only_dead_processes_test_files() {
        let dir = std::env::temp_dir().join(format!("calli_sweep_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A pid that was alive a moment ago and is not now.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        let me = std::process::id();
        let gone = [
            format!("calli_mcp_{dead}_0.db"),
            format!("calli_mcp_{dead}_0.db-wal"),
            format!("calli_mcp_skew_{dead}_3.db-shm"),
            format!("calli_mcp_{dead}_0.ledger.jsonl"),
        ];
        let kept = [
            format!("calli_mcp_{me}_0.db"),
            "calli_mcp_1_0.db".to_string(),
            format!("calli_cost_{dead}.db"),
            format!("calli_mcp_{dead}_0.jsonl"),
            format!("other_{dead}_0.db"),
        ];
        for name in gone.iter().chain(&kept) {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        // A directory with a test file's name is not a file.
        std::fs::create_dir(dir.join(format!("calli_dir_{dead}_1.db"))).unwrap();
        // Nothing written after the sweep began goes, whatever its pid: a new test process
        // handed a dead pid recreates these very names while a long sweep is deleting.
        assert_eq!(
            sweep_dead_test_files_before(&dir, std::time::SystemTime::UNIX_EPOCH),
            0
        );
        for name in &gone {
            assert!(dir.join(name).exists(), "{name} is newer than the sweep");
        }
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(1);
        assert_eq!(sweep_dead_test_files_before(&dir, later), gone.len());
        for name in &gone {
            assert!(!dir.join(name).exists(), "{name} removed");
        }
        for name in &kept {
            assert!(dir.join(name).exists(), "{name} kept");
        }
        assert!(dir.join(format!("calli_dir_{dead}_1.db")).is_dir());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_pool_opens_queries_and_has_vec0() {
        let path = TestPath::new("pool");
        let _writer = open(&path).unwrap(); // create + migrate first (readers can't)
        let pool = read_pool(&path, 3).unwrap();
        let conn = pool.0.get().unwrap();
        let sources: i64 = conn
            .query_row("SELECT COUNT(*) FROM sources", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sources, 14, "pooled read sees the migrated schema");
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
        // Idle readers are reaped down to one instead of all staying open (and cached).
        assert_eq!(pool.0.min_idle(), Some(1));
        assert_eq!(pool.0.idle_timeout(), Some(READER_IDLE_TIMEOUT));
        shrink_memory(&c2);
    }

    fn fresh_path(tag: &str) -> TestPath {
        TestPath::new(tag)
    }

    /// A process that finds the schema out of date while another one is upgrading it waits
    /// for that upgrade (well past its normal busy timeout) instead of failing with
    /// "database is locked", then finds nothing left to do.
    #[test]
    fn open_waits_out_another_process_upgrade() {
        let p = fresh_path("upgrade_wait");
        drop(open(&p).unwrap());

        // "Another process": rewinds the version, then holds the write lock mid-upgrade.
        let other = Connection::open(&p).unwrap();
        let version: i64 = other
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        other
            .pragma_update(None, "user_version", version - 1)
            .unwrap();
        other.execute_batch("BEGIN IMMEDIATE").unwrap();
        other.pragma_update(None, "user_version", version).unwrap();

        let path = p.to_path_buf();
        let opener = std::thread::spawn(move || {
            open_with(&path, Duration::from_millis(50), Duration::from_secs(10))
        });
        std::thread::sleep(Duration::from_millis(400)); // far past the 50 ms busy timeout
        other.execute_batch("COMMIT").unwrap();

        let conn = opener.join().unwrap().expect("waited for the upgrade");
        let busy_ms: i64 = conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        assert_eq!(busy_ms, 50, "the normal busy timeout is restored");
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, version);
    }

    /// Hooks open with `open_sidecar_current`, which refuses an index behind this binary's
    /// schema (an upgrade would hold the lock past the hook's budget) or ahead of it (an
    /// older binary must not write a newer schema), and never creates a missing one.
    #[test]
    fn sidecar_current_open_never_migrates() {
        let p = fresh_path("current");
        assert!(open_sidecar_current(&p).is_err(), "missing index");
        assert!(!p.exists(), "a hook never creates the index");
        drop(open(&p).unwrap());
        let conn = open_sidecar_current(&p).expect("current schema opens");
        conn.execute("INSERT INTO app_config (key, value) VALUES ('k', 'v')", [])
            .expect("writable");
        let auto: i64 = conn
            .query_row("PRAGMA wal_autocheckpoint", [], |r| r.get(0))
            .unwrap();
        assert_eq!(auto, 0, "a hook's commit never runs a checkpoint");
        drop(conn);
        let conn = open_sidecar_current_with(&p, Duration::from_millis(700)).unwrap();
        let busy_ms: i64 = conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        assert_eq!(busy_ms, 700, "the caller's wait, not the sidecar's 60 s");
        drop(conn);

        let raw = Connection::open(&p).unwrap();
        let version: i64 = raw
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        raw.pragma_update(None, "user_version", version - 1)
            .unwrap();
        let err = open_sidecar_current(&p).unwrap_err();
        assert!(is_schema_mismatch(&err), "{err:#}");
        let after: i64 = raw
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(after, version - 1, "behind: left for the app to upgrade");

        raw.pragma_update(None, "user_version", version + 1)
            .unwrap();
        let err = open_sidecar_current(&p).unwrap_err();
        assert!(is_schema_mismatch(&err), "{err:#}");
        assert!(!is_schema_mismatch(&anyhow::anyhow!("unrelated")));
    }

    #[test]
    fn busy_errors_are_recognized() {
        let p = fresh_path("busy");
        let holder = open(&p).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();
        let other = open(&p).unwrap();
        other.busy_timeout(Duration::from_millis(10)).unwrap();
        let err = anyhow::Error::from(
            other
                .execute("INSERT INTO app_config (key, value) VALUES ('k', 'v')", [])
                .unwrap_err(),
        )
        .context("writing");
        assert!(is_busy(&err), "{err:#}");
        assert!(!is_busy(&anyhow::anyhow!("unrelated")));
        holder.execute_batch("ROLLBACK").unwrap();
    }

    /// Every shape a lock error reaches a retry loop in: SQLITE_LOCKED as well as BUSY, under
    /// context, behind the app's transparent AppError, and flattened to text.
    #[test]
    fn busy_detection_sees_through_wrappers() {
        let sqlite = |code: i32, msg: &str| {
            rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), Some(msg.into()))
        };
        let locked = sqlite(rusqlite::ffi::SQLITE_LOCKED, "database table is locked");
        assert!(is_busy_sqlite(&locked));
        assert!(is_busy(&anyhow::Error::from(locked).context("saving tags")));

        let busy = sqlite(rusqlite::ffi::SQLITE_BUSY, "database is locked");
        let app = crate::error::AppError::from(anyhow::Error::from(busy));
        assert!(is_busy(&anyhow::Error::from(app).context("command")));

        let flattened =
            anyhow::anyhow!(sqlite(rusqlite::ffi::SQLITE_BUSY, "database is locked").to_string());
        assert!(is_busy(&flattened));

        let constraint = sqlite(rusqlite::ffi::SQLITE_CONSTRAINT, "UNIQUE constraint failed");
        assert!(!is_busy_sqlite(&constraint));
        assert!(!is_busy(&anyhow::Error::from(constraint).context("insert")));
    }

    #[test]
    fn preread_only_for_big_files_that_fit_in_ram() {
        const GIB: u64 = 1 << 30;
        assert!(
            !should_preread(100 << 20, Some(16 * GIB)),
            "small index: skip"
        );
        assert!(should_preread(5 * GIB, Some(16 * GIB)));
        assert!(
            !should_preread(5 * GIB, Some(8 * GIB)),
            "over half of RAM: skip"
        );
        assert!(
            !should_preread(9 * GIB, Some(64 * GIB)),
            "over the fixed cap: skip"
        );
        assert!(
            should_preread(5 * GIB, None),
            "unknown RAM falls back to the fixed cap"
        );
    }

    #[test]
    fn ram_probe_parses_meminfo_and_cgroup_limits() {
        let meminfo = "MemTotal:       16318412 kB\nMemFree:         1234 kB\n";
        assert_eq!(parse_meminfo_total(meminfo), Some(16_318_412 * 1024));
        assert_eq!(parse_meminfo_total("MemFree: 1 kB\n"), None);
        assert_eq!(parse_cgroup_limit("1073741824\n"), Some(1 << 30));
        assert_eq!(parse_cgroup_limit("max\n"), None, "cgroup v2: no limit");
        assert_eq!(
            parse_cgroup_limit("9223372036854771712\n"),
            Some(9_223_372_036_854_771_712),
            "cgroup v1's no-limit value parses; the min with MemTotal drops it"
        );
    }

    /// The probe answers on every platform that ships (macOS, Linux, Windows); `None` there
    /// would silently fall back to the 8 GiB cap whatever the machine has.
    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn ram_probe_answers() {
        let ram = physical_memory().expect("RAM is known on this platform");
        assert!(ram >= 256 << 20, "{ram}");
    }

    #[test]
    fn has_table_reflects_the_schema() {
        let conn = temp_db();
        assert!(has_table(&conn, "vec_chunk_owner"));
        assert!(
            !has_table(&conn, "idx_vec_chunk_owner_message"),
            "an index, not a table"
        );
        conn.execute_batch("DROP TABLE vec_chunk_owner").unwrap();
        assert!(!has_table(&conn, "vec_chunk_owner"), "re-read, not cached");
    }

    #[test]
    fn has_index_reflects_the_schema() {
        let conn = temp_db();
        assert!(has_index(&conn, "idx_messages_cover"));
        assert!(!has_index(&conn, "idx_messages_pending_embed"));
        conn.execute_batch("DROP INDEX idx_messages_cover").unwrap();
        assert!(
            !has_index(&conn, "idx_messages_cover"),
            "re-read, not cached"
        );
    }

    #[test]
    fn migrations_seed_sources() {
        let conn = temp_db();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM sources", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 14, "fourteen source kinds seeded");
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

    /// A unit test that names no index gets one that does not exist, never the machine's own
    /// (see `implicit_index_path`); an override still wins.
    #[test]
    fn a_test_without_an_override_never_names_the_real_index() {
        if std::env::var_os("CALLIMACHUS_DB").is_some() {
            return; // whoever runs the tests chose an index
        }
        let implicit = default_index_path();
        assert!(
            !implicit.starts_with(app_data_dir()),
            "{}",
            implicit.display()
        );
        assert!(!implicit.exists());
        let dir = test_support::TestDir::new("default_override");
        let seen = test_support::with_default_index(&dir, default_index_path);
        assert_eq!(seen, dir.to_path_buf());
        assert_eq!(default_index_path(), implicit);
    }

    /// A panic under the shared connection's guard, inside a raw `BEGIN` (no rusqlite
    /// `Transaction` to roll it back on drop), poisons the mutex and leaves the write lock
    /// held. The next caller still gets the connection, the half-done write is gone, and
    /// another connection can write again.
    #[test]
    fn a_panic_under_the_connection_lock_does_not_break_the_next_caller() {
        let dir = test_support::TestDir::new("poison");
        let db = Db(Mutex::new(open(&dir).unwrap()));
        let wrote = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let conn = lock_conn(&db.0);
            conn.execute_batch(
                "BEGIN IMMEDIATE; INSERT INTO app_config (key, value) VALUES ('half', 'done');",
            )
            .unwrap();
            panic!("a handler bug under the guard");
        }));
        assert!(wrote.is_err());
        assert!(db.0.is_poisoned());

        let conn = lock_conn(&db.0);
        assert!(
            !db.0.is_poisoned(),
            "recovered once, for every later caller"
        );
        assert!(conn.is_autocommit(), "the abandoned BEGIN was rolled back");
        let half: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM app_config WHERE key = 'half'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(half, 0);
        conn.execute(
            "INSERT INTO app_config (key, value) VALUES ('next', 'ok')",
            [],
        )
        .unwrap();
        drop(conn);
        let other = open(&dir).unwrap();
        other
            .execute(
                "INSERT INTO app_config (key, value) VALUES ('other', 'ok')",
                [],
            )
            .expect("the write lock was released");

        // The non-blocking form recovers the same way, and stays None while the lock is held.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let conn = lock_conn(&db.0);
            conn.execute_batch("BEGIN").unwrap();
            panic!("again");
        }));
        let conn = try_lock_conn(&db.0).expect("a poisoned lock is still free");
        assert!(conn.is_autocommit());
        let held = std::thread::scope(|s| s.spawn(|| try_lock_conn(&db.0).is_none()).join());
        assert!(held.unwrap(), "None while another thread holds it");
    }

    #[cfg(unix)]
    fn mode(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    fn set_mode(p: &Path, m: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
    }

    #[cfg(unix)]
    fn sidecar(index: &Path, suffix: &str) -> PathBuf {
        let mut s = index.as_os_str().to_owned();
        s.push(suffix);
        PathBuf::from(s)
    }

    /// A first open in a missing folder makes it owner-only, and the index with its WAL and
    /// shared memory 0600: the index keeps transcripts unredacted.
    #[cfg(unix)]
    #[test]
    fn a_new_index_and_its_folder_are_owner_only() {
        let dir = test_support::TestDir::new("db_perms_new");
        let index = dir.parent().unwrap().join("sub").join("index.db");
        let above = mode(dir.parent().unwrap());
        let conn = open(&index).unwrap();
        conn.execute_batch("CREATE TABLE IF NOT EXISTS t (x); INSERT INTO t VALUES (1);")
            .unwrap();
        assert_eq!(mode(index.parent().unwrap()), 0o700);
        for p in [
            index.clone(),
            sidecar(&index, "-wal"),
            sidecar(&index, "-shm"),
        ] {
            assert_eq!(mode(&p), 0o600, "{}", p.display());
        }
        // Only the new folder: the one above it, which existed, keeps its mode.
        assert_eq!(mode(dir.parent().unwrap()), above);
    }

    /// An index from before files were owner-only (0644) is tightened by the next writable
    /// open, and never by a read-only one, a hook's or a pooled reader's.
    #[cfg(unix)]
    #[test]
    fn writable_opens_tighten_a_wide_index_and_read_only_ones_never_do() {
        let dir = test_support::TestDir::new("db_perms_old");
        drop(open(&dir).unwrap());
        let files = [
            dir.to_path_buf(),
            sidecar(&dir, "-wal"),
            sidecar(&dir, "-shm"),
        ];
        // A live reader keeps the WAL and shared memory around between the opens below.
        let keep = open_readonly(&dir).unwrap();
        keep.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap();
        for p in &files {
            if p.exists() {
                set_mode(p, 0o644);
            }
        }
        let wide = || {
            files
                .iter()
                .filter(|p| p.exists())
                .all(|p| mode(p) == 0o644)
        };
        drop(open_readonly(&dir).unwrap());
        drop(open_sidecar_current(&dir).unwrap());
        drop(open_sidecar_current_long_lived(&dir).unwrap());
        let pool = read_pool(&dir, 2).unwrap();
        drop(pool.0.get().unwrap());
        assert!(wide(), "read-only and hook opens leave modes alone");
        drop(open(&dir).unwrap());
        for p in files.iter().filter(|p| p.exists()) {
            assert_eq!(mode(p), 0o600, "{}", p.display());
        }
        drop(keep);
    }

    /// Only the app's own data folder is tightened when it already exists; a folder the user
    /// chose for `CALLIMACHUS_DB` keeps its mode.
    #[cfg(unix)]
    #[test]
    fn only_the_default_data_folder_is_tightened() {
        let dir = test_support::TestDir::new("db_perms_dirs");
        let theirs = dir.parent().unwrap().join("theirs");
        let ours = dir.parent().unwrap().join("ours");
        for d in [&theirs, &ours] {
            std::fs::create_dir(d).unwrap();
            set_mode(d, 0o755);
        }
        prepare_index_dir(&theirs, &ours).unwrap();
        assert_eq!(mode(&theirs), 0o755);
        prepare_index_dir(&ours, &ours).unwrap();
        assert_eq!(mode(&ours), 0o700);
        // A symbolic link named like the data folder is never followed into a chmod.
        let target = dir.parent().unwrap().join("target");
        std::fs::create_dir(&target).unwrap();
        set_mode(&target, 0o755);
        let link = dir.parent().unwrap().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        prepare_index_dir(&link, &link).unwrap();
        assert_eq!(mode(&target), 0o755);
    }
}
