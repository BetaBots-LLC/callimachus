//! Persistent diagnostics log for the desktop app (`<data dir>/callimachus.log`).
//!
//! A GUI launch (Finder, Dock, Start menu, and every Windows release build, which uses the
//! GUI subsystem) has no stderr anyone can read, so background failures (index, embed,
//! distill, startup) used to vanish. Lines written through [`write_line`] / `app_log!` are
//! appended to this file (which a user can attach to a bug report) AND go to stderr as before.
//!
//! Deliberately tiny: plain `std::fs` appends, one rotation step at 1 MB
//! (`callimachus.log` -> `callimachus.log.1`), no new crate. Only the GUI process calls
//! [`init`]; in the `cal` CLI and the MCP server `write_line` only writes stderr, so hooks
//! and sidecars never write here.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Rotate once the log passes this size, keeping one previous file.
const MAX_BYTES: u64 = 1024 * 1024;

/// The log file, once `init` ran. The mutex serializes append + rotate across threads.
static LOG: OnceLock<Mutex<PathBuf>> = OnceLock::new();

/// Log a formatted line to stderr and, in the GUI, to the persistent log file.
macro_rules! app_log {
    ($($arg:tt)*) => {
        $crate::applog::write_line(&format!($($arg)*))
    };
}

/// Start persisting log lines to `path` (rotating an oversized file first). Idempotent:
/// later calls are ignored.
pub fn init(path: PathBuf) {
    rotate_if_large(&path);
    let _ = LOG.set(Mutex::new(path));
}

/// Where the log lives, if persistence is on (for error screens and diagnostics).
pub fn path() -> Option<PathBuf> {
    LOG.get().and_then(|m| m.lock().ok().map(|p| p.clone()))
}

/// Write one line: to the log file when [`init`] ran, and to stderr. Never fails or panics;
/// a log that can't be written is simply skipped.
pub fn write_line(msg: &str) {
    if let Some(slot) = LOG.get() {
        if let Ok(path) = slot.lock() {
            rotate_if_large(&path);
            append(&path, msg);
        }
    }
    // Not `eprintln!`: it panics when stderr is broken (EPIPE after `callimachus 2>&1 | head`
    // exits, EIO once a launching terminal hangs up), which would kill whichever background
    // thread logged (the watcher, an index or embed job) and leave its job flag set.
    let _ = writeln!(std::io::stderr(), "{msg}");
}

/// Record panics in the log (thread, location, message) before the default hook prints
/// them. Without this, a panic in a background thread of a GUI launch leaves no trace.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("unnamed");
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "(non-string panic payload)".to_string());
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "unknown location".to_string());
        if let Some(slot) = LOG.get() {
            if let Ok(path) = slot.lock() {
                append(
                    &path,
                    &format!("[panic] thread '{name}' panicked at {location}: {payload}"),
                );
            }
        }
        previous(info);
    }));
}

fn append(path: &Path, msg: &str) {
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    // The log can contain project paths; keep it private to the user on unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    if let Ok(mut f) = opts.open(path) {
        let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
        let _ = writeln!(f, "{ts} {msg}");
    }
}

fn rotate_if_large(path: &Path) {
    let too_big = std::fs::metadata(path)
        .map(|m| m.len() > MAX_BYTES)
        .unwrap_or(false);
    if too_big {
        let mut old = path.as_os_str().to_owned();
        old.push(".1");
        // Replaces the previous `.1`; losing the oldest history is the point of rotation.
        let _ = std::fs::rename(path, PathBuf::from(old));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_once_past_the_cap() {
        let dir = std::env::temp_dir().join(format!("calli_applog_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let log = dir.join("callimachus.log");
        let rotated = dir.join("callimachus.log.1");
        let _ = std::fs::remove_file(&rotated);
        std::fs::write(&log, vec![b'x'; (MAX_BYTES + 1) as usize]).unwrap();
        rotate_if_large(&log);
        assert!(!log.exists(), "the oversized log is moved aside");
        assert!(rotated.exists(), "and kept as .1");
        append(&log, "fresh line");
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.ends_with("fresh line\n"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
