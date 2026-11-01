//! Background file watcher. Watches every source's store and, on a debounced change,
//! re-indexes only what changed into the canonical store. Runs on its own thread with
//! its own DB connection.
//!
//! Routing is by watched root (the most specific root containing the changed path), so
//! relocated stores (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`) and Windows paths route the same
//! way as the defaults. Claude Code events are indexed per changed file; every other
//! source re-runs its (fingerprint-skipping) scan. Roots that don't exist at launch are
//! picked up later by a periodic re-check, so an agent installed while the app sits in
//! the tray still gets live indexing.
//!
//! Watch events only cover changes from the moment the watch starts, so once the roots
//! are registered the watcher also runs one catch-up pass over every source, picking up
//! the sessions agents wrote while the app was closed.

use super::scanner_for;
use crate::db;
use notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{new_debouncer_opt, DebounceEventResult, NoCache};
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

/// How often to look for roots that didn't exist when the watcher started.
const ROOT_RECHECK: Duration = Duration::from_secs(60);

/// How long to wait before retrying a catch-up deferred by a running reindex or embed build.
const CATCH_UP_RETRY: Duration = Duration::from_secs(30);

/// Minimum time between two scans of the same source. Cursor's state.vscdb takes writes
/// for all editor state, not just chat, so its WAL changes constantly while Cursor is in
/// use; coalescing keeps that from turning into a continuous rescan loop.
fn min_interval(kind: &str) -> Duration {
    if kind == super::cursor::KIND {
        Duration::from_secs(30)
    } else {
        Duration::ZERO
    }
}

/// One watched store.
#[derive(Debug, Clone)]
struct Target {
    path: PathBuf,
    kind: &'static str,
    mode: RecursiveMode,
    /// When set, only files whose name starts with this belong to the source (the
    /// directory is shared with unrelated files, e.g. other extensions' storage).
    file_prefix: Option<&'static str>,
}

impl Target {
    fn new(path: PathBuf, kind: &'static str) -> Self {
        Self {
            path,
            kind,
            mode: RecursiveMode::Recursive,
            file_prefix: None,
        }
    }
}

/// Stores to watch, each tagged with the source kind it belongs to.
fn watch_targets() -> Vec<Target> {
    let mut v = Vec::new();
    if let Some(p) = super::claude::projects_root() {
        v.push(Target::new(p, super::claude::KIND));
    }
    if let Some(root) = super::codex::codex_root() {
        v.push(Target::new(root.join("sessions"), super::codex::KIND));
        v.push(Target::new(
            root.join("archived_sessions"),
            super::codex::KIND,
        ));
    }
    // Cursor's global DB lives in a directory that also holds every extension's storage:
    // watch only its top level, and only the state.vscdb family of files.
    if let Some(db) = super::cursor::global_db_path() {
        if let Some(dir) = db.parent() {
            v.push(Target {
                path: dir.to_path_buf(),
                kind: super::cursor::KIND,
                mode: RecursiveMode::NonRecursive,
                file_prefix: Some("state.vscdb"),
            });
        }
    }
    if let Some(p) = super::gemini::tmp_root() {
        v.push(Target::new(p, super::gemini::KIND));
    }
    if let Some(p) = super::qwen::tmp_root() {
        v.push(Target::new(p, super::qwen::KIND));
    }
    if let Some(p) =
        super::goose::sessions_db_path().and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        v.push(Target::new(p, super::goose::KIND));
    }
    // Watch the JSON storage tree (legacy) and the directory holding opencode.db
    // (V1 SQLite). Both may exist; the indexer dispatches based on DB presence.
    if let Some(p) = super::opencode::storage_root() {
        v.push(Target::new(p, super::opencode::KIND));
    }
    if let Some(db) = super::opencode::db_path() {
        if let Some(dir) = db.parent() {
            v.push(Target {
                path: dir.to_path_buf(),
                kind: super::opencode::KIND,
                mode: RecursiveMode::NonRecursive,
                file_prefix: Some("opencode.db"),
            });
        }
    }
    if let Some(p) = super::continue_cli::sessions_root() {
        v.push(Target::new(p, super::continue_cli::KIND));
    }
    for p in super::cline::task_roots() {
        v.push(Target::new(p, super::cline::KIND));
    }
    for p in super::roo::task_roots() {
        v.push(Target::new(p, super::roo::KIND));
    }
    for p in super::kilo::task_roots() {
        v.push(Target::new(p, super::kilo::KIND));
    }
    // VS Code-native / Copilot chat: each editor's workspaceStorage + emptyWindowChatSessions.
    for p in super::copilot::watch_roots() {
        v.push(Target::new(p, super::copilot::KIND));
    }
    v
}

