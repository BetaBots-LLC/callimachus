//! Source indexers. Each agent (Claude Code, Codex, Cursor) parses its on-disk
//! history into the common `ParsedThread` shape, which `upsert_thread` writes into
//! the canonical store. Indexing a thread is idempotent: re-running replaces its
//! messages, so a changed file can be fully re-parsed safely.

pub mod claude;
pub mod cline;
pub mod codex;
pub mod common;
pub mod continue_cli;
pub mod copilot;
pub mod copilot_cli;
pub mod cursor;
pub mod file_edits;
pub mod gemini;
pub mod goose;
pub mod kilo;
pub mod opencode;
pub mod parse_health;
pub mod qwen;
pub mod roo;
pub mod usage_backfill;
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

impl MsgUsage {
    /// The stored columns' view of this usage, for comparing a re-parse with what a row holds.
    fn columns(&self) -> (Option<&str>, [i64; 4]) {
        (
            Some(self.model.as_str()),
            [self.input, self.output, self.cache_write, self.cache_read],
        )
    }
}

impl ParsedThread {
    /// Record one model call's usage on the message at `at` (the call's first message), or,
    /// when the call produced no message of its own (`None`: a tool-only OpenCode turn, a
    /// Codex response that was all reasoning, a Qwen record carrying only tokens), on the
    /// last message so far. A call landing where usage already sits is added to it, so no
    /// spend is lost to the one-usage-per-row column layout; it keeps the first model.
    /// Usage before any message is dropped: there is no row to carry it.
    pub fn attach_usage(&mut self, at: Option<usize>, u: MsgUsage) {
        let Some(idx) = at
            .filter(|i| *i < self.messages.len())
            .or_else(|| self.messages.len().checked_sub(1))
        else {
            return;
        };
        match self.usage.last_mut() {
            Some((last, prev)) if *last == idx => {
                prev.input += u.input;
                prev.output += u.output;
                prev.cache_write += u.cache_write;
                prev.cache_read += u.cache_read;
            }
            _ => self.usage.push((idx, u)),
        }
    }
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

impl IndexReport {
    /// Count one file or thread a scan processed: `Some(n)` was processed and `n` message
    /// rows were inserted (often 0: a title-only append, or an unchanged re-parse after a
    /// stamp change), `None` was unchanged or skipped on purpose, and an error is counted,
    /// not raised, so one bad file never aborts the rest of the pass.
    pub fn record(&mut self, result: Result<Option<usize>>) {
        match result {
            Ok(Some(n)) => {
                self.threads_indexed += 1;
                self.messages_indexed += n;
            }
            Ok(None) => self.threads_skipped += 1,
            Err(_) => self.errors += 1,
        }
    }

    /// [`record`](Self::record) for one file (or session) `what`, logging a failure with
    /// it: the count alone made `cal index` exit 1 saying the lines above name the failure
    /// while nothing did, and left a file that fails on every pass unfindable.
    pub fn record_at(&mut self, what: &std::path::Path, result: Result<Option<usize>>) {
        if let Err(e) = &result {
            app_log!("[index] {}: {e:#}", what.display());
        }
        self.record(result);
    }

    /// Fold another pass's tally into this one.
    fn add(&mut self, other: &IndexReport) {
        self.threads_indexed += other.threads_indexed;
        self.threads_skipped += other.threads_skipped;
        self.messages_indexed += other.messages_indexed;
        self.errors += other.errors;
    }
}

/// A full scan of one source. `tick` is called once per thread (or file) processed, so a
/// caller can drive a progress bar.
pub type Scan = fn(&mut Connection, &mut dyn FnMut()) -> Result<IndexReport>;

/// One indexed source.
pub struct Source {
    /// The `sources.kind` row (seeded by migration) and the frontend's `SourceKind`.
    pub kind: &'static str,
    pub scan: Scan,
    /// The indexer stores tool results through `claude::cap_tool_text`. Only these sources'
    /// tool rows may be compacted by cleanup: compacting any other source's rows would just
    /// be undone by its next re-index, which sees a mismatch and stores the full text again.
    pub caps_tool_output: bool,
}

/// Every source, in full-scan order. This is the one list to extend when adding a source:
/// the full reindex, the per-source reindex command, the watcher and tool-output cleanup all
/// read it, and tests check it against the `sources` table and the telemetry allowlist.
pub const SOURCES: &[Source] = &[
    Source {
        kind: claude::KIND,
        scan: claude::scan,
        caps_tool_output: true,
    },
    Source {
        kind: codex::KIND,
        scan: codex::scan,
        caps_tool_output: true,
    },
    Source {
        kind: cursor::KIND,
        scan: cursor::scan,
        caps_tool_output: true,
    },
    Source {
        kind: gemini::KIND,
        scan: gemini::scan,
        caps_tool_output: true,
    },
    Source {
        kind: qwen::KIND,
        scan: qwen::scan,
        caps_tool_output: true,
    },
    Source {
        kind: goose::KIND,
        scan: goose::scan,
        caps_tool_output: false,
    },
    Source {
        kind: opencode::KIND,
        scan: opencode::scan,
        caps_tool_output: false,
    },
    Source {
        kind: continue_cli::KIND,
        scan: continue_cli::scan,
        caps_tool_output: true,
    },
    Source {
        kind: cline::KIND,
        scan: cline::scan,
        caps_tool_output: true,
    },
    Source {
        kind: roo::KIND,
        scan: roo::scan,
        caps_tool_output: true,
    },
    Source {
        kind: kilo::KIND,
        scan: kilo::scan,
        caps_tool_output: true,
    },
    Source {
        kind: copilot::KIND,
        scan: copilot::scan,
        caps_tool_output: false,
    },
    Source {
        kind: copilot_cli::KIND,
        scan: copilot_cli::scan,
        caps_tool_output: true,
    },
];

/// The full scan for a source kind, or None for a kind no indexer handles (`in_app`, a
/// typo). Callers that take a kind from outside (a command argument, a watch route) use
/// this instead of matching kinds by hand, so a new source can't be missed.
pub fn scanner_for(kind: &str) -> Option<Scan> {
    SOURCES.iter().find(|s| s.kind == kind).map(|s| s.scan)
}

/// Run one scan of `kind` (`run`: a full scan, the watcher's changed paths) with what it
/// parsed counted, then judged and logged by [`parse_health::after_pass`]. `full`: the scan
/// covered every file of the source (a reindex, `cal index`, the launch catch-up), not a
/// watcher batch. A scan that fails records nothing: a retry parses the same files again.
pub fn scan_counted(
    conn: &mut Connection,
    kind: &str,
    full: bool,
    run: impl FnOnce(&mut Connection) -> Result<IndexReport>,
) -> Result<IndexReport> {
    let (result, stats) = common::collect_parse_stats(|| run(conn));
    if result.is_ok() {
        parse_health::after_pass(conn, kind, &stats, full);
    }
    result
}

/// Kinds whose tool rows are stored capped (see [`Source::caps_tool_output`]).
pub fn capped_tool_sources() -> impl Iterator<Item = &'static str> {
    SOURCES
        .iter()
        .filter(|s| s.caps_tool_output)
        .map(|s| s.kind)
}

