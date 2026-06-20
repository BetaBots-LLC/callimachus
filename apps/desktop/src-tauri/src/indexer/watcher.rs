//! Background file watcher. Watches the three source roots and, on a debounced
//! change, re-scans only the affected source into the canonical store. Runs on its
//! own thread and reaches the DB through the Tauri-managed state.

use crate::db;
use notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebounceEventResult};
use rusqlite::Connection;
use std::path::PathBuf;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

/// Roots to watch, paired with the source kind they belong to.
fn watch_targets() -> Vec<(PathBuf, &'static str)> {
    let mut v = Vec::new();
    if let Some(p) = super::claude::projects_root() {
        v.push((p, super::claude::KIND));
    }
    if let Some(p) = super::codex::codex_root().map(|r| r.join("sessions")) {
        v.push((p, super::codex::KIND));
    }
    // Watch the directory holding Cursor's global DB (not the multi-GB file itself).
    if let Some(db) = super::cursor::global_db_path() {
        if let Some(dir) = db.parent() {
            v.push((dir.to_path_buf(), super::cursor::KIND));
        }
    }
    if let Some(p) = super::gemini::tmp_root() {
        v.push((p, super::gemini::KIND));
    }
    if let Some(p) = super::qwen::tmp_root() {
        v.push((p, super::qwen::KIND));
    }
    if let Some(p) =
        super::goose::sessions_db_path().and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        v.push((p, super::goose::KIND));
    }
    if let Some(p) = super::opencode::storage_root() {
        v.push((p, super::opencode::KIND));
    }
    if let Some(p) = super::continue_cli::sessions_root() {
        v.push((p, super::continue_cli::KIND));
    }
    for p in super::cline::task_roots() {
        v.push((p, super::cline::KIND));
    }
    for p in super::roo::task_roots() {
        v.push((p, super::roo::KIND));
    }
    for p in super::kilo::task_roots() {
        v.push((p, super::kilo::KIND));
    }
    v
}

/// Spawn the watcher thread. Errors are logged, never fatal.
pub fn spawn(app: AppHandle) {
    std::thread::spawn(move || {
        if let Err(e) = run(app) {
            eprintln!("[watcher] stopped: {e}");
        }
    });
}

fn run(app: AppHandle) -> anyhow::Result<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut debouncer = new_debouncer(Duration::from_millis(800), None, tx)?;

    let targets = watch_targets();
    for (path, _) in &targets {
        if path.exists() {
            let _ = debouncer.watch(path, RecursiveMode::Recursive);
        }
    }

    // Index on a DEDICATED connection, separate from the app's shared Mutex<Connection>.
    // Holding the shared lock across a re-scan was freezing every UI query ("stuck
    // loading") whenever an agent wrote a thread file. With WAL, this writer and the
    // UI's readers run concurrently.
    let mut conn = db::open(&db::default_index_path())?;

    // Block on debounced batches until the channel closes (app exit drops debouncer).
    for result in rx {
        let DebounceEventResult::Ok(events) = result else {
            continue;
        };
        let mut kinds = Vec::new();
        for ev in &events {
            for path in &ev.paths {
                let s = path.to_string_lossy();
                let kind = if s.contains("/.claude/") {
                    super::claude::KIND
                } else if s.contains("/.codex/") {
                    super::codex::KIND
                } else if s.contains("/Cursor/") {
                    super::cursor::KIND
                } else if s.contains("/.gemini/") {
                    super::gemini::KIND
                } else if s.contains("/.qwen/") {
                    super::qwen::KIND
                } else if s.contains("/goose/sessions") {
                    super::goose::KIND
                } else if s.contains("/opencode/storage") {
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
                    continue;
                };
                if !kinds.contains(&kind) {
                    kinds.push(kind);
                }
            }
        }
        if kinds.is_empty() {
            continue;
        }
        reindex(&app, &mut conn, &kinds);
    }
    Ok(())
}

/// Re-scan the affected sources and notify the frontend. Writes on its own `conn`, so
/// the UI's shared connection is never blocked across the scan.
fn reindex(app: &AppHandle, conn: &mut Connection, kinds: &[&str]) {
    type Scan = fn(&mut Connection, &mut dyn FnMut()) -> anyhow::Result<super::IndexReport>;
    for &kind in kinds {
        let scan: Scan = match kind {
            k if k == super::claude::KIND => super::claude::scan,
            k if k == super::codex::KIND => super::codex::scan,
            k if k == super::cursor::KIND => super::cursor::scan,
            k if k == super::gemini::KIND => super::gemini::scan,
            k if k == super::qwen::KIND => super::qwen::scan,
            k if k == super::goose::KIND => super::goose::scan,
            k if k == super::opencode::KIND => super::opencode::scan,
            k if k == super::continue_cli::KIND => super::continue_cli::scan,
            k if k == super::cline::KIND => super::cline::scan,
            k if k == super::roo::KIND => super::roo::scan,
            k if k == super::kilo::KIND => super::kilo::scan,
            _ => continue,
        };
        // The watcher is a SECOND writer (alongside the app's shared connection). If it
        // loses a write-lock race past busy_timeout, retry instead of silently dropping
        // the thread — re-scanning is cheap + idempotent (file_state skips done threads).
        let mut report = scan(conn, &mut || {});
        for _ in 0..2 {
            match &report {
                Err(e) if is_db_locked(e) => {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    report = scan(conn, &mut || {});
                }
                _ => break,
            }
        }
        match report {
            Ok(r) if r.threads_indexed > 0 => {
                // Best-effort nudge; the frontend also refetches on window focus.
                let _ = app.emit("index:updated", kind);
            }
            Err(e) => eprintln!("[watcher] {kind}: {e}"),
            _ => {}
        }
    }
}

/// Whether an error is SQLite's transient "database is locked" (busy) race.
fn is_db_locked(e: &anyhow::Error) -> bool {
    let s = e.to_string().to_ascii_lowercase();
    s.contains("database is locked") || s.contains("database table is locked")
}
