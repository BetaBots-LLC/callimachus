//! Source indexers. Each agent (Claude Code, Codex, Cursor) parses its on-disk
//! history into the common `ParsedThread` shape, which `upsert_thread` writes into
//! the canonical store. Indexing a thread is idempotent: re-running replaces its
//! messages, so a changed file can be fully re-parsed safely.

pub mod claude;
pub mod cline;
pub mod codex;
pub mod continue_cli;
pub mod cursor;
pub mod gemini;
pub mod goose;
pub mod kilo;
pub mod opencode;
pub mod qwen;
pub mod roo;
pub mod watcher;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use std::path::Path;

/// A conversation thread parsed from a source, ready to persist.
#[derive(Debug, Default)]
pub struct ParsedThread {
    pub external_id: String,
    pub title: Option<String>,
    pub project_path: Option<String>,
    pub git_branch: Option<String>,
    pub created_at: Option<i64>,
    pub updated_at: Option<i64>,
    pub is_subagent: bool,
    pub messages: Vec<ParsedMessage>,
}

/// One message within a thread.
#[derive(Debug)]
pub struct ParsedMessage {
    pub role: String, // user | assistant | tool | system
    pub text: String,
    pub tool_name: Option<String>,
    pub ts: Option<i64>,
}

/// Tally returned to the frontend after an indexing pass.
#[derive(Debug, Default, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexReport {
    pub threads_indexed: usize,
    pub threads_skipped: usize,
    pub messages_indexed: usize,
    pub errors: usize,
}

/// Run every source indexer and sum the reports.
/// Index every source, one at a time. Calls `on_progress(done, total, next_source)`
/// before each — `done` sources already finished, `next_source` about to scan — so a
/// background reindex can drive a progress bar. Pass `|_, _, _| {}` to ignore progress.
pub fn scan_all_with_progress(
    conn: &mut Connection,
    mut on_progress: impl FnMut(usize, usize, &str),
) -> Result<IndexReport> {
    type Scan = fn(&mut Connection) -> Result<IndexReport>;
    let sources: [(&str, Scan); 11] = [
        ("claude_code", claude::scan),
        ("codex", codex::scan),
        ("cursor", cursor::scan),
        ("gemini", gemini::scan),
        ("qwen", qwen::scan),
        ("goose", goose::scan),
        ("opencode", opencode::scan),
        ("continue", continue_cli::scan),
        ("cline", cline::scan),
        ("roo", roo::scan),
        ("kilo", kilo::scan),
    ];
    let n = sources.len();
    let mut total = IndexReport::default();
    for (i, (name, scan)) in sources.into_iter().enumerate() {
        on_progress(i, n, name);
        let r = scan(conn)?;
        total.threads_indexed += r.threads_indexed;
        total.threads_skipped += r.threads_skipped;
        total.messages_indexed += r.messages_indexed;
        total.errors += r.errors;
    }
    on_progress(n, n, "");
    Ok(total)
}

/// Resolve the numeric source id for a source kind (rows are seeded by migration).
pub fn source_id(conn: &Connection, kind: &str) -> Result<i64> {
    conn.query_row("SELECT id FROM sources WHERE kind = ?1", [kind], |r| r.get(0))
        .with_context(|| format!("unknown source kind {kind}"))
}