/// Index every source, one at a time, summing the reports. Calls `on_progress(threads_seen,
/// current_source)` thread-granularly (throttled) so a background reindex can drive a live
/// progress bar even while one big source churns. Pass `|_, _| {}` to ignore progress.
pub fn scan_all_with_progress(
    conn: &mut Connection,
    mut on_progress: impl FnMut(usize, &str),
) -> Result<IndexReport> {
    // Progress is THREAD-granular, not source-granular: each source ticks once per thread
    // it processes (indexed OR skipped), so the bar keeps moving even while one big source
    // (usually Claude Code) churns through thousands of files. `seen` is the running count
    // across all sources; the total isn't known up front, so the UI renders it indeterminate.
    let mut total = IndexReport::default();
    let mut seen = 0usize;
    for source in SOURCES {
        let name = source.kind;
        on_progress(seen, name);
        // Isolate per-source failures: a single source erroring (a missing/locked
        // agent DB, an OS-specific quirk, a malformed store) must NOT abort the whole
        // index and zero out every other source. Log it, count it, keep going — the
        // same resilience each scan already applies per-file.
        let mut tick = || {
            seen += 1;
            // Emit the first 50 per-thread (so movement is obviously live), then every 10.
            if seen <= 50 || seen.is_multiple_of(10) {
                on_progress(seen, name);
            }
        };
        match scan_counted(conn, name, true, |conn| (source.scan)(conn, &mut tick)) {
            Ok(r) => total.add(&r),
            Err(e) => {
                // Not eprintln!, which panics on a closed stderr (`cal index 2>&1 | head`).
                app_log!("[index] source '{name}' failed: {e:#}");
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
///
/// - A git worktree (where `.git` is a file pointing into `<repo>/.git/worktrees/<name>`)
///   keys on its MAIN repo, so worktree sessions share the repo's memory instead of each
///   getting an empty project under a path that is later deleted.
/// - The walk stops at the home directory: a `~/.git` (dotfiles kept as a plain repo) is
///   only the project when the path IS the home dir, so a non-repo directory under home
///   keys on itself rather than on the whole home directory.
/// - Windows `\\?\` verbatim prefixes from `canonicalize` are stripped, so keys match
///   paths that were normalized without resolving.
pub fn canonical_project(path: &str) -> Option<String> {
    // Resolved once per process: indexing calls this for every thread.
    static HOME: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    let home = HOME.get_or_init(|| {
        dirs::home_dir().map(|h| strip_verbatim_prefix(&std::fs::canonicalize(&h).unwrap_or(h)))
    });
    canonical_project_with_home(path, home.as_deref())
}

/// `canonical_project` with the home directory passed in (tests use a fake one).
fn canonical_project_with_home(path: &str, home: Option<&Path>) -> Option<String> {
    use std::path::PathBuf;
    let p = path.trim();
    if p.is_empty() {
        return None;
    }
    // Expand a leading `~`.
    let expanded: PathBuf = if p == "~" {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from(p))
    } else {
        match p.strip_prefix("~/") {
            Some(rest) => dirs::home_dir()
                .map(|h| h.join(rest))
                .unwrap_or_else(|| PathBuf::from(p)),
            None => PathBuf::from(p),
        }
    };
    // If it exists: resolve symlinks + absolutize, then walk up to the git root.
    if let Ok(abs) = std::fs::canonicalize(&expanded) {
        let abs = strip_verbatim_prefix(&abs);
        let root = git_root(&abs, home).unwrap_or(abs);
        return Some(path_key(&root));
    }
    // Doesn't exist on this machine: light normalization only.
    Some(path_key(&expanded))
}

/// The repository root for `start` (walking up), resolving worktrees to their main repo.
/// Never walks above `home` unless `start` is `home` itself.
fn git_root(start: &Path, home: Option<&Path>) -> Option<std::path::PathBuf> {
    let mut dir = start;
    loop {
        if home == Some(dir) && dir != start {
            return None; // reached home from below: home's own repo doesn't count
        }
        let dot_git = dir.join(".git");
        if dot_git.is_dir() {
            return Some(dir.to_path_buf());
        }
        if dot_git.is_file() {
            return Some(worktree_main_repo(dir, &dot_git).unwrap_or_else(|| dir.to_path_buf()));
        }
        dir = dir.parent()?;
    }
}

/// For a linked worktree (`.git` file with `gitdir: <common>/worktrees/<name>`), the main
/// repo's working directory. None for anything else, e.g. a submodule (`gitdir:
/// ../.git/modules/x`), which is its own project.
fn worktree_main_repo(dir: &Path, dot_git_file: &Path) -> Option<std::path::PathBuf> {
    let content = std::fs::read_to_string(dot_git_file).ok()?;
    let gitdir = content
        .lines()
        .find_map(|l| l.trim().strip_prefix("gitdir:"))?
        .trim();
    let gitdir = if Path::new(gitdir).is_absolute() {
        std::path::PathBuf::from(gitdir)
    } else {
        dir.join(gitdir)
    };
    // `<common>/worktrees/<name>`; `commondir` (when present) names <common> explicitly.
    let common = match std::fs::read_to_string(gitdir.join("commondir")) {
        Ok(c) => {
            let c = c.trim();
            if Path::new(c).is_absolute() {
                std::path::PathBuf::from(c)
            } else {
                gitdir.join(c)
            }
        }
        Err(_) => {
            let parent = gitdir.parent()?;
            if parent.file_name()? != "worktrees" {
                return None;
            }
            parent.parent()?.to_path_buf()
        }
    };
    let common = std::fs::canonicalize(&common).ok()?;
    let common = strip_verbatim_prefix(&common);
    if common.file_name()? == ".git" {
        // The usual layout: the main working tree is the parent of its .git dir.
        common.parent().map(Path::to_path_buf)
    } else {
        // A bare repo (`repo.git`) has no main working tree; key on the repo dir.
        Some(common)
    }
}

/// Strip a Windows verbatim prefix: `\\?\C:\x` -> `C:\x`, `\\?\UNC\srv\share` ->
/// `\\srv\share`. A no-op for every other path (and on Unix in practice).
fn strip_verbatim_prefix(p: &Path) -> std::path::PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return std::path::PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return std::path::PathBuf::from(rest);
    }
    p.to_path_buf()
}

/// A path as a key string: trailing separators trimmed, but a root (`/`, `C:\`) kept whole.
fn path_key(p: &Path) -> String {
    let s = p.to_string_lossy();
    let trimmed = s.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() || trimmed.ends_with(':') {
        s.into_owned()
    } else {
        trimmed.to_string()
    }
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

/// The threads whose per-thread rows (`file_edits` above all) belong to the session
/// `thread_id`: the thread itself, plus, for a Claude Code session, its subagent transcripts.
/// Claude Code writes a session's subagents beside it (`<slug>/<session>.jsonl` and
/// `<slug>/<session>/subagents/agent-*.jsonl`), each indexed as its own thread with
/// `is_subagent = 1`; in workflow-heavy repos they make most of a session's edits, so a
/// reader joining commits to sessions to files must include them. Other sources record no
/// parent for a subagent. A range probe of the `(source_id, external_id)` unique index.
pub fn session_thread_ids(conn: &Connection, thread_id: i64) -> Result<Vec<i64>> {
    let mut out = vec![thread_id];
    let Some((source_id, external_id)) = conn
        .query_row(
            "SELECT source_id, external_id FROM threads WHERE id = ?1",
            [thread_id],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?
    else {
        return Ok(out);
    };
    let Some((lo, hi)) = subagent_id_range(&external_id) else {
        return Ok(out);
    };
    let mut stmt = conn.prepare_cached(SUBAGENTS_OF_SQL)?;
    for id in stmt.query_map(params![source_id, lo, hi], |r| r.get::<_, i64>(0))? {
        out.push(id?);
    }
    Ok(out)
}

/// The `external_id` range `[lo, hi)` holding the subagent transcripts of the session whose
/// `external_id` this is (see [`session_thread_ids`]); None for a thread that has none by
/// construction (not a `.jsonl` session, or itself a subagent transcript).
pub(crate) fn subagent_id_range(external_id: &str) -> Option<(String, String)> {
    let stem = external_id
        .strip_suffix(".jsonl")
        .filter(|s| !s.contains("/subagents/"))?;
    // `/` + 1 is `0`: every id under `<stem>/subagents/` and nothing after it.
    Some((format!("{stem}/subagents/"), format!("{stem}/subagents0")))
}

/// The subagent transcripts of one session (see [`session_thread_ids`]).
pub(crate) const SUBAGENTS_OF_SQL: &str = "SELECT id FROM threads
     WHERE source_id = ?1 AND external_id >= ?2 AND external_id < ?3 AND is_subagent = 1
     ORDER BY id";

/// The session a subagent transcript belongs to (see [`session_thread_ids`]): its parent's
/// thread id, or `thread_id` itself for any other thread or when the parent is not indexed.
pub fn parent_session_id(conn: &Connection, thread_id: i64) -> Result<i64> {
    let row: Option<(i64, String)> = conn
        .query_row(
            "SELECT source_id, external_id FROM threads WHERE id = ?1 AND is_subagent = 1",
            [thread_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((source_id, external_id)) = row else {
        return Ok(thread_id);
    };
    let Some((stem, _)) = external_id.split_once("/subagents/") else {
        return Ok(thread_id);
    };
    Ok(conn
        .query_row(
            "SELECT id FROM threads WHERE source_id = ?1 AND external_id = ?2",
            params![source_id, format!("{stem}.jsonl")],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(thread_id))
}

/// Every subagent transcript's parent session at once, as `subagent -> parent` thread ids
/// ([`parent_session_id`] answers for one): what cost reporting folds a subagent's spend
/// into and issue mining files a subagent's errors under. A subagent whose parent is not
/// indexed is left out, so it stays on its own.
pub(crate) fn subagent_parents(conn: &Connection) -> Result<std::collections::HashMap<i64, i64>> {
    let mut stmt = conn.prepare_cached(SUBAGENT_PARENTS_SQL)?;
    let pairs = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(pairs)
}

/// Each Claude Code subagent transcript's parent: `<slug>/<session>.jsonl` for
/// `<slug>/<session>/subagents/.../agent-<id>.jsonl`, matched on the unique
/// (source, external id) index.
pub(crate) const SUBAGENT_PARENTS_SQL: &str = "SELECT c.id, p.id FROM threads c
     JOIN threads p ON p.source_id = c.source_id
      AND p.external_id = substr(c.external_id, 1, instr(c.external_id, '/subagents/') - 1) || '.jsonl'
     WHERE c.is_subagent = 1 AND instr(c.external_id, '/subagents/') > 0";

/// Resolve the numeric source id for a source kind (rows are seeded by migration).
pub fn source_id(conn: &Connection, kind: &str) -> Result<i64> {
    conn.query_row("SELECT id FROM sources WHERE kind = ?1", [kind], |r| {
        r.get(0)
    })
    .with_context(|| format!("unknown source kind {kind}"))
}

/// Insert or fully replace a thread and all its messages. Empty threads are dropped.
/// Returns how many message rows were inserted: the whole thread on a first index or a
/// full replace, only the new tail on an append, 0 when nothing changed or the thread was
/// skipped. Legacy rows rewritten in place by the tool-output cap upgrade (see
/// `is_recapped`) are not counted. Callers report this as "messages indexed", so a 1-line
/// append to a 17k-message session reads as 1, not 17,114.
pub fn upsert_thread(
    conn: &mut Connection,
    source_id: i64,
    thread: &ParsedThread,
) -> Result<usize> {
    upsert_thread_at(conn, source_id, thread, &[])
}

/// [`upsert_thread`] for a source that records a working directory per line (Claude Code):
/// `cwds` holds `(first message index, cwd)` at each change, in order, and a tool call's
/// relative paths resolve against the cwd it ran in rather than the thread's last one.
pub fn upsert_thread_at(
    conn: &mut Connection,
    source_id: i64,
    thread: &ParsedThread,
    cwds: &[(usize, String)],
) -> Result<usize> {
    if thread.messages.is_empty() {
        return Ok(0);
    }
    // The user deleted this thread (cleanup::delete_threads tombstones it): don't re-import
    // it. Claude, Codex, Cursor and Copilot also check before parsing; this covers every
    // other indexer. A fast path only: the authoritative check is repeated inside the
    // write transaction below.
    if crate::cleanup::is_tombstoned(conn, source_id, &thread.external_id)? {
        return Ok(0);
    }
    let t0 = std::time::Instant::now();
    let now = chrono::Utc::now().timestamp();
    // UTF-8 byte size of all message text (String::len is byte length), stored so the
    // cleanup list reads a column instead of SUM(LENGTH(text)) across every message.
    let bytes: i64 = thread.messages.iter().map(|m| m.text.len() as i64).sum();
    // Count of DISTILLABLE messages (what the LLM actually sees); distill staleness keys off
    // this, not total message_count, so appended tool/system rows don't re-trigger a distill.
    let distillable = distillable_count(&thread.messages);
    let anchors = edit_anchors(thread, cwds);
    let Some(written) = in_write_tx(conn, |tx| {
        write_thread(
            tx,
            source_id,
            thread,
            now,
            bytes,
            distillable,
            &anchors,
            cwds,
        )
    })?
    else {
        return Ok(0);
    };
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
    Ok(written)
}

/// Append `thread.messages` (ONLY the new tail, its `usage` keyed by index into that tail)
/// to an already stored thread whose first `start_seq` messages are known to be unchanged,
/// and refresh the thread's metadata from `thread`. For sources that prove a pure append
/// themselves (Claude Code's byte cursor: fingerprint plus stored count): unlike
/// [`upsert_thread`] it never reads the stored message texts, so its cost tracks the tail,
/// not the thread. On a 17k-message session that prefix read was most of a 30-40 ms append.
///
/// Returns the rows inserted, or None when the stored thread is not the one the caller
/// expects (missing, tombstoned, or its message count moved because another writer got
/// there first). Nothing is written then, and the caller should fall back to a full parse.
pub fn append_messages(
    conn: &mut Connection,
    source_id: i64,
    thread: &ParsedThread,
    start_seq: usize,
) -> Result<Option<usize>> {
    append_messages_at(conn, source_id, thread, start_seq, &[])
}

/// [`append_messages`] with the tail's per-line working directories (see
/// [`upsert_thread_at`]; indices into the tail).
pub fn append_messages_at(
    conn: &mut Connection,
    source_id: i64,
    thread: &ParsedThread,
    start_seq: usize,
    cwds: &[(usize, String)],
) -> Result<Option<usize>> {
    let now = chrono::Utc::now().timestamp();
    let anchors = edit_anchors(thread, cwds);
    in_write_tx(conn, |tx| {
        write_append(tx, source_id, thread, start_seq, now, &anchors, cwds)
    })
}

/// Run `write` under the write lock and commit only when it returns Some. On its own that
/// is one IMMEDIATE transaction: it takes the lock up front, so the busy handler waits for
/// another writer instead of failing a read-to-write upgrade. Inside a caller's open
/// transaction (claude::scan batches a full scan into a few commits) it is a savepoint, so
/// a failing thread rolls back only its own rows. Either way, dropping the scope without
/// committing rolls it back.
fn in_write_tx<T>(
    conn: &mut Connection,
    write: impl FnOnce(&Connection) -> Result<Option<T>>,
) -> Result<Option<T>> {
    if conn.is_autocommit() {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let out = write(&tx)?;
        if out.is_some() {
            tx.commit()?;
        }
        Ok(out)
    } else {
        let sp = conn.savepoint()?;
        let out = write(&sp)?;
        if out.is_some() {
            sp.commit()?;
        }
        Ok(out)
    }
}

/// Messages the distiller reads (user and assistant turns, tool calls included).
fn distillable_count(messages: &[ParsedMessage]) -> i64 {
    messages
        .iter()
        .filter(|m| m.role == "user" || m.role == "assistant")
        .count() as i64
}

/// The body of [`upsert_thread`], run under the write lock `tx` holds. Returns the rows
/// inserted, or None (and writes nothing) when the thread was tombstoned.
#[allow(clippy::too_many_arguments)] // all distinct; [`upsert_thread_at`] is the one caller
fn write_thread(
    tx: &Connection,
    source_id: i64,
    thread: &ParsedThread,
    now: i64,
    bytes: i64,
    distillable: i64,
    anchors: &file_edits::Anchors,
    cwds: &[(usize, String)],
) -> Result<Option<usize>> {
    // Re-check under the write lock: the watcher indexes on its own connection, so a delete
    // that committed after the early check would otherwise be undone by the INSERT below
    // (the thread comes back next to its own tombstone).
    if crate::cleanup::is_tombstoned(tx, source_id, &thread.external_id)? {
        return Ok(None);
    }

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
    // point-sampling it can never keep a stale in-place edit: any mismatch or shrink falls
    // back to a correct full replace (also covers rewritten / DB-backed sources). When
    // n == existing_count and the prefix matches, the tail is empty: a clean no-op.
    // (Sources that can prove an append without this read use `append_messages`.)
    let existing_count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM messages WHERE thread_id = ?1",
        [thread_id],
        |r| r.get(0),
    )?;
    let n = thread.messages.len() as i64;
    // Stored tool rows that differ from the new parse only by the index-time tool-output
    // cap (see `recapped`), rewritten in place on an append.
    let mut recapped: Vec<i64> = Vec::new();
    // Stored prefix rows whose token usage differs from the new parse, rewritten in place on
    // an append. Usage often lands after its message was stored: Codex writes the turn's
    // `token_count` event after the response items, Gemini re-records a message once its
    // tokens arrive. And a thread indexed before its source captured usage gets it here on
    // the next re-parse instead of needing a full replace. The usage columns sit after
    // `text` in the row, which this scan reads anyway, and no trigger watches them.
    let mut reusage: Vec<i64> = Vec::new();
    let prefix_usage: std::collections::HashMap<usize, &MsgUsage> = thread
        .usage
        .iter()
        .filter(|(i, _)| (*i as i64) < existing_count)
        .map(|(i, u)| (*i, u))
        .collect();
    let incremental = existing_count > 0 && n >= existing_count && {
        let mut stmt = tx.prepare(
            "SELECT seq, text, model, input_tokens, output_tokens, cache_write_tokens,
                    cache_read_tokens
             FROM messages WHERE thread_id = ?1 ORDER BY seq",
        )?;
        let mut matched = 0i64;
        let mut ok = true;
        let rows = stmt.query_map([thread_id], |r| {
            let tokens = |i| r.get::<_, Option<i64>>(i).map(|v| v.unwrap_or(0));
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                [tokens(3)?, tokens(4)?, tokens(5)?, tokens(6)?],
            ))
        })?;
        for row in rows {
            let (seq, text, model, tokens) = row?;
            let Some((i, new)) = usize::try_from(seq)
                .ok()
                .and_then(|i| thread.messages.get(i).map(|m| (i, m)))
            else {
                ok = false;
                break;
            };
            if new.text != text {
                if !is_recapped(new, &text) {
                    ok = false;
                    break;
                }
                recapped.push(seq);
            }
            let parsed = prefix_usage.get(&i).map(|u| u.columns());
            let stored = model.as_deref().map(|m| (Some(m), tokens));
            if parsed != stored {
                reusage.push(seq);
            }
            matched += 1;
        }
        ok && matched == existing_count
    };

    if incremental {
        // Tool output indexed before the cap existed: store the capped text a fresh index
        // would, without the full replace below (which churns every message id, re-queues
        // the thread's prose for embedding and drops fact provenance). FTS follows through
        // the update trigger.
        let mut upd =
            tx.prepare("UPDATE messages SET text = ?3 WHERE thread_id = ?1 AND seq = ?2")?;
        for seq in &recapped {
            upd.execute(params![thread_id, seq, thread.messages[*seq as usize].text])?;
        }
        let mut upd = tx.prepare(
            "UPDATE messages SET model = ?3, input_tokens = ?4, output_tokens = ?5,
                    cache_write_tokens = ?6, cache_read_tokens = ?7
             WHERE thread_id = ?1 AND seq = ?2",
        )?;
        for seq in &reusage {
            let u = prefix_usage.get(&(*seq as usize));
            upd.execute(params![
                thread_id,
                seq,
                u.map(|u| u.model.as_str()),
                u.map(|u| u.input),
                u.map(|u| u.output),
                u.map(|u| u.cache_write),
                u.map(|u| u.cache_read),
            ])?;
        }
    } else {
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
    let tail = &thread.messages[start_seq..];
    let usage: std::collections::HashMap<usize, &MsgUsage> = thread
        .usage
        .iter()
        .filter(|(i, _)| *i >= start_seq)
        .map(|(i, u)| (*i - start_seq, u))
        .collect();
    let inserted = insert_messages(tx, thread_id, start_seq, tail, &usage)?;
    derive_todos(
        tx,
        thread_id,
        &inserted,
        incremental,
        thread.is_subagent,
        now,
    )?;
    derive_file_mentions(tx, thread_id, &inserted, incremental)?;
    derive_file_edits(tx, thread_id, anchors, cwds, 0, &inserted, incremental)?;
    if !incremental && existing_count > 0 {
        // The replace deleted every message the thread's kept facts pointed at (0011's ON
        // DELETE SET NULL); point them at the rows that took their place.
        crate::knowledge::rebind_fact_sources(tx, thread_id)?;
    }
    // A full replace of an existing thread (prefix mismatch / shrink) means the content
    // changed even if the count didn't (e.g. an in-place edit).
    invalidate_distilled(
        tx,
        thread_id,
        distillable,
        existing_count > 0 && !incremental,
    )?;
    Ok(Some(inserted.len()))
}

/// The body of [`append_messages`], run under the write lock `tx` holds.
fn write_append(
    tx: &Connection,
    source_id: i64,
    thread: &ParsedThread,
    start_seq: usize,
    now: i64,
    anchors: &file_edits::Anchors,
    cwds: &[(usize, String)],
) -> Result<Option<usize>> {
    if crate::cleanup::is_tombstoned(tx, source_id, &thread.external_id)? {
        return Ok(None);
    }
    let Some((thread_id, stored_count)) = tx
        .query_row(
            "SELECT id, message_count FROM threads WHERE source_id = ?1 AND external_id = ?2",
            params![source_id, thread.external_id],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()?
    else {
        return Ok(None);
    };
    // The caller checked the count before parsing, but the watcher and a full scan index on
    // separate connections: re-check under the lock. The rows must add up too (MAX(seq) is
    // one probe of the (thread_id, seq) unique index), or the tail would land at the wrong
    // seq; either mismatch means a full parse is the safe answer.
    let next_seq: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq) + 1, 0) FROM messages WHERE thread_id = ?1",
        [thread_id],
        |r| r.get(0),
    )?;
    if stored_count != start_seq as i64 || next_seq != stored_count {
        return Ok(None);
    }

    let tail_bytes: i64 = thread.messages.iter().map(|m| m.text.len() as i64).sum();
    let project_key = thread.project_path.as_deref().and_then(canonical_project);
    tx.execute(
        "UPDATE threads SET title = ?2, project_path = ?3, git_branch = ?4, created_at = ?5,
            updated_at = ?6, message_count = message_count + ?7, last_indexed_at = ?8,
            bytes = bytes + ?9, project_key = ?10, distillable_count = distillable_count + ?11
         WHERE id = ?1",
        params![
            thread_id,
            thread.title,
            thread.project_path,
            thread.git_branch,
            thread.created_at,
            thread.updated_at,
            thread.messages.len() as i64,
            now,
            tail_bytes,
            project_key,
            distillable_count(&thread.messages),
        ],
    )?;
    let distillable: i64 = tx.query_row(
        "SELECT distillable_count FROM threads WHERE id = ?1",
        [thread_id],
        |r| r.get(0),
    )?;
    let usage: std::collections::HashMap<usize, &MsgUsage> =
        thread.usage.iter().map(|(i, u)| (*i, u)).collect();
    let inserted = insert_messages(tx, thread_id, start_seq, &thread.messages, &usage)?;
    derive_todos(tx, thread_id, &inserted, true, thread.is_subagent, now)?;
    derive_file_mentions(tx, thread_id, &inserted, true)?;
    derive_file_edits(tx, thread_id, anchors, cwds, start_seq, &inserted, true)?;
    invalidate_distilled(tx, thread_id, distillable, false)?;
    Ok(Some(inserted.len()))
}

/// One message row just inserted.
struct Inserted<'a> {
    id: i64,
    seq: i64,
    role: &'a str,
    text: &'a str,
    tool_name: Option<&'a str>,
    ts: Option<i64>,
}

/// Insert `messages` as seqs `first_seq..`, with `usage` keyed by index into `messages`.
fn insert_messages<'a>(
    tx: &Connection,
    thread_id: i64,
    first_seq: usize,
    messages: &'a [ParsedMessage],
    usage: &std::collections::HashMap<usize, &MsgUsage>,
) -> Result<Vec<Inserted<'a>>> {
    let mut inserted = Vec::with_capacity(messages.len());
    let mut stmt = tx.prepare(
        "INSERT INTO messages
            (thread_id, seq, role, text, tool_name, ts,
             model, input_tokens, output_tokens, cache_write_tokens, cache_read_tokens)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )?;
    for (i, m) in messages.iter().enumerate() {
        let u = usage.get(&i);
        stmt.execute(params![
            thread_id,
            (first_seq + i) as i64,
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
        inserted.push(Inserted {
            id: tx.last_insert_rowid(),
            seq: (first_seq + i) as i64,
            role: m.role.as_str(),
            text: m.text.as_str(),
            tool_name: m.tool_name.as_deref(),
            ts: m.ts,
        });
    }
    Ok(inserted)
}

/// Free heuristic knowledge tier: re-derive this thread's TODO facts every index
/// (delete + rescan) so they never go stale. Only user/assistant text is scanned.
/// The LLM-distilled facts (extractor='llm') are left untouched here. Always on: it needs
/// no LLM and nothing leaves the machine, so it no longer waits for the distillation
/// opt-in (which left TODOs, the hook and Projects empty for most users). Subagent threads
/// are skipped: their "TODO"s are the parent agent's task list echoed back, not the user's.
fn derive_todos(
    tx: &Connection,
    thread_id: i64,
    inserted: &[Inserted],
    appended: bool,
    is_subagent: bool,
    now: i64,
) -> Result<()> {
    if is_subagent {
        return Ok(());
    }
    // An append whose tail has no user/assistant text has nothing to scan: skip the seeds
    // (a full replace still takes the open rows, as the thread may have lost its messages).
    let scans = inserted
        .iter()
        .any(|m| m.role == "user" || m.role == "assistant");
    if appended && !scans {
        return Ok(());
    }
    // Keep CURATED todos (done / dismissed / pinned / edited) so closing one survives a
    // re-index; a full replace re-derives only the open, untouched ones (each keeping the uid
    // it had, so a full re-index does not mint new facts), an append scans only the new tail
    // (where a deferred-work list still deletes an earlier list's open follow-ups, unless the
    // user reworded one of the thread's TODOs: `TodoDeriver` then leaves follow-ups alone).
    let mut derive = crate::knowledge::TodoDeriver::start(tx, thread_id, !appended, now)?;
    // Every message is read: a later deferred-work list replaces the earlier ones.
    for m in inserted {
        if m.role != "user" && m.role != "assistant" {
            continue;
        }
        derive.message(m.id, m.role == "assistant", m.text, m.ts, m.seq)?;
    }
    Ok(())
}

/// Code-aware search: re-derive this thread's file-path mentions (delete + rescan,
/// every index). Only user/assistant text: tool RESULTS (ls output) would be noise.
/// Full replace re-derives file mentions; an append keeps existing + adds the tail's
/// (INSERT OR IGNORE dedups at the unique constraint).
fn derive_file_mentions(
    tx: &Connection,
    thread_id: i64,
    inserted: &[Inserted],
    appended: bool,
) -> Result<()> {
    if !appended {
        tx.execute(
            "DELETE FROM file_mentions WHERE thread_id = ?1",
            [thread_id],
        )?;
    }
    const MAX_PATHS_PER_THREAD: usize = 200;
    let mut pstmt =
        tx.prepare("INSERT OR IGNORE INTO file_mentions (thread_id, path) VALUES (?1, ?2)")?;
    let mut seen = std::collections::HashSet::new();
    // On an append the existing mentions are kept, so seed `seen` with them, both to
    // dedup and so the per-thread cap counts the thread total, not just the new tail.
    if appended {
        let mut existing = tx.prepare("SELECT path FROM file_mentions WHERE thread_id = ?1")?;
        for p in existing
            .query_map([thread_id], |r| r.get::<_, String>(0))?
            .flatten()
        {
            seen.insert(p.to_ascii_lowercase());
        }
    }
    'paths: for m in inserted {
        if m.role != "user" && m.role != "assistant" {
            continue;
        }
        for path in extract_paths(m.text) {
            if seen.len() >= MAX_PATHS_PER_THREAD {
                break 'paths;
            }
            if seen.insert(path.to_ascii_lowercase()) {
                pstmt.execute(params![thread_id, path])?;
            }
        }
    }
    Ok(())
}