/// The routing table: every target under both its configured and its resolved spelling
/// (FSEvents reports symlink-resolved paths, e.g. /private/var for /var), longest root
/// first so nested stores (a Cline task dir inside Cursor's globalStorage) win.
fn routes(targets: &[Target]) -> Vec<Target> {
    let mut out: Vec<Target> = Vec::new();
    for t in targets {
        out.push(t.clone());
        if let Ok(real) = std::fs::canonicalize(&t.path) {
            if real != t.path {
                out.push(Target {
                    path: real,
                    ..t.clone()
                });
            }
        }
    }
    out.sort_by_key(|t| std::cmp::Reverse(t.path.as_os_str().len()));
    out
}

/// Which source a changed path belongs to, if any.
fn route(path: &Path, routes: &[Target]) -> Option<&'static str> {
    let mut under_any_root = false;
    for t in routes {
        if !path.starts_with(&t.path) {
            continue;
        }
        under_any_root = true;
        let accepted = match t.file_prefix {
            None => true,
            Some(prefix) => path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(prefix)),
        };
        if accepted {
            return Some(t.kind);
        }
    }
    if under_any_root {
        return None; // inside a watched dir, but a file that isn't the source's
    }
    legacy_route(path)
}

/// Substring routing for paths that don't start with any known root spelling (a path
/// reported through a different symlink). Normalized to '/' so it matches on Windows.
fn legacy_route(path: &Path) -> Option<&'static str> {
    let s = path.to_string_lossy().replace('\\', "/");
    let kind = if s.contains("/.claude/") {
        super::claude::KIND
    } else if s.contains("/.codex/") {
        super::codex::KIND
    } else if s.contains("/chatSessions/") || s.contains("/emptyWindowChatSessions/") {
        // Must precede the editor-name checks: these live under <editor>/User,
        // so a Cursor chat path also contains "/Cursor/".
        super::copilot::KIND
    } else if s.contains("/Cursor/") && s.contains("/state.vscdb") {
        super::cursor::KIND
    } else if s.contains("/.gemini/") {
        super::gemini::KIND
    } else if s.contains("/.qwen/") {
        super::qwen::KIND
    } else if s.contains("/goose/sessions") {
        super::goose::KIND
    } else if s.contains("/opencode/storage")
        || s.ends_with("/opencode.db")
        || s.ends_with("/opencode.db-wal")
    {
        super::opencode::KIND
    } else if s.contains("/.continue/sessions") {
        super::continue_cli::KIND
    } else if s.contains("saoudrizwan.claude-dev") {
        super::cline::KIND
    } else if s.contains("rooveterinaryinc.roo-cline") {
        super::roo::KIND
    } else if s.contains("kilocode.kilo-code") {
        super::kilo::KIND
    } else {
        return None;
    };
    Some(kind)
}

/// Spawn the watcher thread. Errors are logged, never fatal.
pub fn spawn(app: AppHandle) {
    std::thread::spawn(move || {
        if let Err(e) = run(app) {
            app_log!("[watcher] stopped: {e}");
        }
    });
}

/// Work accumulated for one source until it is due.
#[derive(Default)]
struct Pending {
    paths: Vec<PathBuf>,
    /// The OS dropped events (or a non-path-targeted source): re-run the full scan.
    full: bool,
}

