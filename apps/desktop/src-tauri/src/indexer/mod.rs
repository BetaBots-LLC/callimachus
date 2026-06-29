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
    /// Token usage per assistant turn, keyed by the turn's first-message INDEX in `messages`.
    /// Populated by sources that report it (Claude Code); powers the cost/spend layer.
    pub usage: Vec<(usize, MsgUsage)>,
}

/// Token usage + model for one assistant API turn (from the source's `usage` block).
#[derive(Debug, Clone, Default)]
pub struct MsgUsage {
    pub model: String,
    pub input: i64,
    pub output: i64,
    pub cache_write: i64,
    pub cache_read: i64,
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

/// Index every source, one at a time, summing the reports. Calls `on_progress(threads_seen,
/// current_source)` thread-granularly (throttled) so a background reindex can drive a live
/// progress bar even while one big source churns. Pass `|_, _| {}` to ignore progress.
pub fn scan_all_with_progress(
    conn: &mut Connection,
    mut on_progress: impl FnMut(usize, &str),
) -> Result<IndexReport> {
    type Scan = fn(&mut Connection, &mut dyn FnMut()) -> Result<IndexReport>;
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
    // Progress is THREAD-granular, not source-granular: each source ticks once per thread
    // it processes (indexed OR skipped), so the bar keeps moving even while one big source
    // (usually Claude Code) churns through thousands of files. `seen` is the running count
    // across all sources; the total isn't known up front, so the UI renders it indeterminate.
    let mut total = IndexReport::default();
    let mut seen = 0usize;
    for (name, scan) in sources.into_iter() {
        on_progress(seen, name);
        // Isolate per-source failures: a single source erroring (a missing/locked
        // agent DB, an OS-specific quirk, a malformed store) must NOT abort the whole
        // index and zero out every other source. Log it, count it, keep going — the
        // same resilience each scan already applies per-file.
        match scan(conn, &mut || {
            seen += 1;
            // Emit the first 50 per-thread (so movement is obviously live), then every 10.
            if seen <= 50 || seen.is_multiple_of(10) {
                on_progress(seen, name);
            }
        }) {
            Ok(r) => {
                total.threads_indexed += r.threads_indexed;
                total.threads_skipped += r.threads_skipped;
                total.messages_indexed += r.messages_indexed;
                total.errors += r.errors;
            }
            Err(e) => {
                eprintln!("[index] source '{name}' failed: {e:#}");
                total.errors += 1;
            }
        }
    }
    on_progress(seen, "");
    Ok(total)
}

/// Normalize a project path into a STABLE key, so one repo doesn't fragment into separate
/// memories across `~` vs absolute, trailing slashes, symlinks, or a subdir vs the repo
/// root. When the path exists on disk we resolve it and walk up to the nearest git root;
/// otherwise we apply a light string normalization. Returns None for an empty path. The
/// result is what Project Memory / recall scoping group on (column `threads.project_key`).
pub fn canonical_project(path: &str) -> Option<String> {
    use std::path::PathBuf;
    let p = path.trim();
    if p.is_empty() {
        return None;
    }
    // Expand a leading `~`.
    let expanded: PathBuf = match p.strip_prefix("~/") {
        Some(rest) => dirs::home_dir()
            .map(|h| h.join(rest))
            .unwrap_or_else(|| PathBuf::from(p)),
        None => PathBuf::from(p),
    };
    // If it exists: resolve symlinks + absolutize, then walk up to the git root.
    if let Ok(abs) = std::fs::canonicalize(&expanded) {
        let mut dir = abs.as_path();
        loop {
            if dir.join(".git").exists() {
                return Some(dir.to_string_lossy().trim_end_matches('/').to_string());
            }
            match dir.parent() {
                Some(par) => dir = par,
                None => break,
            }
        }
        return Some(abs.to_string_lossy().trim_end_matches('/').to_string());
    }
    // Doesn't exist on this machine: light normalization only.
    Some(expanded.to_string_lossy().trim_end_matches('/').to_string())
}

/// Compute `project_key` for threads that don't have one yet (post-0016 backfill). Groups
/// by distinct project_path so canonicalization runs once per path. Fast (paths are few).
pub fn backfill_project_keys(conn: &Connection) -> Result<usize> {
    let paths: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT project_path FROM threads
             WHERE project_key IS NULL AND project_path IS NOT NULL AND project_path != ''",
        )?;
        let r = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        r
    };
    let mut n = 0;
    for path in paths {
        if let Some(key) = canonical_project(&path) {
            n += conn.execute(
                "UPDATE threads SET project_key = ?1 WHERE project_path = ?2 AND project_key IS NULL",
                params![key, path],
            )?;
        }
    }
    Ok(n)
}