/// The `file_edits` anchors for `thread`'s tool calls: the thread's cwd and every cwd a call
/// ran in (`cwds`, or one the call names). Resolved before the write transaction opens, since
/// each probes the disk for its checkout (a cwd on a sleeping volume must not stall a writer
/// holding the lock); a thread without tool calls probes nothing.
fn edit_anchors(thread: &ParsedThread, cwds: &[(usize, String)]) -> file_edits::Anchors {
    let mut call_cwds: Vec<String> = Vec::new();
    let mut any = false;
    for (i, m) in thread.messages.iter().enumerate() {
        let Some(name) = m.tool_name.as_deref() else {
            continue;
        };
        any = true;
        let named = file_edits::call_cwd(name, &m.text);
        for c in named.as_deref().into_iter().chain(cwd_at(cwds, i)) {
            if !call_cwds.iter().any(|k| k == c) {
                call_cwds.push(c.to_string());
            }
        }
    }
    if !any {
        return file_edits::Anchors::default();
    }
    file_edits::Anchors::resolve(
        thread.project_path.as_deref(),
        call_cwds.iter().map(String::as_str),
    )
}

/// The cwd message `i` ran in: the last change at or before it, else the first one recorded.
fn cwd_at(cwds: &[(usize, String)], i: usize) -> Option<&str> {
    let after = cwds.partition_point(|(k, _)| *k <= i);
    cwds.get(after.saturating_sub(1)).map(|(_, c)| c.as_str())
}