fn run(app: AppHandle) -> anyhow::Result<()> {
    let (tx, rx) = std::sync::mpsc::channel::<DebounceEventResult>();
    // NoCache instead of the default file-id cache (FileIdMap on macOS and Windows): that
    // cache walks and stats every file under each root on every watch() and rescan, only
    // to stitch renames into one event. Routing here reads plain event paths, and an
    // unstitched rename still reports both the old and the new path.
    let mut debouncer = new_debouncer_opt::<_, RecommendedWatcher, NoCache>(
        Duration::from_millis(800),
        None,
        tx,
        NoCache,
        notify::Config::default(),
    )?;

    // Index on a DEDICATED connection, separate from the app's shared Mutex<Connection>.
    // Holding the shared lock across a re-scan was freezing every UI query ("stuck
    // loading") whenever an agent wrote a thread file. With WAL, this writer and the
    // UI's readers run concurrently.
    let mut conn = db::open(&db::default_index_path())?;

    let mut watched: HashSet<PathBuf> = HashSet::new();
    let mut warned: HashSet<PathBuf> = HashSet::new();
    let mut table = register_roots(&conn, &mut debouncer, &mut watched, &mut warned);
    let mut last_root_check = Instant::now();

    let mut pending: HashMap<&'static str, Pending> = HashMap::new();
    let mut last_run: HashMap<&'static str, Instant> = HashMap::new();

    // Not on a first run: until something is indexed, indexing waits for the user to start
    // it from onboarding. Roots are already watched, so a write during the pass is queued.
    let mut catch_up_at = has_threads(&conn).then(Instant::now);

    loop {
        if catch_up_at.is_some_and(|at| at <= Instant::now()) {
            catch_up_at = if catch_up(&app, &mut conn, &mut last_run) {
                None
            } else {
                Some(Instant::now() + CATCH_UP_RETRY)
            };
        }

        // Sleep until the next deferred scan, catch-up retry or root re-check is due.
        let now = Instant::now();
        let next_due = pending
            .keys()
            .map(|k| due_at(k, &last_run))
            .chain(catch_up_at)
            .min()
            .map(|d| d.saturating_duration_since(now));
        let until_root_check = ROOT_RECHECK.saturating_sub(last_root_check.elapsed());
        let timeout = next_due.map_or(until_root_check, |d| d.min(until_root_check));

        match rx.recv_timeout(timeout) {
            Ok(Ok(events)) => {
                for ev in &events {
                    let rescan = ev.need_rescan();
                    for path in &ev.paths {
                        let Some(kind) = route(path, &table) else {
                            continue;
                        };
                        let p = pending.entry(kind).or_default();
                        p.full |= rescan;
                        p.paths.push(path.clone());
                    }
                }
            }
            Ok(Err(errors)) => {
                for e in errors {
                    app_log!("[watcher] watch error: {e}");
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            // The debouncer (and so the sender) is dropped: the app is exiting.
            Err(RecvTimeoutError::Disconnected) => break,
        }

        // Run every source whose work is due.
        let now = Instant::now();
        let due: Vec<&'static str> = pending
            .keys()
            .copied()
            .filter(|k| due_at(k, &last_run) <= now)
            .collect();
        for kind in due {
            if let Some(work) = pending.remove(kind) {
                reindex(&app, &mut conn, kind, work);
                last_run.insert(kind, Instant::now());
            }
        }

        if last_root_check.elapsed() >= ROOT_RECHECK {
            table = register_roots(&conn, &mut debouncer, &mut watched, &mut warned);
            last_root_check = Instant::now();
        }
    }
    Ok(())
}

/// Whether anything has been indexed yet.
fn has_threads(conn: &Connection) -> bool {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM threads)", [], |r| r.get(0))
        .unwrap_or(false)
}

/// Re-scan every source once, on the watcher's own connection, to pick up what changed
/// while the app was closed. Unchanged files cost a stat and one lookup each, so this is
/// fast when there is nothing to do. Holds the reindex flag meanwhile, so a Reindex click
/// doesn't start a second scan of the same files, then kicks auto-distill the way index_all
/// does. Returns false, having done nothing, when a reindex, an embedding build or a
/// whole-file rewrite already holds the writer.
fn catch_up(
    app: &AppHandle,
    conn: &mut Connection,
    last_run: &mut HashMap<&'static str, Instant>,
) -> bool {
    // Same rule as index_all: never fight a semantic build or a VACUUM for the write lock.
    if app.state::<crate::EmbedJob>().is_running() || app.state::<crate::CompactJob>().0.is_set() {
        return false;
    }
    // Cleared on drop, including when a scan panics, so Reindex is never left disabled.
    let Some(job) = crate::JobGuard::try_acquire(&app.state::<crate::IndexJob>().0) else {
        return false; // a reindex is running
    };
    for source in super::SOURCES {
        let full = Pending {
            paths: Vec::new(),
            full: true,
        };
        reindex(app, conn, source.kind, full);
        last_run.insert(source.kind, Instant::now());
    }
    // Released first: auto_distill_kick returns early while IndexJob is set. The launch kick
    // usually lands during this pass and bails, and a drain running when it started yielded
    // to it, so this kick is what distills the sessions written while the app was closed.
    drop(job);
    crate::auto_distill_kick(app);
    true
}

fn due_at(kind: &str, last_run: &HashMap<&'static str, Instant>) -> Instant {
    match last_run.get(kind) {
        Some(t) => *t + min_interval(kind),
        None => Instant::now(),
    }
}

/// Watch every target root that exists and isn't watched yet; returns the routing table.
/// Watch failures (e.g. the Linux inotify watch limit on a big tree) are logged once per
/// root instead of being silently dropped. Re-reads the user's data-root settings first,
/// so a relocated Claude / Codex dir set in Settings gets watched without a restart.
fn register_roots<W: notify::Watcher, C: notify_debouncer_full::FileIdCache>(
    conn: &Connection,
    debouncer: &mut notify_debouncer_full::Debouncer<W, C>,
    watched: &mut HashSet<PathBuf>,
    warned: &mut HashSet<PathBuf>,
) -> Vec<Target> {
    super::claude::load_config_dir_setting(conn);
    super::codex::load_codex_home_setting(conn);
    let targets = watch_targets();
    for t in &targets {
        if watched.contains(&t.path) || !t.path.exists() {
            continue;
        }
        match debouncer.watch(&t.path, t.mode) {
            Ok(()) => {
                watched.insert(t.path.clone());
            }
            Err(e) => {
                if warned.insert(t.path.clone()) {
                    app_log!("[watcher] can't watch {}: {e}", t.path.display());
                }
            }
        }
    }
    routes(&targets)
}

/// Re-index one source and notify the frontend. Writes on its own `conn`, so the UI's
/// shared connection is never blocked across the scan.
fn reindex(app: &AppHandle, conn: &mut Connection, kind: &'static str, work: Pending) {
    let run = |conn: &mut Connection| -> anyhow::Result<super::IndexReport> {
        // Claude Code: index just the transcripts that changed (a debounced batch during
        // an active session names one or two files out of thousands).
        if kind == super::claude::KIND && !work.full {
            return super::claude::index_paths(conn, &work.paths);
        }
        match scanner_for(kind) {
            Some(scan) => scan(conn, &mut || {}),
            None => Ok(super::IndexReport::default()),
        }
    };
    // The watcher is a SECOND writer (alongside the app's shared connection). If it
    // loses a write-lock race past busy_timeout, retry instead of silently dropping
    // the thread — re-scanning is cheap + idempotent (file_state skips done threads).
    let mut report = run(conn);
    for _ in 0..2 {
        match &report {
            Err(e) if db::is_busy(e) => {
                std::thread::sleep(Duration::from_millis(500));
                report = run(conn);
            }
            _ => break,
        }
    }
    match report {
        Ok(r) if r.threads_indexed > 0 => {
            // Nudge the UI (payload: the source kind): main.tsx throttles this (~4s) into
            // refetches of the visible results/stats and marks open threads stale.
            let _ = app.emit("index:updated", kind);
        }
        Err(e) => app_log!("[watcher] {kind}: {e}"),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every kind the watcher can route must have a registered scan, or that source's
    /// events are silently dropped (how Copilot once ended up watched but never
    /// live-indexed).
    #[test]
    fn every_watched_kind_has_a_scanner() {
        for t in watch_targets() {
            assert!(
                scanner_for(t.kind).is_some(),
                "unroutable target {}",
                t.kind
            );
        }
    }

    #[test]
    fn routes_by_most_specific_root_and_file_filter() {
        let base = PathBuf::from("/home/me/.config/Cursor/User");
        let table = routes(&[
            Target {
                path: base.join("globalStorage"),
                kind: super::super::cursor::KIND,
                mode: RecursiveMode::NonRecursive,
                file_prefix: Some("state.vscdb"),
            },
            Target::new(
                base.join("globalStorage/emptyWindowChatSessions"),
                super::super::copilot::KIND,
            ),
            Target::new(
                base.join("globalStorage/saoudrizwan.claude-dev/tasks"),
                super::super::cline::KIND,
            ),
            Target::new(
                PathBuf::from("/data/claude-work/projects"),
                super::super::claude::KIND,
            ),
        ]);
        let r = |p: &str| route(Path::new(p), &table);
        let gs = "/home/me/.config/Cursor/User/globalStorage";
        assert_eq!(r(&format!("{gs}/state.vscdb-wal")), Some("cursor"));
        assert_eq!(
            r(&format!("{gs}/emptyWindowChatSessions/a.json")),
            Some("copilot")
        );
        assert_eq!(
            r(&format!(
                "{gs}/saoudrizwan.claude-dev/tasks/1/ui_messages.json"
            )),
            Some("cline")
        );
        assert_eq!(
            r(&format!("{gs}/some.other-extension/cache.json")),
            None,
            "unrelated extension storage doesn't trigger a Cursor rescan"
        );
        // A relocated Claude config dir (no "/.claude/" in the path) still routes.
        assert_eq!(
            r("/data/claude-work/projects/-proj/s.jsonl"),
            Some("claude_code")
        );
        // Unknown roots fall back to substring routing.
        assert_eq!(
            r("/Users/x/.claude/projects/-p/s.jsonl"),
            Some("claude_code")
        );
    }
}