/// Insert or fully replace a thread and all its messages. Empty threads are dropped.
pub fn upsert_thread(conn: &mut Connection, source_id: i64, thread: &ParsedThread) -> Result<usize> {
    if thread.messages.is_empty() {
        return Ok(0);
    }
    let now = chrono::Utc::now().timestamp();
    // UTF-8 byte size of all message text (String::len is byte length) — stored so the
    // cleanup list reads a column instead of SUM(LENGTH(text)) across every message.
    let bytes: i64 = thread.messages.iter().map(|m| m.text.len() as i64).sum();
    let tx = conn.transaction()?;

    tx.execute(
        "INSERT INTO threads (source_id, external_id, title, project_path, git_branch,
            created_at, updated_at, message_count, last_indexed_at, is_subagent, bytes)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT (source_id, external_id) DO UPDATE SET
            title = excluded.title,
            project_path = excluded.project_path,
            git_branch = excluded.git_branch,
            created_at = excluded.created_at,
            updated_at = excluded.updated_at,
            message_count = excluded.message_count,
            last_indexed_at = excluded.last_indexed_at,
            is_subagent = excluded.is_subagent,
            bytes = excluded.bytes",
        params![
            source_id,
            thread.external_id,
            thread.title,
            thread.project_path,
            thread.git_branch,
            thread.created_at,
            thread.updated_at,
            thread.messages.len() as i64,
            now,
            thread.is_subagent as i64,
            bytes,
        ],
    )?;

    let thread_id: i64 = tx.query_row(
        "SELECT id FROM threads WHERE source_id = ?1 AND external_id = ?2",
        params![source_id, thread.external_id],
        |r| r.get(0),
    )?;

    // Full replace: clear existing messages (FTS triggers keep the index in sync).
    tx.execute("DELETE FROM messages WHERE thread_id = ?1", [thread_id])?;

    let mut inserted: Vec<(i64, &str, &str)> = Vec::with_capacity(thread.messages.len());
    {
        let mut stmt = tx.prepare(
            "INSERT INTO messages (thread_id, seq, role, text, tool_name, ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for (seq, m) in thread.messages.iter().enumerate() {
            stmt.execute(params![
                thread_id,
                seq as i64,
                m.role,
                m.text,
                m.tool_name,
                m.ts
            ])?;
            inserted.push((tx.last_insert_rowid(), m.role.as_str(), m.text.as_str()));
        }
    }

    // Free heuristic knowledge tier: re-derive this thread's TODO facts every index
    // (delete + rescan) so they never go stale. Only user/assistant text is scanned.
    // The LLM-distilled facts (extractor='llm') are left untouched here. Gated on the
    // opt-in: when knowledge is off we don't write todo facts at all.
    if crate::knowledge::get_config(&tx)?.enabled {
        const MAX_TODOS_PER_THREAD: usize = 25;
        tx.execute("DELETE FROM facts WHERE thread_id = ?1 AND extractor = 'heuristic'", [thread_id])?;
        let mut fstmt = tx.prepare(
            "INSERT INTO facts (thread_id, kind, text, source_message_id, status, extractor, created_at)
             VALUES (?1, 'todo', ?2, ?3, 'open', 'heuristic', ?4)",
        )?;
        let mut seen = std::collections::HashSet::new();
        let mut count = 0usize;
        'outer: for (mid, role, text) in &inserted {
            if *role != "user" && *role != "assistant" {
                continue;
            }
            for todo in crate::knowledge::extract_todos(text) {
                if count >= MAX_TODOS_PER_THREAD {
                    break 'outer;
                }
                if seen.insert(todo.to_ascii_lowercase()) {
                    fstmt.execute(params![thread_id, todo, mid, now])?;
                    count += 1;
                }
            }
        }
    }

    // Code-aware search: re-derive this thread's file-path mentions (delete + rescan,
    // every index). Only user/assistant text — tool RESULTS (ls output) would be noise.
    tx.execute("DELETE FROM file_mentions WHERE thread_id = ?1", [thread_id])?;
    {
        const MAX_PATHS_PER_THREAD: usize = 200;
        let mut pstmt =
            tx.prepare("INSERT OR IGNORE INTO file_mentions (thread_id, path) VALUES (?1, ?2)")?;
        let mut seen = std::collections::HashSet::new();
        'paths: for (_mid, role, text) in &inserted {
            if *role != "user" && *role != "assistant" {
                continue;
            }
            for path in extract_paths(text) {
                if seen.len() >= MAX_PATHS_PER_THREAD {
                    break 'paths;
                }
                if seen.insert(path.to_ascii_lowercase()) {
                    pstmt.execute(params![thread_id, path])?;
                }
            }
        }
    }

    // Invalidate LLM-distilled knowledge when the message set actually changed. The
    // upsert above doesn't touch knowledge_msg_count (set at distillation time), so it
    // still holds the prior count here; only threads that were distilled AND changed
    // get reset, so unchanged re-indexes never wipe distilled facts.
    let prev_kcount: Option<i64> = tx
        .query_row("SELECT knowledge_msg_count FROM threads WHERE id = ?1", [thread_id], |r| {
            r.get::<_, Option<i64>>(0)
        })
        .optional()?
        .flatten();
    if let Some(pc) = prev_kcount {
        if pc != thread.messages.len() as i64 {
            tx.execute(
                "UPDATE threads SET knowledge_extracted = 0, knowledge_error = NULL WHERE id = ?1",
                [thread_id],
            )?;
            tx.execute("DELETE FROM facts WHERE thread_id = ?1 AND extractor = 'llm'", [thread_id])?;
        }
    }

    tx.commit()?;
    Ok(thread.messages.len())
}

/// Extensions that mark a bare filename (no slash) as a real file reference.
const CODE_EXTS: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "mjs", "cjs", "py", "go", "java", "kt", "rb", "php", "c", "cc",
    "cpp", "h", "hpp", "cs", "swift", "sql", "sh", "bash", "zsh", "toml", "yaml", "yml", "json",
    "md", "mdx", "css", "scss", "html", "xml", "lock", "cfg", "ini", "vue", "svelte", "proto",
    "gradle", "ex", "exs", "scala", "dart", "lua",
];