/// The `file_edits` rows for the tool calls just inserted (see [`file_edits`]): a full
/// replace re-derives the thread's rows, an append adds the tail's. Paths are anchored on
/// the checkout each call ran in (`anchors`, resolved by [`edit_anchors`]); `base` is the
/// seq of `cwds`' index 0.
fn derive_file_edits(
    tx: &Connection,
    thread_id: i64,
    anchors: &file_edits::Anchors,
    cwds: &[(usize, String)],
    base: usize,
    inserted: &[Inserted],
    appended: bool,
) -> Result<()> {
    // No tool call in an append's tail: nothing to add.
    if appended && !inserted.iter().any(|m| m.tool_name.is_some()) {
        return Ok(());
    }
    let rows: Vec<file_edits::ToolRow> = inserted
        .iter()
        .map(|m| file_edits::ToolRow {
            id: Some(m.id),
            tool_name: m.tool_name,
            text: m.text,
            ts: m.ts,
            cwd: m
                .tool_name
                .and_then(|_| cwd_at(cwds, (m.seq as usize).saturating_sub(base))),
        })
        .collect();
    file_edits::derive(tx, thread_id, anchors, &rows, appended)?;
    Ok(())
}

/// Mark LLM-distilled knowledge stale when the message set actually changed. The thread
/// upsert doesn't touch knowledge_msg_count (set at distillation time), so it still holds
/// the prior count here; only threads that were distilled AND changed get reset, so
/// unchanged re-indexes never touch distilled knowledge. `content_changed` covers a full
/// replace whose count didn't move (an in-place edit).
///
/// The facts themselves stay: `knowledge::store_distilled` replaces the non-curated LLM
/// facts atomically when the re-distill lands, so until then the old ones keep serving
/// Project Memory, the SessionStart hook and recall. Deleting them here emptied an active
/// session's memory on its first appended turn, for the rest of the session.
fn invalidate_distilled(
    tx: &Connection,
    thread_id: i64,
    distillable: i64,
    content_changed: bool,
) -> Result<()> {
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
        }
    }
    Ok(())
}

