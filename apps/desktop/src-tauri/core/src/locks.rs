//! Which process keeps the index fresh. The desktop app holds `<index>.gui.lock` (for
//! `index.db`, `index.db.gui.lock` next to it) for as long as it runs, and `cal watch` holds
//! `<index>.watch.lock`; each refuses to run its watcher while the other's lock is live, so
//! two watchers never rescan the same files and race each other's auto-distill rounds
//! (which spend the user's daily allowance). `cal distill --auto` holds
//! `<index>.distill.lock` for its drain, and the app's and `cal watch`'s auto-distill kicks
//! skip while it is live. `cal index` holds `<index>.index.lock` for its pass, so the Claude
//! Code plugin's SessionStart guard (`cal index --if-stale`) never starts a second pass while
//! one runs (when the last one finished is `jobs::LAST_PASS_AT` in the index, stamped at its
//! end, not this file's mtime, which a pass that died at once also set). The locks are named
//! after the index file, not its directory:
//! `pnpm desktop:dev` keeps `index.dev.db` next to the installed app's `index.db`, and a
//! shared directory lock made `cal watch` on one index refuse because of an app on the other.
//!
//! Liveness is an OS file lock (`File::try_lock`: flock on Unix, LockFileEx on Windows), not
//! the pid inside the file: the OS drops the lock when the holder exits or crashes, so a
//! stale file never blocks anyone and a reused pid is never mistaken for a live holder. The
//! pid is written for the messages that name the holder. The file is never deleted (an
//! unlink racing a new holder could leave two processes each holding a lock on a different
//! file of the same name).
//!
//! Each side takes its own lock first and then checks the other's, as the job flags do, so
//! two processes starting at once can both refuse but never both run.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Held by the desktop app while it runs.
pub const GUI_LOCK: &str = "gui.lock";
/// Held by `cal watch` while it runs.
pub const WATCH_LOCK: &str = "watch.lock";
/// Held by `cal distill --auto` for its drain.
pub const DISTILL_LOCK: &str = "distill.lock";
/// Held by `cal index` for its pass (run by hand, or in the background by the plugin's
/// SessionStart guard).
pub const INDEX_LOCK: &str = "index.lock";

/// A lock this process holds until it drops (or exits).
#[derive(Debug)]
pub struct ProcessLock {
    _file: File,
    path: PathBuf,
}

impl ProcessLock {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// What [`acquire`] found.
#[derive(Debug)]
pub enum Acquire {
    Acquired(ProcessLock),
    /// Another live process holds it; its pid when the file could be read.
    Held(Option<u32>),
}

/// The lock file `name` of the index at `index`: `<index file name>.<name>` next to it.
pub fn lock_path(index: &Path, name: &str) -> PathBuf {
    let file = match index.file_name() {
        Some(f) => format!("{}.{name}", f.to_string_lossy()),
        None => name.to_string(),
    };
    match index.parent() {
        Some(dir) => dir.join(file),
        None => PathBuf::from(file),
    }
}

/// Take the lock `name` next to `index` for the life of this process and write our pid
/// into it. Creates the index directory if needed.
pub fn acquire(index: &Path, name: &str) -> std::io::Result<Acquire> {
    let path = lock_path(index, name);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Not `truncate(true)`: the file may be a live holder's, whose pid must survive this
    // open. It is truncated below, once the lock is ours.
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    // A `holder` probe takes a shared lock for an instant, which an exclusive lock waits
    // out; a few short retries keep a probe from reading as a live holder at startup.
    for attempt in 0..3 {
        match file.try_lock() {
            Ok(()) => {
                file.set_len(0)?;
                file.seek(SeekFrom::Start(0))?;
                write!(file, "{}", std::process::id())?;
                file.flush()?;
                return Ok(Acquire::Acquired(ProcessLock { _file: file, path }));
            }
            Err(TryLockError::WouldBlock) if attempt < 2 => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(TryLockError::WouldBlock) => return Ok(Acquire::Held(read_pid(&mut file))),
            Err(TryLockError::Error(e)) => return Err(e),
        }
    }
    unreachable!("the last attempt returns")
}

/// Whether a live process holds `name` for `index`: Some(its pid, if readable), or None
/// when nobody does (no file, or a stale one). Some(None) also when the probe cannot tell
/// (the file cannot be opened, or the filesystem refuses locks): reading that as free let
/// both sides run their watchers, while reading it as held only keeps one side waiting.
pub fn holder(index: &Path, name: &str) -> Option<Option<u32>> {
    let path = lock_path(index, name);
    // Read-only: a lock file another user created (`sudo cal watch`, a shared
    // CALLIMACHUS_DB) is often not writable by us but still lockable.
    let mut file = match OpenOptions::new().read(true).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => return Some(None),
    };
    // Shared, not exclusive: two probes at once (the Settings poll and the app's recheck
    // loop, or the app against `cal distill --auto`) never see each other as a holder, while
    // a holder's exclusive lock still blocks this one.
    match file.try_lock_shared() {
        // Free: shared by us for an instant, released when `file` drops.
        Ok(()) => None,
        Err(TryLockError::WouldBlock) => Some(read_pid(&mut file)),
        Err(TryLockError::Error(_)) => Some(None),
    }
}