/// Resolve the numeric source id for a source kind (rows are seeded by migration).
pub fn source_id(conn: &Connection, kind: &str) -> Result<i64> {
    conn.query_row("SELECT id FROM sources WHERE kind = ?1", [kind], |r| {
        r.get(0)
    })
    .with_context(|| format!("unknown source kind {kind}"))
}

/// Insert or fully replace a thread and all its messages. Empty threads are dropped.
pub fn upsert_thread(
    conn: &mut Connection,
    source_id: i64,
    thread: &ParsedThread,
) -> Result<usize> {
    if thread.messages.is_empty() {
        return Ok(0);
    }
    let t0 = std::time::Instant::now();
    let now = chrono::Utc::now().timestamp();
    // UTF-8 byte size of all message text (String::len is byte length) — stored so the
    // cleanup list reads a column instead of SUM(LENGTH(text)) across every message.
    let bytes: i64 = thread.messages.iter().map(|m| m.text.len() as i64).sum();
    // Count of DISTILLABLE messages (what the LLM actually sees); distill staleness keys off
    // this, not total message_count, so appended tool/system rows don't re-trigger a distill.
    let distillable: i64 = thread
        .messages
        .iter()
        .filter(|m| m.role == "user" || m.role == "assistant")
        .count() as i64;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

    let project_key = thread.project_path.as_deref().and_then(canonical_project);
    tx.execute(
        "INSERT INTO threads (source_id, external_id, title, project_path, git_branch,
            created_at, updated_at, message_count, last_indexed_at, is_subagent, bytes, project_key,
            distillable_count)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT (source_id, external_id) DO UPDATE SET
            title = excluded.title,
            project_path = excluded.project_path,
            git_branch = excluded.git_branch,
            created_at = excluded.created_at,
            updated_at = excluded.updated_at,
            message_count = excluded.message_count,
            last_indexed_at = excluded.last_indexed_at,
            is_subagent = excluded.is_subagent,
            bytes = excluded.bytes,
            project_key = excluded.project_key,
            distillable_count = excluded.distillable_count",
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
            project_key,
            distillable,
        ],
    )?;

    let thread_id: i64 = tx.query_row(
        "SELECT id FROM threads WHERE source_id = ?1 AND external_id = ?2",
        params![source_id, thread.external_id],
        |r| r.get(0),
    )?;

    // Incremental indexing: agent transcripts are append-only, so when the stored messages
    // are an EXACT prefix of the new parse we insert ONLY the new tail instead of re-inserting
    // and re-FTS-ing the whole thread (the dominant cost on long, actively-growing sessions).
    // We verify the prefix by reading every stored message and comparing it to the new parse:
    // a read-only scan with no FTS work, still far cheaper than the full rewrite, and unlike
    // point-sampling it can never keep a stale in-place edit — any mismatch or shrink falls
    // back to a correct full replace (also covers rewritten / DB-backed sources). When
    // n == existing_count and the prefix matches, the tail is empty: a clean no-op.
    let existing_count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM messages WHERE thread_id = ?1",
        [thread_id],
        |r| r.get(0),
    )?;
    let n = thread.messages.len() as i64;
    let incremental = existing_count > 0 && n >= existing_count && {
        let mut stmt =
            tx.prepare("SELECT seq, text FROM messages WHERE thread_id = ?1 ORDER BY seq")?;
        let mut matched = 0i64;
        let mut ok = true;
        let rows = stmt.query_map([thread_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (seq, text) = row?;
            if seq < 0
                || seq as usize >= thread.messages.len()
                || thread.messages[seq as usize].text != text
            {
                ok = false;
                break;
            }
            matched += 1;
        }
        ok && matched == existing_count
    };

    if !incremental {
        // Full replace: clear existing messages (FTS triggers keep the index in sync).
        tx.execute("DELETE FROM messages WHERE thread_id = ?1", [thread_id])?;
    }
    // On an append, continue the seq numbering after the stored prefix and only touch the
    // tail; on a full replace, (re)insert everything from seq 0.
    let start_seq = if incremental {
        existing_count as usize
    } else {
        0
    };

    let mut inserted: Vec<(i64, &str, &str)> =
        Vec::with_capacity(thread.messages.len().saturating_sub(start_seq));
    {
        let usage_by_idx: std::collections::HashMap<usize, &MsgUsage> =
            thread.usage.iter().map(|(i, u)| (*i, u)).collect();
        let mut stmt = tx.prepare(
            "INSERT INTO messages
                (thread_id, seq, role, text, tool_name, ts,
                 model, input_tokens, output_tokens, cache_write_tokens, cache_read_tokens)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        )?;
        for (seq, m) in thread.messages.iter().enumerate().skip(start_seq) {
            let u = usage_by_idx.get(&seq);
            stmt.execute(params![
                thread_id,
                seq as i64,
                m.role,
                m.text,
                m.tool_name,
                m.ts,
                u.map(|u| u.model.as_str()),
                u.map(|u| u.input),
                u.map(|u| u.output),
                u.map(|u| u.cache_write),
                u.map(|u| u.cache_read),
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
        // Keep CURATED todos (done / dismissed / pinned) so closing one survives re-index;
        // re-derive only the open, untouched ones.
        // Full replace re-derives open heuristic todos from scratch; an append keeps the
        // existing ones (the `seen` seed below dedups) and scans only the new tail.
        if !incremental {
            tx.execute(
                "DELETE FROM facts WHERE thread_id = ?1 AND extractor = 'heuristic'
                    AND status = 'open' AND hidden = 0 AND pinned = 0",
                [thread_id],
            )?;
        }
        let mut fstmt = tx.prepare(
            "INSERT INTO facts (thread_id, kind, text, source_message_id, status, extractor, created_at)
             VALUES (?1, 'todo', ?2, ?3, 'open', 'heuristic', ?4)",
        )?;
        let mut seen = std::collections::HashSet::new();
        // Seed with kept curated todos so we don't re-add an open copy of a closed one.
        {
            let mut kept = tx.prepare(
                "SELECT text FROM facts WHERE thread_id = ?1 AND extractor = 'heuristic'",
            )?;
            for t in kept
                .query_map([thread_id], |r| r.get::<_, String>(0))?
                .flatten()
            {
                seen.insert(t.to_ascii_lowercase());
            }
        }
        // The cap is a per-thread TOTAL; on an append the existing open todos weren't deleted,
        // so they still count toward it — seed the counter instead of starting at 0.
        let mut count: usize = if incremental {
            tx.query_row(
                "SELECT COUNT(*) FROM facts WHERE thread_id = ?1
                    AND extractor = 'heuristic' AND status = 'open'",
                [thread_id],
                |r| r.get::<_, i64>(0),
            )? as usize
        } else {
            0
        };
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
    // Full replace re-derives file mentions; an append keeps existing + adds the tail's
    // (INSERT OR IGNORE dedups at the unique constraint).
    if !incremental {
        tx.execute(
            "DELETE FROM file_mentions WHERE thread_id = ?1",
            [thread_id],
        )?;
    }
    {
        const MAX_PATHS_PER_THREAD: usize = 200;
        let mut pstmt =
            tx.prepare("INSERT OR IGNORE INTO file_mentions (thread_id, path) VALUES (?1, ?2)")?;
        let mut seen = std::collections::HashSet::new();
        // On an append the existing mentions are kept, so seed `seen` with them — both to
        // dedup and so the per-thread cap counts the thread total, not just the new tail.
        if incremental {
            let mut existing = tx.prepare("SELECT path FROM file_mentions WHERE thread_id = ?1")?;
            for p in existing
                .query_map([thread_id], |r| r.get::<_, String>(0))?
                .flatten()
            {
                seen.insert(p.to_ascii_lowercase());
            }
        }
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
    // get reset, so unchanged re-indexes never wipe distilled facts. A full replace of an
    // existing thread (prefix mismatch / shrink) means the content changed even if the
    // count didn't (e.g. an in-place edit), so treat that as changed too.
    let content_changed = existing_count > 0 && !incremental;
    let prev_kcount: Option<i64> = tx
        .query_row(
            "SELECT knowledge_msg_count FROM threads WHERE id = ?1",
            [thread_id],
            |r| r.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten();
    if let Some(pc) = prev_kcount {
        // Compare against the DISTILLABLE count (matches what store_distilled records), so
        // appended tool/system rows alone don't invalidate distilled knowledge.
        if pc != distillable || content_changed {
            tx.execute(
                "UPDATE threads SET knowledge_extracted = 0, knowledge_error = NULL WHERE id = ?1",
                [thread_id],
            )?;
            // Keep curated facts (pinned / edited / hidden) even when the thread changed.
            tx.execute(
                "DELETE FROM facts WHERE thread_id = ?1 AND extractor = 'llm'
                    AND pinned = 0 AND edited = 0 AND hidden = 0",
                [thread_id],
            )?;
        }
    }

    tx.commit()?;
    // A re-index does a full message replace; on a huge changed thread (FTS re-tokenizes
    // everything) that can take seconds. Log the slow ones so a stall is visible/explained
    // in the terminal rather than looking like a hang.
    let elapsed = t0.elapsed();
    if elapsed.as_millis() > 1500 {
        eprintln!(
            "[index] slow thread: {} messages, {} KB took {:.1}s",
            thread.messages.len(),
            bytes / 1024,
            elapsed.as_secs_f64(),
        );
    }
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
            || matches!(
                c,
                '(' | ')'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | '"'
                    | '\''
                    | '`'
                    | ','
                    | ';'
                    | '<'
                    | '>'
                    | '|'
                    | '='
            )
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
pub fn set_file_state(
    conn: &Connection,
    path: &str,
    kind: &str,
    mtime: i64,
    size: i64,
) -> Result<()> {
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
    if let Ok(conn) = Connection::open_with_flags(path, ro) {
        // A trivial query forces SQLite to actually touch the file now, so a lock
        // surfaces here (and we can fall back) rather than mid-scan.
        if conn.query_row("SELECT 1", [], |_| Ok(())).is_ok() {
            return Ok(conn);
        }
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

/// Read-only change check for a single-file source (Cursor/Goose DBs). Returns
/// `(unchanged, mtime, size)`. Unlike a write-on-check, this NEVER advances `index_state`:
/// the caller must persist the new state with `set_file_state` only AFTER its upserts
/// succeed. Writing it here (before the work) would let the watcher's retry skip threads
/// that failed mid-pass on a transient write-lock, silently dropping them.
pub fn file_change_state(conn: &Connection, path: &Path) -> Result<(bool, i64, i64)> {
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
    Ok((unchanged, mtime, size))
}

#[cfg(test)]
mod tests {
    use super::{
        canonical_project, extract_paths, source_id, upsert_thread, ParsedMessage, ParsedThread,
    };

    fn thread_with(texts: &[&str]) -> ParsedThread {
        ParsedThread {
            external_id: "t1".into(),
            title: Some("t".into()),
            project_path: None,
            git_branch: None,
            created_at: None,
            updated_at: None,
            is_subagent: false,
            usage: Vec::new(),
            messages: texts
                .iter()
                .enumerate()
                .map(|(i, t)| ParsedMessage {
                    role: "user".into(),
                    text: (*t).to_string(),
                    tool_name: None,
                    ts: Some(i as i64),
                })
                .collect(),
        }
    }

    fn message_rowids(conn: &rusqlite::Connection, tid: i64) -> Vec<i64> {
        conn.prepare("SELECT id FROM messages WHERE thread_id = ?1 ORDER BY seq")
            .unwrap()
            .query_map([tid], |r| r.get(0))
            .unwrap()
            .flatten()
            .collect()
    }

    #[test]
    fn incremental_reindex_appends_tail_only() {
        let p = std::env::temp_dir().join(format!("calli_incr_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let mut conn = crate::db::open(&p).unwrap();
        let sid = source_id(&conn, "claude_code").unwrap();

        // Initial index: 3 messages, all inserted.
        upsert_thread(&mut conn, sid, &thread_with(&["alpha", "beta", "gamma"])).unwrap();
        let tid: i64 = conn
            .query_row("SELECT id FROM threads WHERE external_id = 't1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let before = message_rowids(&conn, tid);
        assert_eq!(before.len(), 3);

        // Re-index after two messages were appended: the new tail is inserted, the existing
        // three rows are reused (rowids unchanged) — proof we didn't re-insert the prefix.
        upsert_thread(
            &mut conn,
            sid,
            &thread_with(&["alpha", "beta", "gamma", "delta", "epsilon"]),
        )
        .unwrap();
        let after = message_rowids(&conn, tid);
        assert_eq!(after.len(), 5);
        assert_eq!(
            &after[..3],
            &before[..],
            "prefix rows must be reused on append"
        );

        // A shrink falls back to a full replace: count drops to 3 and the new content lands.
        // (Rowid identity can't be asserted here — SQLite reuses rowids after a full delete.)
        upsert_thread(&mut conn, sid, &thread_with(&["alpha", "CHANGED", "gamma"])).unwrap();
        assert_eq!(message_rowids(&conn, tid).len(), 3, "shrink → full replace");
        let seq1: String = conn
            .query_row(
                "SELECT text FROM messages WHERE thread_id = ?1 AND seq = 1",
                [tid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(seq1, "CHANGED", "full replace applies the edit");

        // A same-length in-place edit of a prefix message forces a full replace (the prefix
        // no longer matches), so the new text lands instead of being kept stale.
        upsert_thread(&mut conn, sid, &thread_with(&["alpha", "EDITED", "gamma"])).unwrap();
        let seq1b: String = conn
            .query_row(
                "SELECT text FROM messages WHERE thread_id = ?1 AND seq = 1",
                [tid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(seq1b, "EDITED", "same-length middle edit is not missed");

        // Regression for the sampling bug: a thread with >=4 messages whose UNSAMPLED interior
        // message is edited in place (same length) must still full-replace. Grow to 5, then
        // edit seq 3 ("dddd" -> "DDDD"); point-sampling {0,2,4} would have missed seq 3.
        upsert_thread(
            &mut conn,
            sid,
            &thread_with(&["alpha", "EDITED", "gamma", "dddd", "eeee"]),
        )
        .unwrap();
        upsert_thread(
            &mut conn,
            sid,
            &thread_with(&["alpha", "EDITED", "gamma", "DDDD", "eeee"]),
        )
        .unwrap();
        let seq3: String = conn
            .query_row(
                "SELECT text FROM messages WHERE thread_id = ?1 AND seq = 3",
                [tid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            seq3, "DDDD",
            "interior same-length edit on a long thread is not missed"
        );

        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn distillable_count_excludes_tool_rows_and_gates_invalidation() {
        let p = std::env::temp_dir().join(format!("calli_dc_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let mut conn = crate::db::open(&p).unwrap();
        let sid = source_id(&conn, "claude_code").unwrap();

        let mk = |roles: &[(&str, &str)]| ParsedThread {
            external_id: "t1".into(),
            title: Some("t".into()),
            messages: roles
                .iter()
                .enumerate()
                .map(|(i, (role, text))| ParsedMessage {
                    role: (*role).to_string(),
                    text: (*text).to_string(),
                    tool_name: None,
                    ts: Some(i as i64),
                })
                .collect(),
            ..Default::default()
        };
        let base: &[(&str, &str)] = &[
            ("user", "a"),
            ("assistant", "b"),
            ("user", "c"),
            ("assistant", "d"),
        ];
        upsert_thread(&mut conn, sid, &mk(base)).unwrap();
        let tid: i64 = conn
            .query_row("SELECT id FROM threads WHERE external_id = 't1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let col = |c: &rusqlite::Connection, name: &str| -> i64 {
            c.query_row(
                &format!("SELECT {name} FROM threads WHERE id = ?1"),
                [tid],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(col(&conn, "distillable_count"), 4);

        // Simulate a completed distill (records distillable_count, not total).
        conn.execute(
            "UPDATE threads SET knowledge_extracted = 1, knowledge_msg_count = distillable_count WHERE id = ?1",
            [tid],
        )
        .unwrap();

        // Append a TOOL row: distillable count unchanged, distilled knowledge NOT invalidated.
        let mut with_tool = base.to_vec();
        with_tool.push(("tool", "ls output"));
        upsert_thread(&mut conn, sid, &mk(&with_tool)).unwrap();
        assert_eq!(
            col(&conn, "distillable_count"),
            4,
            "tool rows aren't distillable"
        );
        assert_eq!(col(&conn, "message_count"), 5);
        assert_eq!(
            col(&conn, "knowledge_extracted"),
            1,
            "a tool-only append must NOT re-trigger distillation"
        );

        // Append a USER row: distillable +1, knowledge invalidated for re-distill.
        let mut with_user = with_tool.clone();
        with_user.push(("user", "e"));
        upsert_thread(&mut conn, sid, &mk(&with_user)).unwrap();
        assert_eq!(col(&conn, "distillable_count"), 5);
        assert_eq!(
            col(&conn, "knowledge_extracted"),
            0,
            "a new user/assistant message DOES re-trigger distillation"
        );

        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn canonical_project_normalizes_and_groups() {
        assert_eq!(canonical_project(""), None);
        assert_eq!(canonical_project("   "), None);
        // Nonexistent path: trailing slash trimmed, otherwise unchanged.
        assert_eq!(
            canonical_project("/no/such/proj/"),
            Some("/no/such/proj".to_string())
        );
        assert_eq!(
            canonical_project("/no/such/proj"),
            Some("/no/such/proj".to_string())
        );

        // A real git repo: a subdir resolves to the SAME key as the repo root.
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("cp_{}_{n}", std::process::id()));
        let deep = root.join("a/b");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let k_root = canonical_project(root.to_str().unwrap());
        let k_deep = canonical_project(deep.to_str().unwrap());
        assert!(k_root.is_some());
        assert_eq!(k_root, k_deep, "subdir groups with the repo root");
        let _ = std::fs::remove_dir_all(&root);
    }

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