/// True when `stored` is a row indexed before the index-time cap and `new` is the same text
/// freshly parsed, i.e. capped by `claude::cap_tool_text`. Every indexer that caps (Claude,
/// Codex, Gemini, Qwen, Cline and forks, Continue) caps the `tool` rows; Claude Code also
/// caps its tool calls (assistant rows with a tool_name, "Write: {input json}"). An upgrade
/// must not read the shorter text as a rewritten transcript and fully replace the thread.
/// Uncapped sources never match: their fresh text is the full one, longer than any cap.
fn is_recapped(new: &ParsedMessage, stored: &str) -> bool {
    (new.role == "tool" || (new.role == "assistant" && new.tool_name.is_some()))
        && stored.len() > new.text.len()
        && stored.len() > claude::TOOL_TEXT_CAP_CHARS
        && claude::cap_tool_text(stored) == new.text
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
/// Where the stamp of a session file that could not be read at all is kept in `index_state`:
/// the file's path plus `#unreadable`, a key no reader of the file itself looks up. Kept under
/// the path itself, it would read as indexed and unchanged to a later Callimachus that can
/// read the form, which would skip the file until it next changed. Such a file gets no stamp
/// of its own, so it is read again on every pass (a Reindex included); this stamp only lets
/// the parse canary count it once per change instead of once per pass: with the watcher's
/// small batches summed (`parse_health`), one broken file counted every time would add up to
/// a format-change warning.
pub(crate) fn unreadable_key(path: &str) -> String {
    format!("{path}#unreadable")
}

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
    open_external_readonly_mode(path).map(|(conn, _)| conn)
}

/// [`open_external_readonly`], also saying whether it took the `immutable=1` fallback
/// (`true`). Such a connection reads the main file only: rows still in the `-wal` are
/// invisible to it, so a WAL-mode source that records its file stamp after that pass would
/// skip those rows until the file next changes (OpenCode, whose new turns sit in
/// `opencode.db-wal` until a checkpoint).
pub fn open_external_readonly_mode(path: &Path) -> Result<(Connection, bool)> {
    let ro = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
    if let Ok(conn) = Connection::open_with_flags(path, ro) {
        // A trivial query forces SQLite to actually touch the file now, so a lock
        // surfaces here (and we can fall back) rather than mid-scan.
        if conn.query_row("SELECT 1", [], |_| Ok(())).is_ok() {
            return Ok((conn, false));
        }
    }
    // Immutable fallback: file:<path>?immutable=1
    let uri = format!("file:{}?immutable=1", path.to_string_lossy());
    let conn = Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("opening {} read-only", path.display()))?;
    Ok((conn, true))
}

/// Read-only change check for a single-file source (Cursor/Goose DBs). Returns
/// `(unchanged, mtime, size)`. Unlike a write-on-check, this NEVER advances `index_state`:
/// the caller must persist the new state with `set_file_state` only AFTER its upserts
/// succeed. Writing it here (before the work) would let the watcher's retry skip threads
/// that failed mid-pass on a transient write-lock, silently dropping them.
pub fn file_change_state(conn: &Connection, path: &Path) -> Result<(bool, i64, i64)> {
    let (mtime, size) = file_stamp(path)?;
    let path_str = path.to_string_lossy().to_string();
    let unchanged = matches!(file_state(conn, &path_str)?, Some((m, s)) if m == mtime && s == size);
    Ok((unchanged, mtime, size))
}