/// Extract likely file-path mentions from text. Conservative: a token counts if it has a
/// `/` plus an extension (`src/foo.rs`) or is a bare filename with a known code extension
/// (`mod.rs`). Skips URLs and version-like tokens (`1.2.3`).
pub fn extract_paths(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.split(|c: char| {
        c.is_whitespace()
            || matches!(c, '(' | ')' | '[' | ']' | '{' | '}' | '"' | '\'' | '`' | ',' | ';' | '<' | '>' | '|' | '=')
    }) {
        let t = raw.trim_matches(|c: char| matches!(c, '.' | ':' | '*' | '#' | '!' | '?' | '@'));
        if t.len() < 3 || t.len() > 200 || t.contains("://") {
            continue;
        }
        let Some(dot) = t.rfind('.') else {
            continue;
        };
        let (stem, ext) = (&t[..dot], &t[dot + 1..]);
        if stem.is_empty()
            || ext.is_empty()
            || !ext.bytes().all(|b| b.is_ascii_alphanumeric())
            || ext.bytes().all(|b| b.is_ascii_digit())
        {
            continue;
        }
        if t.contains('/') || CODE_EXTS.contains(&ext.to_ascii_lowercase().as_str()) {
            out.push(t.to_string());
        }
    }
    out
}

/// Read the stored (mtime, size) for a file, if we've indexed it before.
pub fn file_state(conn: &Connection, path: &str) -> Result<Option<(i64, i64)>> {
    Ok(conn
        .query_row(
            "SELECT mtime, size FROM index_state WHERE path = ?1",
            [path],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()?)
}

/// Record that we've indexed a file at its current (mtime, size).
pub fn set_file_state(conn: &Connection, path: &str, kind: &str, mtime: i64, size: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO index_state (path, source_kind, mtime, size, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (path) DO UPDATE SET mtime = ?3, size = ?4, updated_at = ?5",
        params![path, kind, mtime, size, chrono::Utc::now().timestamp()],
    )?;
    Ok(())
}

/// Open another app's SQLite DB (Codex/Cursor) for reading without disturbing it.
/// Tries a plain read-only open first; if that fails because the owning app holds
/// a lock, falls back to an immutable open (ignores any hot WAL — we may miss the
/// very latest uncommitted rows, which is fine for indexing).
pub fn open_external_readonly(path: &Path) -> Result<Connection> {
    let ro = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
    match Connection::open_with_flags(path, ro) {
        Ok(conn) => {
            // A trivial query forces SQLite to actually touch the file now, so a lock
            // surfaces here (and we can fall back) rather than mid-scan.
            if conn.query_row("SELECT 1", [], |_| Ok(())).is_ok() {
                return Ok(conn);
            }
        }
        Err(_) => {}
    }
    // Immutable fallback: file:<path>?immutable=1
    let uri = format!("file:{}?immutable=1", path.to_string_lossy());
    let conn = Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("opening {} read-only", path.display()))?;
    Ok(conn)
}

/// Check a single source file's (mtime, size) against `index_state`; returns true
/// if unchanged since last pass. Used by SQLite-backed sources (one DB file).
pub fn file_unchanged(conn: &Connection, path: &Path, kind: &str) -> Result<bool> {
    let meta = std::fs::metadata(path)?;
    let size = meta.len() as i64;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let path_str = path.to_string_lossy().to_string();
    let unchanged = matches!(file_state(conn, &path_str)?, Some((m, s)) if m == mtime && s == size);
    if !unchanged {
        set_file_state(conn, &path_str, kind, mtime, size)?;
    }
    Ok(unchanged)
}

#[cfg(test)]
mod tests {
    use super::extract_paths;

    #[test]
    fn extract_paths_finds_real_paths_only() {
        let text = "I edited `src/embed/mod.rs` and apps/desktop/package.json, \
            also see README.md. Ran cargo 1.2.3 and visited https://x.com/y.html, \
            and a plain word.foo shouldn't count.";
        let paths = extract_paths(text);
        assert!(paths.iter().any(|p| p == "src/embed/mod.rs"));
        assert!(paths.iter().any(|p| p == "apps/desktop/package.json"));
        assert!(paths.iter().any(|p| p == "README.md")); // bare, known ext
        assert!(!paths.iter().any(|p| p.contains("1.2.3"))); // version, not a path
        assert!(!paths.iter().any(|p| p.contains("x.com"))); // url stripped (://)
        assert!(!paths.iter().any(|p| p == "word.foo")); // unknown ext, no slash
    }
}