/// The pid in a lock file. Windows refuses reads of a range another process locked, so the
/// pid is unknown there.
fn read_pid(file: &mut File) -> Option<u32> {
    let mut text = String::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.read_to_string(&mut text).ok()?;
    text.trim().parse().ok()
}

/// " (pid N)" for a message, or "" when the pid is unknown.
pub fn pid_note(pid: Option<u32>) -> String {
    pid.map(|p| format!(" (pid {p})")).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index_in(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("calli_locks_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("index.db")
    }

    /// One holder at a time, the holder's pid is readable, and dropping the lock frees it
    /// without deleting the file (a stale file never reads as live).
    #[test]
    fn a_lock_has_one_live_holder_and_frees_on_drop() {
        let index = index_in("one_holder");
        assert!(holder(&index, WATCH_LOCK).is_none(), "no file yet");
        let Acquire::Acquired(lock) = acquire(&index, WATCH_LOCK).unwrap() else {
            panic!("a free lock is acquired");
        };
        assert!(lock.path().ends_with("index.db.watch.lock"));
        // flock is per open file description, so a second open in this process contends
        // like another process would.
        match acquire(&index, WATCH_LOCK).unwrap() {
            Acquire::Held(pid) => {
                if cfg!(unix) {
                    assert_eq!(pid, Some(std::process::id()));
                }
            }
            Acquire::Acquired(_) => panic!("acquired twice"),
        }
        assert!(holder(&index, WATCH_LOCK).is_some());
        assert!(
            holder(&index, GUI_LOCK).is_none(),
            "the other lock is separate"
        );
        drop(lock);
        assert!(lock_path(&index, WATCH_LOCK).exists(), "the file stays");
        assert!(
            holder(&index, WATCH_LOCK).is_none(),
            "a stale file is not live"
        );
        assert!(matches!(
            acquire(&index, WATCH_LOCK).unwrap(),
            Acquire::Acquired(_)
        ));
    }

    /// Each index file has its own locks: the dev app's `index.dev.db` sits next to the
    /// installed app's `index.db`, and neither may block the other's watcher.
    #[test]
    fn locks_are_per_index_file_not_per_directory() {
        let index = index_in("per_file");
        let dev = index.with_file_name("index.dev.db");
        assert_eq!(
            lock_path(&index, GUI_LOCK),
            index.with_file_name("index.db.gui.lock")
        );
        let Acquire::Acquired(_gui) = acquire(&index, GUI_LOCK).unwrap() else {
            panic!("a free lock is acquired");
        };
        assert!(holder(&index, GUI_LOCK).is_some());
        assert!(
            holder(&dev, GUI_LOCK).is_none(),
            "another index's lock is separate"
        );
        assert!(matches!(
            acquire(&dev, GUI_LOCK).unwrap(),
            Acquire::Acquired(_)
        ));
    }

    /// Probes take a shared lock, so two at once never read as a holder to each other, and
    /// a probe in flight does not stop `acquire`.
    #[test]
    fn concurrent_probes_do_not_see_each_other() {
        let index = index_in("probes");
        let Acquire::Acquired(lock) = acquire(&index, WATCH_LOCK).unwrap() else {
            panic!("a free lock is acquired");
        };
        drop(lock);
        // A probe's shared lock held open, as a second probe would see it mid-flight.
        let probe = File::open(lock_path(&index, WATCH_LOCK)).unwrap();
        probe.try_lock_shared().unwrap();
        assert!(
            holder(&index, WATCH_LOCK).is_none(),
            "a probe is not a holder"
        );
        drop(probe);
        assert!(matches!(
            acquire(&index, WATCH_LOCK).unwrap(),
            Acquire::Acquired(_)
        ));
    }

    /// A lock file that exists but cannot be opened reads as held (pid unknown), never as
    /// free.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_lock_file_reads_as_held() {
        use std::os::unix::fs::PermissionsExt;
        let index = index_in("unreadable");
        let Acquire::Acquired(lock) = acquire(&index, WATCH_LOCK).unwrap() else {
            panic!("a free lock is acquired");
        };
        drop(lock);
        let path = lock_path(&index, WATCH_LOCK);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        // root opens any file, so the probe can only fail for a normal user.
        if File::open(&path).is_err() {
            assert_eq!(holder(&index, WATCH_LOCK), Some(None));
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
}