/// A file's `(mtime in epoch seconds, size in bytes)`: the change key every indexer stores
/// in `index_state`. An mtime the platform can't report reads as 0.
pub fn file_stamp(path: &Path) -> std::io::Result<(i64, i64)> {
    let meta = std::fs::metadata(path)?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64);
    Ok((mtime, meta.len() as i64))
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
    fn file_edits_follow_the_messages_and_todos_keep_their_uid() {
        let p = std::env::temp_dir().join(format!("calli_fedits_{}.db", std::process::id()));
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        let mut conn = crate::db::open(&p).unwrap();
        let sid = source_id(&conn, "claude_code").unwrap();
        let call = |name: &str, input: &str, ts: i64| ParsedMessage {
            role: "assistant".into(),
            text: format!("{name}: {input}"),
            tool_name: Some(name.into()),
            ts: Some(ts),
        };
        let mut t = thread_with(&["TODO: wire the exporter"]);
        t.project_path = Some("/no/such/checkout/app".into());
        t.messages.push(call(
            "Read",
            r#"{"file_path":"/no/such/checkout/app/src/a.rs"}"#,
            10,
        ));
        t.messages.push(call(
            "Edit",
            r#"{"file_path":"/no/such/checkout/app/src/a.rs"}"#,
            11,
        ));
        t.messages
            .push(call("Bash", r#"{"command":"cat docs/x.md"}"#, 12));
        upsert_thread(&mut conn, sid, &t).unwrap();
        let tid: i64 = conn
            .query_row("SELECT id FROM threads WHERE external_id = 't1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let rows = |conn: &rusqlite::Connection| -> Vec<(String, String, i64)> {
            conn.prepare("SELECT path, op, ts FROM file_edits WHERE thread_id = ?1 ORDER BY rowid")
                .unwrap()
                .query_map([tid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        let want = |v: &[(&str, &str, i64)]| -> Vec<(String, String, i64)> {
            v.iter()
                .map(|(a, b, c)| (a.to_string(), b.to_string(), *c))
                .collect()
        };
        assert_eq!(
            rows(&conn),
            want(&[
                ("src/a.rs", "read", 10),
                ("src/a.rs", "edit", 11),
                ("docs/x.md", "read", 12)
            ])
        );
        let todo_uid: String = conn
            .query_row("SELECT uid FROM facts WHERE kind = 'todo'", [], |r| {
                r.get(0)
            })
            .unwrap();

        // An append adds only the tail's rows.
        t.messages.push(call(
            "Write",
            r#"{"file_path":"src/b.rs","content":"x"}"#,
            13,
        ));
        upsert_thread(&mut conn, sid, &t).unwrap();
        assert_eq!(rows(&conn).len(), 4);
        assert_eq!(
            rows(&conn)[3],
            ("src/b.rs".to_string(), "write".to_string(), 13)
        );

        // A full replace re-derives them (no duplicates) and the open TODO keeps its uid.
        t.messages[0].text = "TODO: wire the exporter (edited)".into();
        t.messages.insert(
            1,
            thread_with(&["TODO: wire the exporter"]).messages.remove(0),
        );
        upsert_thread(&mut conn, sid, &t).unwrap();
        assert_eq!(rows(&conn).len(), 4);
        let uids: Vec<String> = conn
            .prepare("SELECT uid FROM facts WHERE kind = 'todo' ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(uids.len(), 2, "{uids:?}");
        assert!(uids.contains(&todo_uid), "the same TODO keeps its identity");
        let seqs: Vec<i64> = conn
            .prepare("SELECT source_seq FROM facts WHERE kind = 'todo' ORDER BY source_seq")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(seqs, vec![0, 1]);

        // Deleting the thread cascades.
        conn.execute("DELETE FROM threads WHERE id = ?1", [tid])
            .unwrap();
        assert!(rows(&conn).is_empty());

        // The lookups phase 2 runs: by path (cal why) and by thread.
        let plan = |sql: &str| -> String {
            conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap()
                .query_map([], |r| r.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
                .join(" | ")
        };
        let p = plan("SELECT thread_id, op, ts FROM file_edits WHERE path = 'a' AND ts >= 0");
        assert!(p.contains("USING INDEX idx_file_edits_path_ts"), "{p}");
        let p = plan("SELECT path, op FROM file_edits WHERE thread_id = 1");
        assert!(p.contains("USING INDEX idx_file_edits_thread"), "{p}");
        let p = plan("SELECT id FROM threads WHERE last_indexed_at >= 5");
        assert!(p.contains("idx_threads_last_indexed"), "{p}");
    }

    /// Usage that arrives after its message was stored (or a source that only now captures
    /// it) is written onto the stored prefix in place: the append stays an append, no row
    /// is replaced, and an unchanged re-parse writes nothing.
    #[test]
    fn late_usage_fills_the_stored_prefix_in_place() {
        let p = std::env::temp_dir().join(format!("calli_lateusage_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let mut conn = crate::db::open(&p).unwrap();
        let sid = source_id(&conn, "codex").unwrap();
        upsert_thread(&mut conn, sid, &thread_with(&["alpha", "beta"])).unwrap();
        let tid: i64 = conn
            .query_row("SELECT id FROM threads WHERE external_id = 't1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let before = message_rowids(&conn, tid);
        let mut grown = thread_with(&["alpha", "beta", "gamma"]);
        let usage = |input| super::MsgUsage {
            model: "gpt-5".into(),
            input,
            output: 2,
            cache_write: 0,
            cache_read: 3,
        };
        grown.usage = vec![(1, usage(10)), (2, usage(20))];
        assert_eq!(upsert_thread(&mut conn, sid, &grown).unwrap(), 1);
        assert_eq!(&message_rowids(&conn, tid)[..2], &before[..]);
        let stored: Vec<(Option<String>, Option<i64>)> = conn
            .prepare("SELECT model, input_tokens FROM messages WHERE thread_id = ?1 ORDER BY seq")
            .unwrap()
            .query_map([tid], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            stored,
            vec![
                (None, None),
                (Some("gpt-5".into()), Some(10)),
                (Some("gpt-5".into()), Some(20))
            ]
        );
        assert_eq!(upsert_thread(&mut conn, sid, &grown).unwrap(), 0);
        let changes = conn.total_changes();
        upsert_thread(&mut conn, sid, &grown).unwrap();
        // The threads row upsert is the only write of an unchanged re-parse.
        assert!(
            conn.total_changes() - changes <= 1,
            "{}",
            conn.total_changes() - changes
        );
    }

    #[test]
    fn usage_without_a_message_lands_on_the_last_one_and_adds_up() {
        let mut t = thread_with(&["a", "b"]);
        let u = |n| super::MsgUsage {
            model: "m".into(),
            input: n,
            ..Default::default()
        };
        t.attach_usage(Some(0), u(1));
        t.attach_usage(None, u(2));
        t.attach_usage(Some(5), u(3));
        let got: Vec<(usize, i64)> = t.usage.iter().map(|(i, u)| (*i, u.input)).collect();
        assert_eq!(got, vec![(0, 1), (1, 5)]);
        let mut empty = ParsedThread::default();
        empty.attach_usage(None, u(1));
        assert!(empty.usage.is_empty(), "no row to carry it");
    }

    #[test]
    fn subagent_transcripts_roll_up_to_their_session() {
        let p = std::env::temp_dir().join(format!("calli_subs_{}.db", std::process::id()));
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        let mut conn = crate::db::open(&p).unwrap();
        let sid = source_id(&conn, "claude_code").unwrap();
        let mut add = |ext: &str, sub: bool| -> i64 {
            let mut t = thread_with(&["hi"]);
            t.external_id = ext.into();
            t.is_subagent = sub;
            upsert_thread(&mut conn, sid, &t).unwrap();
            conn.query_row(
                "SELECT id FROM threads WHERE external_id = ?1",
                [ext],
                |r| r.get(0),
            )
            .unwrap()
        };
        let parent = add("-r-app/s1.jsonl", false);
        let a = add("-r-app/s1/subagents/agent-a.jsonl", true);
        let b = add("-r-app/s1/subagents/agent-b.jsonl", true);
        let other = add("-r-app/s10/subagents/agent-c.jsonl", true);
        add("-r-app/s10.jsonl", false);
        assert_eq!(
            super::session_thread_ids(&conn, parent).unwrap(),
            vec![parent, a, b]
        );
        assert_eq!(super::session_thread_ids(&conn, a).unwrap(), vec![a]);
        assert_eq!(super::parent_session_id(&conn, b).unwrap(), parent);
        assert_eq!(super::parent_session_id(&conn, parent).unwrap(), parent);
        assert_ne!(super::parent_session_id(&conn, other).unwrap(), parent);
        let plan: String = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {}", super::SUBAGENTS_OF_SQL))
            .unwrap()
            .query_map(rusqlite::params![1, "a", "b"], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join(" | ");
        assert!(plan.contains("external_id>?"), "{plan}");
    }

    #[test]
    fn incremental_reindex_appends_tail_only() {
        let p = std::env::temp_dir().join(format!("calli_incr_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let mut conn = crate::db::open(&p).unwrap();
        let sid = source_id(&conn, "claude_code").unwrap();

        // Initial index: 3 messages, all inserted.
        assert_eq!(
            upsert_thread(&mut conn, sid, &thread_with(&["alpha", "beta", "gamma"])).unwrap(),
            3
        );
        let tid: i64 = conn
            .query_row("SELECT id FROM threads WHERE external_id = 't1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let before = message_rowids(&conn, tid);
        assert_eq!(before.len(), 3);

        // Re-index after two messages were appended: the new tail is inserted, the existing
        // three rows are reused (rowids unchanged) — proof we didn't re-insert the prefix.
        let grown = thread_with(&["alpha", "beta", "gamma", "delta", "epsilon"]);
        assert_eq!(
            upsert_thread(&mut conn, sid, &grown).unwrap(),
            2,
            "counts only the new rows"
        );
        assert_eq!(
            upsert_thread(&mut conn, sid, &grown).unwrap(),
            0,
            "an unchanged re-parse writes nothing"
        );
        let after = message_rowids(&conn, tid);
        assert_eq!(after.len(), 5);
        assert_eq!(
            &after[..3],
            &before[..],
            "prefix rows must be reused on append"
        );

        // A shrink falls back to a full replace: count drops to 3 and the new content lands.
        // (Rowid identity can't be asserted here — SQLite reuses rowids after a full delete.)
        assert_eq!(
            upsert_thread(&mut conn, sid, &thread_with(&["alpha", "CHANGED", "gamma"])).unwrap(),
            3,
            "a full replace rewrites every row"
        );
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

    fn temp_db(tag: &str) -> rusqlite::Connection {
        let p = std::env::temp_dir().join(format!("calli_{tag}_{}.db", std::process::id()));
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        crate::db::open(&p).unwrap()
    }

    /// A tool row stored by a build before the index-time cap must not turn the first
    /// re-parse after an upgrade into a full replace: it is rewritten in place and the
    /// rest of the thread (ids, embedding state) is untouched.
    #[test]
    fn uncapped_legacy_tool_rows_are_recapped_in_place() {
        use super::claude::cap_tool_text;
        let mut conn = temp_db("recap");
        let sid = source_id(&conn, "claude_code").unwrap();
        let raw = format!("HEADMARK {} tailmarkword", "line of output\n".repeat(3_000));
        let mut legacy = thread_with(&["run the tests", "here you go"]);
        legacy.messages.insert(
            1,
            ParsedMessage {
                role: "tool".into(),
                text: raw.clone(),
                tool_name: None,
                ts: Some(1),
            },
        );
        upsert_thread(&mut conn, sid, &legacy).unwrap();
        let tid: i64 = conn
            .query_row("SELECT id FROM threads WHERE external_id = 't1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        conn.execute("UPDATE messages SET embedded = 1 WHERE role <> 'tool'", [])
            .unwrap();
        let before = message_rowids(&conn, tid);

        // Today's parse of the same transcript caps the tool output, plus one new message.
        let mut fresh = thread_with(&["run the tests", "here you go", "thanks"]);
        fresh.messages.insert(
            1,
            ParsedMessage {
                role: "tool".into(),
                text: cap_tool_text(&raw),
                tool_name: None,
                ts: Some(1),
            },
        );
        upsert_thread(&mut conn, sid, &fresh).unwrap();

        let after = message_rowids(&conn, tid);
        assert_eq!(&after[..3], &before[..], "no full replace");
        assert_eq!(after.len(), 4);
        let stored: String = conn
            .query_row(
                "SELECT text FROM messages WHERE thread_id = ?1 AND seq = 1",
                [tid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, cap_tool_text(&raw));
        let embedded: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE thread_id = ?1 AND embedded = 1",
                [tid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(embedded, 2, "the prose keeps its embedding state");
        let fts = |q: &str| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH ?1",
                [q],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(fts("tailmarkword"), 1, "FTS follows the rewrite");

        // Any other difference in a stored row is still a rewritten transcript.
        let mut edited = thread_with(&["run the tests", "here you go", "thanks"]);
        edited.messages.insert(
            1,
            ParsedMessage {
                role: "tool".into(),
                text: "different output".into(),
                tool_name: None,
                ts: Some(1),
            },
        );
        upsert_thread(&mut conn, sid, &edited).unwrap();
        let stored: String = conn
            .query_row(
                "SELECT text FROM messages WHERE thread_id = ?1 AND seq = 1",
                [tid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, "different output");
    }

    /// Claude Code tool calls are capped at parse time too, so a legacy uncapped call row
    /// (a huge Write input) is rewritten in place on the next re-parse, not read as a
    /// rewritten transcript.
    #[test]
    fn uncapped_legacy_tool_calls_are_recapped_in_place() {
        use super::claude::cap_tool_text;
        let mut conn = temp_db("recap_call");
        let sid = source_id(&conn, "claude_code").unwrap();
        let raw = format!("Write: {{\"content\":\"{}\"}}", "x = 1\\n".repeat(5_000));
        let call = |text: String| ParsedMessage {
            role: "assistant".into(),
            text,
            tool_name: Some("Write".into()),
            ts: Some(1),
        };
        let mut legacy = thread_with(&["write it"]);
        legacy.messages.push(call(raw.clone()));
        upsert_thread(&mut conn, sid, &legacy).unwrap();
        let tid: i64 = conn
            .query_row("SELECT id FROM threads", [], |r| r.get(0))
            .unwrap();
        let before = message_rowids(&conn, tid);

        let mut fresh = thread_with(&["write it"]);
        fresh.messages.push(call(cap_tool_text(&raw)));
        fresh.messages.push(ParsedMessage {
            role: "user".into(),
            text: "thanks".into(),
            tool_name: None,
            ts: Some(2),
        });
        assert_eq!(upsert_thread(&mut conn, sid, &fresh).unwrap(), 1);
        let after = message_rowids(&conn, tid);
        assert_eq!(&after[..2], &before[..], "no full replace");
        let stored: String = conn
            .query_row(
                "SELECT text FROM messages WHERE thread_id = ?1 AND seq = 1",
                [tid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, cap_tool_text(&raw));

        // A prose assistant row is never "recapped": a shorter text there is a real edit.
        let prose = ParsedMessage {
            role: "assistant".into(),
            text: raw.clone(),
            tool_name: None,
            ts: None,
        };
        assert!(!super::is_recapped(
            &ParsedMessage {
                text: cap_tool_text(&raw),
                ..prose
            },
            &raw
        ));
    }

    /// The append path's seq check must be one probe of the (thread_id, seq) unique index,
    /// not a scan of the thread's rows: it runs under the write lock on every append.
    #[test]
    fn the_append_seq_check_uses_the_unique_index() {
        let conn = temp_db("append_plan");
        let plan: Vec<String> = conn
            .prepare(
                "EXPLAIN QUERY PLAN
                 SELECT COALESCE(MAX(seq) + 1, 0) FROM messages WHERE thread_id = ?1",
            )
            .unwrap()
            .query_map([1_i64], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let plan = plan.join(" | ");
        assert!(
            plan.contains("USING COVERING INDEX sqlite_autoindex_messages_1"),
            "{plan}"
        );
    }

    /// The tombstone is checked again under the write lock: an indexer that passed the early
    /// check just before the user's delete committed must not re-insert the thread.
    #[test]
    fn a_delete_that_lands_after_the_early_check_still_wins() {
        let p = std::env::temp_dir().join(format!("calli_tomb_tx_{}.db", std::process::id()));
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        let mut ui = crate::db::open(&p).unwrap();
        let sid = source_id(&ui, "claude_code").unwrap();
        upsert_thread(&mut ui, sid, &thread_with(&["hi"])).unwrap();

        // The user's delete holds the write lock (uncommitted) while the watcher re-indexes.
        let tx = ui
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        tx.execute(
            "INSERT INTO deleted_threads (source_id, external_id, deleted_at) VALUES (?1, 't1', 0)",
            [sid],
        )
        .unwrap();
        tx.execute("DELETE FROM threads WHERE external_id = 't1'", [])
            .unwrap();
        let path = p.clone();
        let watcher = std::thread::spawn(move || {
            let mut conn = crate::db::open(&path).unwrap();
            // Passes the early check (the delete isn't committed), then waits for the lock.
            upsert_thread(&mut conn, sid, &thread_with(&["hi", "more"])).unwrap()
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        tx.commit().unwrap();

        assert_eq!(watcher.join().unwrap(), 0, "the upsert backs off");
        let n: i64 = ui
            .query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "the deleted thread stays deleted");
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

        conn.execute(
            "INSERT INTO facts (thread_id, kind, text, status, extractor, created_at)
             VALUES (?1, 'decision', 'Use FTS5 for search', 'open', 'llm', 0)",
            [tid],
        )
        .unwrap();
        let llm_facts = |c: &rusqlite::Connection| -> i64 {
            c.query_row(
                "SELECT COUNT(*) FROM facts WHERE thread_id = ?1 AND extractor = 'llm'",
                [tid],
                |r| r.get(0),
            )
            .unwrap()
        };

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
        assert_eq!(
            llm_facts(&conn),
            1,
            "stale facts keep serving until the re-distill replaces them"
        );

        // Same through the byte-cursor append path, and through a full replace.
        conn.execute(
            "UPDATE threads SET knowledge_extracted = 1, knowledge_msg_count = distillable_count WHERE id = ?1",
            [tid],
        )
        .unwrap();
        let tail = mk(&[("assistant", "f")]);
        assert_eq!(
            super::append_messages(&mut conn, sid, &tail, 6).unwrap(),
            Some(1)
        );
        assert_eq!(col(&conn, "distillable_count"), 6);
        assert_eq!(col(&conn, "message_count"), 7);
        assert_eq!(col(&conn, "knowledge_extracted"), 0);
        assert_eq!(llm_facts(&conn), 1);
        upsert_thread(&mut conn, sid, &mk(&[("user", "rewritten")])).unwrap();
        assert_eq!(llm_facts(&conn), 1, "a full replace keeps them too");

        let _ = std::fs::remove_file(&p);
    }

    /// Inside a caller's open transaction (claude::scan's batches) upsert_thread uses a
    /// savepoint: it doesn't fail on "transaction within a transaction", and a thread it
    /// skips (tombstoned) leaves the rest of the batch intact.
    #[test]
    fn upsert_inside_an_open_transaction_uses_a_savepoint() {
        let mut conn = temp_db("sp");
        let sid = source_id(&conn, "claude_code").unwrap();
        conn.execute(
            "INSERT INTO deleted_threads (source_id, external_id, deleted_at) VALUES (?1, 'gone', 0)",
            [sid],
        )
        .unwrap();
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert_eq!(
            upsert_thread(&mut conn, sid, &thread_with(&["a", "b"])).unwrap(),
            2
        );
        let mut gone = thread_with(&["c"]);
        gone.external_id = "gone".into();
        assert_eq!(upsert_thread(&mut conn, sid, &gone).unwrap(), 0);
        assert!(!conn.is_autocommit(), "the outer transaction is still open");
        conn.execute_batch("COMMIT").unwrap();
        let ids: Vec<String> = conn
            .prepare("SELECT external_id FROM threads")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(ids, vec!["t1"]);
    }

    /// Every registered kind has its `sources` row (else every scan of it fails with
    /// "unknown source kind"), and no kind is registered twice. The telemetry allowlist half
    /// lives in the app crate (`telemetry::tests::every_source_is_on_the_telemetry_allowlist`),
    /// since telemetry is GUI only and this crate never references it.
    #[test]
    fn the_source_registry_matches_the_schema_and_telemetry() {
        let conn = temp_db("registry");
        let mut kinds = std::collections::HashSet::new();
        for s in super::SOURCES {
            assert!(kinds.insert(s.kind), "{} registered twice", s.kind);
            assert!(
                source_id(&conn, s.kind).is_ok(),
                "no sources row for {}",
                s.kind
            );
            assert!(super::scanner_for(s.kind).is_some());
        }
        assert!(super::scanner_for("in_app").is_none());
    }

    /// `caps_tool_output` must match what each indexer does: cleanup compacts exactly the
    /// flagged sources' tool rows, and compacting a source that stores full tool output
    /// would be undone (and the thread rewritten) on its next re-index.
    #[test]
    fn caps_tool_output_matches_the_indexers() {
        use super::{
            claude, cline, codex, continue_cli, copilot, copilot_cli, cursor, gemini, goose,
        };
        use super::{kilo, opencode, qwen, roo};
        for s in super::SOURCES {
            let code = match s.kind {
                claude::KIND => include_str!("claude.rs"),
                codex::KIND => include_str!("codex.rs"),
                cursor::KIND => include_str!("cursor.rs"),
                // Qwen parses through Gemini's shared parts flattener.
                gemini::KIND | qwen::KIND => include_str!("gemini.rs"),
                goose::KIND => include_str!("goose.rs"),
                opencode::KIND => include_str!("opencode.rs"),
                continue_cli::KIND => include_str!("continue_cli.rs"),
                // Roo and Kilo run Cline's engine.
                cline::KIND | roo::KIND | kilo::KIND => include_str!("cline.rs"),
                copilot::KIND => include_str!("copilot.rs"),
                copilot_cli::KIND => include_str!("copilot_cli.rs"),
                other => panic!("add {other}'s indexer source here"),
            };
            assert_eq!(
                code.contains("cap_tool_text("),
                s.caps_tool_output,
                "{}: caps_tool_output disagrees with its indexer",
                s.kind
            );
        }
        let capped: Vec<&str> = super::capped_tool_sources().collect();
        assert!(capped.contains(&claude::KIND) && !capped.contains(&goose::KIND));
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

    fn scratch(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "cp_{tag}_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    #[test]
    fn canonical_project_resolves_worktrees_to_the_main_repo() {
        let base = scratch("wt");
        let main = base.join("repo");
        let wt_git = main.join(".git/worktrees/feature");
        std::fs::create_dir_all(&wt_git).unwrap();
        std::fs::write(wt_git.join("commondir"), "../..\n").unwrap();
        let wt = base.join("repo-feature");
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", wt_git.display())).unwrap();

        let k_main = canonical_project(main.to_str().unwrap());
        assert_eq!(canonical_project(wt.to_str().unwrap()), k_main);
        assert_eq!(canonical_project(wt.join("src").to_str().unwrap()), k_main);

        // A submodule (.git file into .git/modules) stays its own project.
        let sub = main.join("vendor/lib");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(main.join(".git/modules/lib")).unwrap();
        std::fs::write(sub.join(".git"), "gitdir: ../../.git/modules/lib\n").unwrap();
        assert_eq!(
            canonical_project(sub.to_str().unwrap()),
            Some(sub.to_string_lossy().into_owned())
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn canonical_project_stops_at_home() {
        let home = scratch("home");
        std::fs::create_dir_all(home.join(".git")).unwrap(); // dotfiles repo in ~
        let scratch_dir = home.join("scratch/notes");
        std::fs::create_dir_all(&scratch_dir).unwrap();
        let repo = home.join("code/app");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        let key = |p: &std::path::Path| {
            super::canonical_project_with_home(p.to_str().unwrap(), Some(&home))
        };

        assert_eq!(
            key(&scratch_dir),
            Some(scratch_dir.to_string_lossy().into_owned()),
            "a non-repo dir under home keys on itself, not on ~"
        );
        assert_eq!(
            key(&repo.join("src")),
            Some(repo.to_string_lossy().into_owned())
        );
        assert_eq!(
            key(&home),
            Some(home.to_string_lossy().into_owned()),
            "home itself is still its own project"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn strips_windows_verbatim_prefixes() {
        use std::path::Path;
        assert_eq!(
            super::strip_verbatim_prefix(Path::new(r"\\?\C:\Users\me\repo")),
            Path::new(r"C:\Users\me\repo")
        );
        assert_eq!(
            super::strip_verbatim_prefix(Path::new(r"\\?\UNC\srv\share\repo")),
            Path::new(r"\\srv\share\repo")
        );
        assert_eq!(
            super::strip_verbatim_prefix(Path::new("/home/me/repo")),
            Path::new("/home/me/repo")
        );
        assert_eq!(super::path_key(Path::new("/")), "/");
        assert_eq!(super::path_key(Path::new("/a/b/")), "/a/b");
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
