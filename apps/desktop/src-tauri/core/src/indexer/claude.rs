//! Claude Code indexer. History lives under `<config>/projects/<project-slug>/`, where
//! `<config>` is `$CLAUDE_CONFIG_DIR` (or a Settings override) and defaults to
//! `~/.claude`. The top-level `<session-uuid>.jsonl` files are the main threads; nested
//! `<uuid>/subagents/agent-*.jsonl` files are subagent transcripts. We index all of them
//! recursively. Each file is one thread keyed by its path-relative id (unique even when
//! subagents reuse a session id). We extract user/assistant text, tool calls, and tool
//! results; `thinking` blocks are skipped.
//!
//! Transcripts are append-only, so after the first full parse each file keeps a byte
//! cursor in `index_state` (`last_offset`, plus a fingerprint of the bytes around it in
//! `hash`). When a file only grew, just the appended lines are read and parsed and the
//! stored prefix is reused, instead of re-reading and re-parsing a transcript that can
//! be tens of megabytes on every write of an active session, and only the new rows are
//! written (the stored messages are never read back). Anything that doesn't look
//! like a pure append (shrink, rewrite, fingerprint mismatch, thread missing from the
//! index) falls back to a full parse.

use super::common::{join_text_parts, parse_rfc3339, push_message, truncate_title};
use super::{
    append_messages_at, file_stamp, source_id, upsert_thread_at, IndexReport, MsgUsage,
    ParsedThread,
};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Duration, Instant};

pub const KIND: &str = "claude_code";

// ---------------------------------------------------------------------------------------
// Data root
// ---------------------------------------------------------------------------------------

static CONFIG_DIR_OVERRIDE: RwLock<Option<PathBuf>> = RwLock::new(None);

/// Point the indexer (and everything else that asks [`config_dir`]) at a relocated Claude
/// config dir. A GUI app launched from Finder/Dock never sees the shell's
/// `CLAUDE_CONFIG_DIR`, so the app needs a way to set it explicitly (a Settings value).
/// `None` (or an empty path) restores the default lookup.
pub fn set_config_dir_override(dir: Option<PathBuf>) {
    if let Ok(mut slot) = CONFIG_DIR_OVERRIDE.write() {
        *slot = dir.filter(|d| !d.as_os_str().is_empty()).map(expand_tilde);
    }
}

/// The Settings override set by [`set_config_dir_override`], if any (no env fallback).
pub fn config_dir_override() -> Option<PathBuf> {
    CONFIG_DIR_OVERRIDE.read().ok().and_then(|g| g.clone())
}

/// Claude Code's config dir: the override, else `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = CONFIG_DIR_OVERRIDE.read().ok().and_then(|g| g.clone()) {
        return Some(dir);
    }
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        return Some(expand_tilde(PathBuf::from(dir)));
    }
    dirs::home_dir().map(|h| h.join(".claude"))
}

/// `<config>/projects`, or None if no config dir can be resolved.
pub fn projects_root() -> Option<PathBuf> {
    config_dir().map(|d| d.join("projects"))
}

/// `app_config` key holding a user-chosen Claude config dir (a Settings value). This is
/// how the override reaches every process that opens the index (app, watcher, `cal`).
pub const CONFIG_DIR_SETTING: &str = "paths.claude_config_dir";

/// Apply the [`CONFIG_DIR_SETTING`] override from the index (cleared when unset). Called at
/// the start of every scan, so a changed setting takes effect on the next pass.
pub fn load_config_dir_setting(conn: &Connection) {
    set_config_dir_override(read_path_setting(conn, CONFIG_DIR_SETTING));
}

/// A non-empty path stored under `key` in `app_config`, if any.
pub(crate) fn read_path_setting(conn: &Connection, key: &str) -> Option<PathBuf> {
    // A read error reads as unset, the same as a missing row.
    crate::knowledge::app_config::get_nonblank(conn, key)
        .ok()
        .flatten()
        .map(PathBuf::from)
}

/// Expand a leading `~` (env vars and settings values are often written that way).
pub(crate) fn expand_tilde(p: PathBuf) -> PathBuf {
    let Ok(rest) = p.strip_prefix("~") else {
        return p;
    };
    match dirs::home_dir() {
        Some(home) => home.join(rest),
        None => p,
    }
}

/// Check a data folder typed in Settings and return the spelling to store: `~` expanded,
/// absolute, canonical (symlinks and `..` resolved, no Windows `\\?\` prefix), and naming a
/// folder that exists. Every process that opens the index (the app, the watcher, `cal
/// hook`) applies the stored value, so a relative or unexpanded path would resolve against
/// each one's own working directory, and a typo would index nothing without any error.
/// The errors are shown as is in Settings.
pub fn normalize_data_root(raw: &str) -> Result<String> {
    let raw = raw.trim();
    let path = expand_tilde(PathBuf::from(raw));
    if !path.is_absolute() {
        if raw.contains('$') || raw.contains('%') {
            anyhow::bail!(
                "\"{raw}\" uses an environment variable, which isn't expanded here. \
                 Enter the folder's full path."
            );
        }
        let example = if cfg!(windows) {
            "such as C:\\Users\\you\\.claude"
        } else {
            "starting with / or ~"
        };
        anyhow::bail!("\"{raw}\" isn't a full path. Enter the folder's full path, {example}.");
    }
    let real = match fs::canonicalize(&path) {
        Ok(real) => super::strip_verbatim_prefix(&real),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            anyhow::bail!("There's no folder at {}.", path.display())
        }
        Err(e) => anyhow::bail!("Can't open {}: {e}.", path.display()),
    };
    if !real.is_dir() {
        anyhow::bail!("{} is a file, not a folder.", real.display());
    }
    real.into_os_string()
        .into_string()
        .map_err(|p| anyhow::anyhow!("{} isn't a valid Unicode path.", Path::new(&p).display()))
}

/// The thread's stable id: the transcript path relative to the projects root, always
/// joined with '/' so ids (and the `/subagents/` and slug checks made on them) are the same
/// on every OS. The `cal hook` path that maps a hook's transcript_path back to a thread
/// must derive ids with this same function.
pub fn external_id_for(root: &Path, path: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(rel) => rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
        Err(_) if cfg!(windows) => path.to_string_lossy().replace('\\', "/"),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

// ---------------------------------------------------------------------------------------
// Tool output cap (shared by every indexer that stores tool results)
// ---------------------------------------------------------------------------------------

/// Max characters of one tool result kept in the index. Tool output (file dumps, build
/// logs, test runs) was ~70% of all stored text even though it is never embedded or
/// distilled. The head keeps the command's context and the tail keeps the errors and
/// summaries that recurring-issue mining reads, so the middle is what gets elided.
pub const TOOL_TEXT_CAP_CHARS: usize = 16_000;
const TOOL_TEXT_TAIL_CHARS: usize = 4_000;

/// Cap one (already trimmed) tool result, or one Claude Code tool call, at
/// [`TOOL_TEXT_CAP_CHARS`]: head + an elision marker + tail. Deterministic, char-boundary
/// safe, and idempotent (the result is never longer than the cap), so the incremental
/// re-index prefix check stays stable and `cleanup::compact_tool_output` produces exactly
/// what a fresh index would, for tool rows of every capping source and Claude Code's tool
/// calls.
pub fn cap_tool_text(text: &str) -> String {
    // Byte length bounds char count, so short text skips the char walk entirely.
    if text.len() <= TOOL_TEXT_CAP_CHARS {
        return text.to_string();
    }
    let total = text.chars().count();
    if total <= TOOL_TEXT_CAP_CHARS {
        return text.to_string();
    }
    // Size the head for the widest marker this text could need, so head + marker + tail
    // never exceeds the cap.
    let marker_budget = elision_marker(total).chars().count();
    let head = TOOL_TEXT_CAP_CHARS - TOOL_TEXT_TAIL_CHARS - marker_budget;
    let tail_from = total - TOOL_TEXT_TAIL_CHARS;
    let byte_at = |n: usize| text.char_indices().nth(n).map_or(text.len(), |(i, _)| i);
    let mut out = String::with_capacity(TOOL_TEXT_CAP_CHARS * 2);
    out.push_str(&text[..byte_at(head)]);
    out.push_str(&elision_marker(tail_from - head));
    out.push_str(&text[byte_at(tail_from)..]);
    out
}

fn elision_marker(elided: usize) -> String {
    format!("\n[… {elided} chars of tool output elided …]\n")
}

// ---------------------------------------------------------------------------------------
// Scanning
// ---------------------------------------------------------------------------------------

/// Recursively collect every `.jsonl` file under `dir`.
fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl(&path, out);
        } else if is_jsonl(&path) {
            out.push(path);
        }
    }
}

fn is_jsonl(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("jsonl")
}

/// Walk every project dir, (re)indexing thread files whose mtime/size changed.
pub fn scan(conn: &mut Connection, tick: &mut dyn FnMut()) -> Result<IndexReport> {
    load_config_dir_setting(conn);
    let Some(root) = projects_root() else {
        return Ok(IndexReport::default());
    };
    scan_root(conn, &root, tick)
}

/// [`scan`] over one projects root, with the writes grouped into [`Batch`]es.
pub(crate) fn scan_root(
    conn: &mut Connection,
    root: &Path,
    tick: &mut dyn FnMut(),
) -> Result<IndexReport> {
    let mut report = IndexReport::default();
    if !root.is_dir() {
        return Ok(report);
    }
    let sid = source_id(conn, KIND)?;

    let mut files = Vec::new();
    collect_jsonl(root, &mut files);

    let mut batch = Batch::new(conn);
    for path in files {
        tick();
        // Checked on every file, not only in Batch::run: unchanged files never reach run(),
        // so a batch opened by an early changed file would otherwise keep the write lock
        // through the whole rest of the walk (seconds on a cold cache or on Windows).
        if let Err(e) = batch.commit_if_due() {
            app_log!("[index] claude_code: committing a batch failed: {e:#}");
            report.errors += 1;
        }
        // Unchanged files (nearly all of them on a re-scan) are skipped before the batch
        // is consulted, so a no-op pass never takes the write lock.
        let size = match changed_size(batch.conn(), &path) {
            Ok(Some(size)) => size,
            Ok(None) => {
                report.record(Ok(None));
                continue;
            }
            Err(e) => {
                report.record_at(&path, Err(e));
                continue;
            }
        };
        report.record_at(
            &path,
            batch.run(size, |conn| index_file(conn, sid, root, &path)),
        );
    }
    if let Err(e) = batch.finish() {
        app_log!("[index] claude_code: committing the last batch failed: {e:#}");
        report.errors += 1;
    }
    Ok(report)
}

/// Files written per batch commit, at most.
const BATCH_FILES: usize = 32;
/// How long a batch may hold the write lock before it commits.
const BATCH_WINDOW: Duration = Duration::from_millis(250);
/// Files at least this big are indexed outside any batch. A full parse runs at roughly
/// 75 MB/s, so a smaller file adds at most ~30 ms to the lock hold.
const BATCH_MAX_FILE: i64 = 2 << 20;

/// Groups a full scan's writes into a few large transactions. One commit per thread spent
/// about a quarter of a cold index in COMMIT (FTS5 flushes and later merges a segment per
/// transaction), so a batch commits every [`BATCH_FILES`] files or [`BATCH_WINDOW`],
/// whichever comes first. Each file runs in its own savepoint inside the batch: a failing
/// file rolls back alone, and its cursor commits together with its thread.
///
/// The lock is taken lazily, right before a file that changed, and a file of
/// [`BATCH_MAX_FILE`] bytes or more first commits the batch and runs on its own, so the lock
/// is never held across a long parse: the worst hold is about one window plus one small
/// file, well under every connection's busy timeout. Dropping a batch without
/// [`Batch::finish`] (an early return, a panic) rolls back its uncommitted files, whose
/// cursors roll back with them, so the next pass redoes them.
struct Batch<'c> {
    conn: &'c mut Connection,
    /// When the open transaction began, and how many files it holds.
    open: Option<(Instant, usize)>,
}

impl<'c> Batch<'c> {
    fn new(conn: &'c mut Connection) -> Self {
        Self { conn, open: None }
    }

    fn conn(&self) -> &Connection {
        self.conn
    }

    /// Index one changed file of `size` bytes, inside the batch when it fits.
    fn run(
        &mut self,
        size: i64,
        index: impl FnOnce(&mut Connection) -> Result<Option<usize>>,
    ) -> Result<Option<usize>> {
        let fits = size < BATCH_MAX_FILE;
        if fits {
            self.commit_if_due()?;
        } else {
            self.commit()?;
        }
        // A BEGIN that loses the lock race past the busy timeout just leaves this file to
        // run unbatched, where upsert_thread takes (and reports failing to take) the lock.
        if self.open.is_none() && fits && self.conn.execute_batch("BEGIN IMMEDIATE").is_ok() {
            self.open = Some((Instant::now(), 0));
        }
        let Some((_, files)) = self.open.as_mut() else {
            return index(self.conn);
        };
        *files += 1;
        self.conn.execute_batch("SAVEPOINT claude_file")?;
        let result = index(self.conn);
        if self.conn.is_autocommit() {
            // SQLite rolled the whole transaction back on its own (SQLITE_FULL, an I/O
            // error): this file and the batch's earlier ones are gone, cursors included.
            self.open = None;
            return Err(match result {
                Err(e) => e,
                Ok(_) => anyhow::anyhow!("the index batch was rolled back"),
            });
        }
        let end = if result.is_ok() {
            "RELEASE claude_file"
        } else {
            "ROLLBACK TO claude_file; RELEASE claude_file"
        };
        self.conn.execute_batch(end)?;
        result
    }

    /// Commit the open transaction once it holds [`BATCH_FILES`] files or has been open for
    /// [`BATCH_WINDOW`]; otherwise leave it open for the next file.
    fn commit_if_due(&mut self) -> Result<()> {
        match self.open {
            Some((since, files)) if files >= BATCH_FILES || since.elapsed() >= BATCH_WINDOW => {
                self.commit()
            }
            _ => Ok(()),
        }
    }

    /// Commit the open transaction, if any. A failed COMMIT is rolled back so the
    /// connection is usable again; the lost files are redone by the next pass.
    fn commit(&mut self) -> Result<()> {
        if self.open.take().is_some() && !self.conn.is_autocommit() {
            if let Err(e) = self.conn.execute_batch("COMMIT") {
                let _ = self.conn.execute_batch("ROLLBACK");
                return Err(e.into());
            }
        }
        Ok(())
    }

    /// Commit what is left at the end of a scan.
    fn finish(mut self) -> Result<()> {
        self.commit()
    }
}

impl Drop for Batch<'_> {
    fn drop(&mut self) {
        if self.open.take().is_some() && !self.conn.is_autocommit() {
            let _ = self.conn.execute_batch("ROLLBACK");
        }
    }
}

/// The file's size when it changed since its cursor was written, None when unchanged.
/// The same check [`index_file`] starts with, without parsing or writing anything.
fn changed_size(conn: &Connection, path: &Path) -> Result<Option<i64>> {
    let (mtime, size) = file_stamp(path)?;
    let unchanged = read_cursor(conn, &path.to_string_lossy())?
        .is_some_and(|c| c.mtime == mtime && c.size == size);
    Ok((!unchanged).then_some(size))
}

/// Index just the given changed paths (the file watcher's fast path): a debounced batch
/// during an active session names one or two transcripts, so there is no reason to walk
/// and stat every file under the projects root. Directories (a new project or subagents
/// dir) are walked; paths outside the projects root are ignored.
pub fn index_paths(conn: &mut Connection, paths: &[PathBuf]) -> Result<IndexReport> {
    let mut report = IndexReport::default();
    load_config_dir_setting(conn);
    let Some(root) = projects_root() else {
        return Ok(report);
    };
    // Watch events can carry the resolved path when the root is (or sits under) a
    // symlink, so accept either spelling and key ids off whichever one matched.
    let mut roots = vec![root.clone()];
    if let Ok(real) = fs::canonicalize(&root) {
        if real != root {
            roots.push(real);
        }
    }
    let mut files: Vec<(PathBuf, PathBuf)> = Vec::new();
    for p in paths {
        let Some(base) = roots.iter().find(|r| p.starts_with(r)) else {
            continue;
        };
        if p.is_dir() {
            let mut found = Vec::new();
            collect_jsonl(p, &mut found);
            files.extend(found.into_iter().map(|f| (base.clone(), f)));
        } else if is_jsonl(p) && p.is_file() {
            files.push((base.clone(), p.clone()));
        }
    }
    files.sort();
    files.dedup();
    if files.is_empty() {
        return Ok(report);
    }
    let sid = source_id(conn, KIND)?;
    for (base, path) in files {
        report.record_at(&path, index_file(conn, sid, &base, &path));
    }
    Ok(report)
}

/// Index one thread file. Returns Some(message rows written) if indexed, None if unchanged
/// or deliberately skipped.
fn index_file(conn: &mut Connection, sid: i64, root: &Path, path: &Path) -> Result<Option<usize>> {
    let path_str = path.to_string_lossy().to_string();
    let (mtime, size) = file_stamp(path)?;

    let cursor = read_cursor(conn, &path_str)?;
    if let Some(c) = &cursor {
        if c.mtime == mtime && c.size == size {
            return Ok(None); // unchanged
        }
    }

    // external_id = path relative to projects root: stable and unique per file.
    let external_id = external_id_for(root, path);

    // The user deleted this thread: don't bring it back (and don't pay to parse it).
    if crate::cleanup::is_tombstoned(conn, sid, &external_id)? {
        write_cursor(conn, &path_str, mtime, size, 0, None)?;
        return Ok(None);
    }

    // Fast path: the file only grew since the cursor, so parse just the new bytes.
    if let Some(c) = cursor.as_ref().filter(|c| c.offset > 0 && size >= c.offset) {
        match try_append(conn, sid, path, &path_str, &external_id, c, mtime, size)? {
            Append::Applied(n) => return Ok(Some(n)),
            Append::NoChange => return Ok(None),
            Append::Fallback => {}
        }
    }

    // Project fallback: top-level dir name is the cwd-slug (used when a subagent
    // file carries no cwd of its own).
    let project_fallback = external_id.split('/').next().map(decode_slug);
    let parsed = parse_full(path, &external_id, project_fallback.as_deref())
        .with_context(|| format!("parsing {path_str}"))?;
    let mut thread = parsed.thread;
    if let Some(t) = thread.as_mut() {
        // Nested `<uuid>/subagents/agent-*.jsonl` files are subagent transcripts.
        t.is_subagent = external_id.contains("/subagents/");
    }
    let tag = cursor_tag(
        &parsed.fingerprint,
        thread.as_ref().map_or(0, |t| t.messages.len()),
    );
    // Sessions the app itself ran through `claude -p` (distill / Ask on older builds that
    // persisted them) are not the user's history.
    if thread.as_ref().is_some_and(is_app_session) {
        write_cursor(
            conn,
            &path_str,
            mtime,
            size,
            parsed.offset as i64,
            Some(&tag),
        )?;
        return Ok(None);
    }
    let n = if let Some(t) = thread {
        upsert_thread_at(conn, sid, &t, &parsed.cwds)?
    } else {
        0
    };
    write_cursor(
        conn,
        &path_str,
        mtime,
        size,
        parsed.offset as i64,
        Some(&tag),
    )?;
    Ok(Some(n))
}

fn is_app_session(t: &ParsedThread) -> bool {
    crate::agent::is_app_cli_session(
        t.project_path.as_deref(),
        t.messages
            .iter()
            .filter(|m| m.role == "user")
            .take(3)
            .map(|m| m.text.as_str()),
    )
}

/// Decode a project slug (`-Users-me-proj`) back to an approximate cwd
/// (`/Users/me/proj`). Lossy when real directory names contain hyphens; the `cwd`
/// field in the file is authoritative when present.
fn decode_slug(slug: &str) -> String {
    slug.replace('-', "/")
}

// ---------------------------------------------------------------------------------------
// Incremental (append-only) re-index
// ---------------------------------------------------------------------------------------

/// Per-file indexing cursor, stored in `index_state`.
struct FileCursor {
    mtime: i64,
    size: i64,
    /// Bytes of the file already folded into the index (end of the last complete line).
    offset: i64,
    /// [`cursor_tag`]: [`fingerprint_file`] at `offset` plus the message count stored then.
    fingerprint: Option<String>,
}

fn read_cursor(conn: &Connection, path: &str) -> Result<Option<FileCursor>> {
    Ok(conn
        .query_row(
            "SELECT mtime, size, last_offset, hash FROM index_state WHERE path = ?1",
            [path],
            |r| {
                Ok(FileCursor {
                    mtime: r.get::<_, Option<i64>>(0)?.unwrap_or(-1),
                    size: r.get::<_, Option<i64>>(1)?.unwrap_or(-1),
                    offset: r.get(2)?,
                    fingerprint: r.get(3)?,
                })
            },
        )
        .optional()?)
}

fn write_cursor(
    conn: &Connection,
    path: &str,
    mtime: i64,
    size: i64,
    offset: i64,
    fingerprint: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO index_state (path, source_kind, mtime, size, last_offset, hash, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (path) DO UPDATE SET
            mtime = excluded.mtime, size = excluded.size, last_offset = excluded.last_offset,
            hash = excluded.hash, updated_at = excluded.updated_at",
        params![
            path,
            KIND,
            mtime,
            size,
            offset,
            fingerprint,
            chrono::Utc::now().timestamp()
        ],
    )?;
    Ok(())
}

/// Bytes hashed at each end of the indexed region to detect a rewritten file.
const FP_WINDOW: usize = 4096;

/// FNV-1a 64: tiny, dependency-free, and stable across Rust releases (std's hasher is
/// not guaranteed to be, and these values are persisted).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Fingerprint of the first and last [`FP_WINDOW`] bytes before `offset`. If either end
/// changed, the file was rewritten rather than appended to.
fn fingerprint_bytes(content: &[u8], offset: usize) -> String {
    let offset = offset.min(content.len());
    let head = &content[..offset.min(FP_WINDOW)];
    let tail = &content[offset.saturating_sub(FP_WINDOW)..offset];
    format!("{:016x}{:016x}", fnv1a(head), fnv1a(tail))
}

/// [`fingerprint_bytes`] computed from the file on disk, reading only the two windows.
fn fingerprint_file(file: &mut fs::File, offset: u64) -> std::io::Result<String> {
    let window = offset.min(FP_WINDOW as u64) as usize;
    let mut head = vec![0u8; window];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut head)?;
    let mut tail = vec![0u8; window];
    file.seek(SeekFrom::Start(offset - window as u64))?;
    file.read_exact(&mut tail)?;
    Ok(format!("{:016x}{:016x}", fnv1a(&head), fnv1a(&tail)))
}

enum Append {
    /// The new lines were indexed; carries the number of message rows inserted.
    Applied(usize),
    /// Only non-message lines were appended; the cursor advanced, the thread is unchanged.
    NoChange,
    /// Not a clean append (or no stored thread to append to): do a full parse.
    Fallback,
}

/// The append fast path. Proof that the file only grew comes from the cursor alone: its
/// byte fingerprint still matches and the stored thread still has the message count the
/// cursor recorded. Given that, only the threads row is read (never a stored message), the
/// tail is parsed onto it, and `indexer::append_messages` inserts just the tail rows. On a
/// 17k-message session the old path read every stored text twice (once to rebuild the
/// thread, once under the write lock to re-check the prefix), so a 1-line append cost
/// 30-40 ms and a progress line that changed nothing still cost 25 ms.
#[allow(clippy::too_many_arguments)] // all distinct; this is the one internal call site
fn try_append(
    conn: &mut Connection,
    sid: i64,
    path: &Path,
    path_str: &str,
    external_id: &str,
    cursor: &FileCursor,
    mtime: i64,
    size: i64,
) -> Result<Append> {
    let Some((expected_fp, expected_count)) = cursor.fingerprint.as_deref().and_then(split_tag)
    else {
        return Ok(Append::Fallback);
    };
    let offset = cursor.offset as u64;
    let mut file = fs::File::open(path)?;
    if fingerprint_file(&mut file, offset)? != expected_fp {
        return Ok(Append::Fallback); // rewritten in place, not appended to
    }
    let Some((mut thread, stored_count)) = load_thread_meta(conn, sid, external_id)? else {
        return Ok(Append::Fallback);
    };
    // Outside a full scan's batch (the watcher path) the cursor and the write are separate
    // commits. If the last pass stored the new messages but died (or lost a lock race)
    // before advancing the cursor, re-reading from the stale offset would append the same
    // lines twice; the count catches that.
    if stored_count != expected_count {
        return Ok(Append::Fallback);
    }

    let mut tail = Vec::with_capacity((size as u64).saturating_sub(offset) as usize);
    file.seek(SeekFrom::Start(offset))?;
    file.read_to_end(&mut tail)?;

    let before = (
        thread.title.clone(),
        thread.project_path.clone(),
        thread.git_branch.clone(),
    );
    // `thread` holds the stored metadata and no messages, so what ingest pushes (and the
    // usage indices it records) is exactly the tail. Until the tail names a cwd, its calls
    // ran where the stored thread last was.
    let mut first_user_text = None;
    let mut cwds: Vec<(usize, String)> = before.1.iter().map(|c| (0, c.clone())).collect();
    let consumed = ingest_jsonl(&mut thread, &tail, &mut first_user_text, &mut cwds);
    finish_title(&mut thread, first_user_text);
    let new_offset = offset + consumed as u64;
    let tag = cursor_tag(
        &fingerprint_file(&mut file, new_offset)?,
        stored_count + thread.messages.len(),
    );

    // Write only when something the index stores changed. A lone timestamp bump (a
    // progress line) isn't worth a threads-row rewrite.
    let changed = !thread.messages.is_empty()
        || thread.title != before.0
        || thread.project_path != before.1
        || thread.git_branch != before.2;
    let result = if changed {
        match append_messages_at(conn, sid, &thread, stored_count, &cwds)? {
            Some(n) => Append::Applied(n),
            // Another writer moved the thread since the check above.
            None => return Ok(Append::Fallback),
        }
    } else {
        Append::NoChange
    };
    write_cursor(conn, path_str, mtime, size, new_offset as i64, Some(&tag))?;
    Ok(result)
}

/// The cursor's `hash` value: the byte fingerprint plus the thread's message count at
/// that offset, as `<fingerprint>:<count>`.
fn cursor_tag(fingerprint: &str, messages: usize) -> String {
    format!("{fingerprint}:{messages}")
}

fn split_tag(tag: &str) -> Option<(&str, usize)> {
    let (fp, count) = tag.split_once(':')?;
    Some((fp, count.parse().ok()?))
}

/// A stored thread's metadata (no messages) plus its stored message count, so new lines
/// can be folded onto it exactly as a full parse would. None when the thread isn't stored.
fn load_thread_meta(
    conn: &Connection,
    sid: i64,
    external_id: &str,
) -> Result<Option<(ParsedThread, usize)>> {
    Ok(conn
        .query_row(
            "SELECT title, project_path, git_branch, created_at, updated_at, message_count,
                    is_subagent
             FROM threads WHERE source_id = ?1 AND external_id = ?2",
            params![sid, external_id],
            |r| {
                let thread = ParsedThread {
                    external_id: external_id.to_string(),
                    title: r.get(0)?,
                    project_path: r.get(1)?,
                    git_branch: r.get(2)?,
                    created_at: r.get(3)?,
                    updated_at: r.get(4)?,
                    is_subagent: r.get::<_, i64>(6)? != 0,
                    messages: Vec::new(),
                    usage: Vec::new(),
                };
                Ok((thread, r.get::<_, i64>(5)?))
            },
        )
        .optional()?
        .and_then(|(t, n)| usize::try_from(n).ok().map(|n| (t, n))))
}

// ---------------------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------------------

/// A full-file parse plus the cursor it leaves behind.
struct FullParse {
    thread: Option<ParsedThread>,
    offset: usize,
    fingerprint: String,
    /// `(first message index, cwd)` at each change of the lines' `cwd` (see
    /// `indexer::upsert_thread_at`): a session that `cd`s or enters a worktree runs its later
    /// calls somewhere else than its first ones.
    cwds: Vec<(usize, String)>,
}

/// Parse a `.jsonl` thread file. `external_id` keys the thread; `project_fallback`
/// is used when no `cwd` appears in the file. Returns None if it has no messages.
#[cfg(test)]
pub fn parse_file(
    path: &Path,
    external_id: &str,
    project_fallback: Option<&str>,
) -> Result<Option<ParsedThread>> {
    Ok(parse_full(path, external_id, project_fallback)?.thread)
}

fn parse_full(path: &Path, external_id: &str, project_fallback: Option<&str>) -> Result<FullParse> {
    let content = fs::read(path)?;
    let mut thread = ParsedThread {
        external_id: external_id.to_string(),
        project_path: project_fallback.map(str::to_string),
        ..Default::default()
    };
    let mut first_user_text: Option<String> = None;
    let mut cwds = Vec::new();
    let offset = ingest_jsonl(&mut thread, &content, &mut first_user_text, &mut cwds);
    let fingerprint = fingerprint_bytes(&content, offset);
    if thread.messages.is_empty() {
        return Ok(FullParse {
            thread: None,
            offset,
            fingerprint,
            cwds,
        });
    }
    finish_title(&mut thread, first_user_text);
    Ok(FullParse {
        thread: Some(thread),
        offset,
        fingerprint,
        cwds,
    })
}

/// Fold every complete JSONL line in `bytes` into `thread`; returns how many bytes were
/// consumed (the new cursor advance). An unterminated last line is consumed only if it is
/// already a complete JSON object: a writer mid-append leaves a fragment that must be
/// re-read once the rest of the line lands. Malformed complete lines are skipped.
fn ingest_jsonl(
    thread: &mut ParsedThread,
    bytes: &[u8],
    first_user_text: &mut Option<String>,
    cwds: &mut Vec<(usize, String)>,
) -> usize {
    let mut pos = 0;
    let mut consumed = 0;
    while pos < bytes.len() {
        match bytes[pos..].iter().position(|&b| b == b'\n') {
            Some(len) => {
                if let Some(obj) = parse_line(&bytes[pos..pos + len]) {
                    ingest_line(thread, &obj, first_user_text, cwds);
                }
                pos += len + 1;
                consumed = pos;
            }
            None => {
                let fragment = &bytes[pos..];
                if let Some(obj) = std::str::from_utf8(fragment)
                    .ok()
                    .and_then(|s| serde_json::from_str::<Value>(s.trim()).ok())
                    .filter(Value::is_object)
                {
                    ingest_line(thread, &obj, first_user_text, cwds);
                    consumed = bytes.len();
                }
                break;
            }
        }
    }
    consumed
}

fn parse_line(line: &[u8]) -> Option<Value> {
    let text = String::from_utf8_lossy(line);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    // Tolerate the occasional malformed line.
    serde_json::from_str::<Value>(text).ok()
}

/// Title fallback: first user message, trimmed to a reasonable length.
fn finish_title(thread: &mut ParsedThread, first_user_text: Option<String>) {
    if thread.title.is_none() {
        thread.title = first_user_text.as_deref().map(truncate_title);
    }
}

/// Fold a single JSONL line into the thread under construction, recording in `cwds` where
/// the line's messages ran when its `cwd` differs from the last one recorded.
fn ingest_line(
    thread: &mut ParsedThread,
    obj: &Value,
    first_user_text: &mut Option<String>,
    cwds: &mut Vec<(usize, String)>,
) {
    // A real cwd in the file overrides the slug-derived fallback.
    if let Some(cwd) = obj.get("cwd").and_then(Value::as_str) {
        if !cwd.is_empty() {
            thread.project_path = Some(cwd.to_string());
            if cwds.last().is_none_or(|(_, c)| c != cwd) {
                cwds.push((thread.messages.len(), cwd.to_string()));
            }
        }
    }
    if let Some(branch) = obj.get("gitBranch").and_then(Value::as_str) {
        if !branch.is_empty() {
            thread.git_branch = Some(branch.to_string());
        }
    }
    let ts = obj
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339);
    if let Some(ts) = ts {
        thread.created_at = Some(thread.created_at.map_or(ts, |c| c.min(ts)));
        thread.updated_at = Some(thread.updated_at.map_or(ts, |u| u.max(ts)));
    }

    match obj.get("type").and_then(Value::as_str) {
        Some("ai-title") => {
            if let Some(t) = obj.get("aiTitle").and_then(Value::as_str) {
                thread.title = Some(t.to_string());
            }
        }
        Some(role @ ("user" | "assistant")) => {
            let message = obj.get("message");
            let content = message.and_then(|m| m.get("content"));
            let before = thread.messages.len();
            extract_messages(thread, role, content, ts);
            // Attach the assistant turn's token usage to its first message (cost is per-turn).
            if role == "assistant" && before < thread.messages.len() {
                if let Some(u) = message.and_then(parse_usage) {
                    thread.usage.push((before, u));
                }
            }
            if role == "user" && first_user_text.is_none() {
                if let Some(m) = thread.messages.get(before) {
                    if m.role == "user" {
                        *first_user_text = Some(m.text.clone());
                    }
                }
            }
        }
        _ => {}
    }
}

/// Extract model + token usage from a Claude `message` object (assistant turns carry a `usage`
/// block: input/output/cache-creation/cache-read token counts and the model id).
fn parse_usage(message: &Value) -> Option<MsgUsage> {
    let model = message.get("model").and_then(Value::as_str)?.to_string();
    let u = message.get("usage")?;
    let n = |k: &str| u.get(k).and_then(Value::as_i64).unwrap_or(0);
    Some(MsgUsage {
        model,
        input: n("input_tokens"),
        output: n("output_tokens"),
        cache_write: n("cache_creation_input_tokens"),
        cache_read: n("cache_read_input_tokens"),
    })
}

/// Turn a message's `content` (string or array of blocks) into ParsedMessages.
fn extract_messages(
    thread: &mut ParsedThread,
    role: &str,
    content: Option<&Value>,
    ts: Option<i64>,
) {
    let out = &mut thread.messages;
    match content {
        Some(Value::String(s)) => push_message(out, role, s, None, ts),
        Some(Value::Array(blocks)) => {
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = block.get("text").and_then(Value::as_str) {
                            push_message(out, role, t, None, ts);
                        }
                    }
                    Some("tool_use") => {
                        let name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("tool")
                            .to_string();
                        let input = block
                            .get("input")
                            .map(|v| v.to_string())
                            .unwrap_or_default();
                        // Capped like tool output: a Write or StructuredOutput call carries
                        // the whole payload (3,412 rows over the cap held 91 MB on a real
                        // index), and these rows are FTS-only, never embedded or distilled
                        // in full.
                        let text = cap_tool_text(format!("{name}: {input}").trim());
                        push_message(out, "assistant", &text, Some(name), ts);
                    }
                    Some("tool_result") => {
                        // tool_result content is a plain string or an array of text blocks.
                        let text = join_text_parts(block.get("content"));
                        push_message(out, "tool", &cap_tool_text(text.trim()), None, ts);
                    }
                    _ => {} // thinking, image, etc. — skipped for now
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::super::{append_messages, upsert_thread, ParsedMessage};
    use super::*;
    use std::io::Write;

    #[test]
    fn parse_usage_reads_model_and_tokens() {
        // The real shape of a Claude Code assistant `message` object.
        let msg: Value = serde_json::from_str(
            r#"{"model":"claude-opus-4-8","usage":{"input_tokens":17276,"output_tokens":797,
                "cache_creation_input_tokens":3139,"cache_read_input_tokens":15853}}"#,
        )
        .unwrap();
        let u = parse_usage(&msg).expect("usage parsed");
        assert_eq!(u.model, "claude-opus-4-8");
        assert_eq!(u.input, 17276);
        assert_eq!(u.output, 797);
        assert_eq!(u.cache_write, 3139);
        assert_eq!(u.cache_read, 15853);
        // A message with no usage block (e.g. a user turn) yields None.
        assert!(parse_usage(&serde_json::json!({"role": "user"})).is_none());
    }

    fn temp_path(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("callimachus_{name}_{}_{n}", std::process::id()))
    }

    const SAMPLE: &str = r#"{"type":"ai-title","aiTitle":"Build the indexer","sessionId":"sess-abc"}
{"type":"user","sessionId":"sess-abc","cwd":"/Users/me/proj","gitBranch":"main","timestamp":"2026-06-01T10:00:00.000Z","message":{"role":"user","content":[{"type":"text","text":"index tauri threads with sqlite fts5"}]}}
{"type":"assistant","sessionId":"sess-abc","timestamp":"2026-06-01T10:00:05.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"Sure, using FTS5"},{"type":"tool_use","name":"Bash","input":{"command":"cargo build"}}]}}
{"type":"user","sessionId":"sess-abc","timestamp":"2026-06-01T10:00:06.000Z","message":{"role":"user","content":[{"type":"tool_result","content":"Finished dev profile"}]}}
"#;

    fn write_sample(name: &str) -> PathBuf {
        let path = temp_path(name);
        std::fs::File::create(&path)
            .unwrap()
            .write_all(SAMPLE.as_bytes())
            .unwrap();
        path
    }

    #[test]
    fn parses_sample_thread() {
        let path = write_sample("sample.jsonl");
        let thread = parse_file(&path, "slug/sess-abc.jsonl", Some("/slug"))
            .unwrap()
            .expect("non-empty thread");
        assert_eq!(thread.external_id, "slug/sess-abc.jsonl");
        assert_eq!(thread.title.as_deref(), Some("Build the indexer"));
        // cwd in the file overrides the slug fallback
        assert_eq!(thread.project_path.as_deref(), Some("/Users/me/proj"));
        assert_eq!(thread.git_branch.as_deref(), Some("main"));
        // user text, assistant text, assistant tool_use, tool result = 4 (thinking skipped)
        assert_eq!(thread.messages.len(), 4);
        let tool = thread
            .messages
            .iter()
            .find(|m| m.tool_name.is_some())
            .unwrap();
        assert_eq!(tool.tool_name.as_deref(), Some("Bash"));
        assert!(tool.text.contains("cargo build"));
    }

    #[test]
    fn title_falls_back_to_first_user_message() {
        // No ai-title line -> title derived from first user text.
        let path = temp_path("notitle.jsonl");
        std::fs::write(
            &path,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hello world question"}]}}
"#,
        )
        .unwrap();
        let thread = parse_file(&path, "x.jsonl", None).unwrap().unwrap();
        assert_eq!(thread.title.as_deref(), Some("hello world question"));
    }

    #[test]
    fn index_then_search_roundtrip() {
        let path = write_sample("rt.jsonl");
        let mut conn = crate::db::open(&temp_path("rt.db")).unwrap();
        let sid = source_id(&conn, KIND).unwrap();
        let thread = parse_file(&path, "slug/rt.jsonl", None).unwrap().unwrap();
        upsert_thread(&mut conn, sid, &thread).unwrap();

        // "fts5" appears in the user message and in "Sure, using FTS5" (case-insensitive).
        let hits =
            crate::search::search(&conn, "fts5", &crate::search::SearchFilters::default()).unwrap();
        assert_eq!(hits.len(), 2);
        // snippet() marks matches with the char(1) sentinel (frontend swaps it for <mark>).
        assert!(hits.iter().all(|h| h.snippet.contains('\u{1}')));
        assert!(hits.iter().all(|h| h.source == "claude_code"));

        // Re-indexing the same thread is idempotent (no duplicate messages).
        upsert_thread(&mut conn, sid, &thread).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 4, "re-upsert replaces rather than appends");
    }

    /// Real-data smoke test against the live ~/.claude history. Ignored by default;
    /// run with: `cargo test -- --ignored real_claude_index --nocapture`
    #[test]
    #[ignore]
    fn real_claude_index() {
        let mut conn = crate::db::open(&temp_path("real.db")).unwrap();
        let report = scan(&mut conn, &mut || {}).unwrap();
        eprintln!("{report:?}");
        assert!(
            report.threads_indexed > 0,
            "indexed at least one real thread"
        );

        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert!(n > 0, "indexed real messages");

        let hits = crate::search::search(&conn, "tauri", &crate::search::SearchFilters::default())
            .unwrap();
        eprintln!("'tauri' hits: {}", hits.len());
    }

    // ---- tool output cap ---------------------------------------------------------------

    #[test]
    fn cap_tool_text_keeps_head_and_tail_and_is_idempotent() {
        assert_eq!(cap_tool_text("short output"), "short output");

        let long = format!("HEAD-START {} TAIL-END", "x".repeat(50_000));
        let capped = cap_tool_text(&long);
        assert!(
            capped.chars().count() <= TOOL_TEXT_CAP_CHARS,
            "within the cap"
        );
        assert!(capped.starts_with("HEAD-START"));
        assert!(capped.ends_with("TAIL-END"), "errors at the end survive");
        assert!(capped.contains("chars of tool output elided"));
        assert_eq!(
            cap_tool_text(&capped),
            capped,
            "capping twice changes nothing"
        );

        // Multi-byte text is cut on char boundaries (would panic otherwise).
        let wide = "━".repeat(30_000);
        let capped = cap_tool_text(&wide);
        assert!(capped.chars().count() <= TOOL_TEXT_CAP_CHARS);
        assert_eq!(cap_tool_text(&capped), capped);
    }

    #[test]
    fn tool_results_are_capped_at_parse_time() {
        let big = "line of build output\n".repeat(5_000);
        let line = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": [{"type": "tool_result", "content": big}]}
        });
        let path = temp_path("bigtool.jsonl");
        std::fs::write(&path, format!("{line}\n")).unwrap();
        let t = parse_file(&path, "x.jsonl", None).unwrap().unwrap();
        assert_eq!(t.messages.len(), 1);
        assert!(t.messages[0].text.chars().count() <= TOOL_TEXT_CAP_CHARS);
    }

    #[test]
    fn tool_calls_are_capped_at_parse_time() {
        let content = "fn main() {}\n".repeat(5_000);
        let line = serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "name": "Write",
                 "input": {"file_path": "/p/src/main.rs", "content": content}},
                {"type": "tool_use", "name": "Bash", "input": {"command": "ls"}},
            ]}
        });
        let path = temp_path("bigcall.jsonl");
        std::fs::write(&path, format!("{line}\n")).unwrap();
        let t = parse_file(&path, "x.jsonl", None).unwrap().unwrap();
        assert_eq!(t.messages.len(), 2);
        let write = &t.messages[0];
        assert_eq!(write.tool_name.as_deref(), Some("Write"));
        assert!(write.text.starts_with("Write: {"), "head keeps the call");
        assert!(write.text.chars().count() <= TOOL_TEXT_CAP_CHARS);
        assert_eq!(cap_tool_text(&write.text), write.text, "idempotent");
        assert_eq!(
            t.messages[1].text, r#"Bash: {"command":"ls"}"#,
            "small calls as-is"
        );
    }

    #[test]
    fn external_ids_always_use_forward_slashes() {
        let root = Path::new("/r");
        let p = root
            .join("slug")
            .join("sess")
            .join("subagents")
            .join("agent-x.jsonl");
        assert_eq!(
            external_id_for(root, &p),
            "slug/sess/subagents/agent-x.jsonl"
        );
    }

    // ---- incremental re-index ----------------------------------------------------------

    fn user_line(text: &str) -> String {
        serde_json::json!({
            "type": "user",
            "cwd": "/Users/me/proj",
            "timestamp": "2026-06-01T10:00:00.000Z",
            "message": {"role": "user", "content": text},
        })
        .to_string()
    }

    struct Fixture {
        root: PathBuf,
        path: PathBuf,
        conn: Connection,
        sid: i64,
    }

    fn fixture(lines: &[String]) -> Fixture {
        let root = temp_path("projects");
        std::fs::create_dir_all(root.join("-Users-me-proj")).unwrap();
        let path = root.join("-Users-me-proj").join("sess.jsonl");
        let mut body = lines.join("\n");
        body.push('\n');
        std::fs::write(&path, body).unwrap();
        let conn = crate::db::open(&temp_path("incr.db")).unwrap();
        let sid = source_id(&conn, KIND).unwrap();
        Fixture {
            root,
            path,
            conn,
            sid,
        }
    }

    fn append(path: &Path, s: &str) {
        std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(s.as_bytes())
            .unwrap();
    }

    fn texts(conn: &Connection) -> Vec<String> {
        conn.prepare("SELECT text FROM messages ORDER BY thread_id, seq")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn cursor_offset(conn: &Connection, path: &Path) -> i64 {
        read_cursor(conn, &path.to_string_lossy())
            .unwrap()
            .unwrap()
            .offset
    }

    #[test]
    fn tool_calls_resolve_against_the_cwd_of_their_line() {
        let repo = temp_path("cwd_repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("apps/web")).unwrap();
        let repo = std::fs::canonicalize(&repo).unwrap();
        let web = repo.join("apps/web");
        let bash = |cwd: &Path, cmd: &str| {
            serde_json::json!({
                "type": "assistant",
                "cwd": cwd.to_string_lossy(),
                "timestamp": "2026-06-01T10:00:01.000Z",
                "message": {"role": "assistant", "content": [
                    {"type": "tool_use", "name": "Bash", "input": {"command": cmd}}
                ]},
            })
            .to_string()
        };
        let mut f = fixture(&[
            bash(&repo, "cat src/a.rs"),
            bash(&web, "cat src/b.rs"),
            bash(&repo, "cat src/c.rs"),
        ]);
        index_file(&mut f.conn, f.sid, &f.root, &f.path).unwrap();
        // The append path: the tail's first line still runs in the stored thread's last cwd
        // until it names another.
        append(
            &f.path,
            &format!(
                "{}\n{}\n",
                bash(&web, "cat src/d.rs"),
                bash(&web, "cat e.rs")
            ),
        );
        index_file(&mut f.conn, f.sid, &f.root, &f.path).unwrap();
        let paths: Vec<String> = f
            .conn
            .prepare("SELECT path FROM file_edits ORDER BY rowid")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            paths,
            vec![
                "src/a.rs",
                "apps/web/src/b.rs",
                "src/c.rs",
                "apps/web/src/d.rs",
                "apps/web/e.rs"
            ]
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn appended_lines_are_parsed_from_the_cursor() {
        // > 2 fingerprint windows of prefix, so a mid-file edit is outside both windows.
        let lines: Vec<String> = (0..200)
            .map(|i| user_line(&format!("message {i:03} {}", "a".repeat(60))))
            .collect();
        let mut f = fixture(&lines);
        assert_eq!(
            index_file(&mut f.conn, f.sid, &f.root, &f.path).unwrap(),
            Some(200)
        );
        assert_eq!(
            index_file(&mut f.conn, f.sid, &f.root, &f.path).unwrap(),
            None,
            "unchanged file is skipped"
        );

        // Edit a line in the middle WITHOUT changing its length (outside both windows),
        // then append. The fast path must not re-read the prefix, so the stored text keeps
        // the original wording: proof that only the appended bytes were parsed.
        let body = std::fs::read_to_string(&f.path).unwrap();
        let edited = body.replacen("message 100 aaaa", "message 100 bbbb", 1);
        assert_eq!(edited.len(), body.len());
        std::fs::write(&f.path, edited).unwrap();
        append(&f.path, &format!("{}\n", user_line("brand new question")));
        assert_eq!(
            index_file(&mut f.conn, f.sid, &f.root, &f.path).unwrap(),
            Some(1),
            "reports the rows written, not the thread size"
        );
        let t = texts(&f.conn);
        assert_eq!(t.len(), 201);
        assert!(
            t[100].starts_with("message 100 aaaa"),
            "prefix not re-parsed"
        );
        assert_eq!(t[200], "brand new question");

        // A half-written line is left for later; the cursor stops before it.
        let full_len = std::fs::metadata(&f.path).unwrap().len() as i64;
        let line = user_line("finished later");
        let (first, rest) = line.split_at(line.len() / 2);
        append(&f.path, first);
        assert_eq!(
            index_file(&mut f.conn, f.sid, &f.root, &f.path).unwrap(),
            None,
            "a fragment adds nothing yet"
        );
        assert_eq!(cursor_offset(&f.conn, &f.path), full_len);
        append(&f.path, &format!("{rest}\n"));
        assert_eq!(
            index_file(&mut f.conn, f.sid, &f.root, &f.path).unwrap(),
            Some(1)
        );
        assert_eq!(texts(&f.conn).last().unwrap(), "finished later");
        let (count, bytes, distillable): (i64, i64, i64) = f
            .conn
            .query_row(
                "SELECT message_count, bytes, distillable_count FROM threads",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((count, distillable), (202, 202));
        let stored_bytes: i64 = f
            .conn
            .query_row(
                "SELECT SUM(length(CAST(text AS BLOB))) FROM messages",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bytes, stored_bytes, "the appended tail's bytes are added");
    }

    #[test]
    fn a_cursor_left_behind_by_a_failed_pass_never_duplicates_messages() {
        let mut f = fixture(&[user_line("one"), user_line("two")]);
        index_file(&mut f.conn, f.sid, &f.root, &f.path).unwrap();
        let path_str = f.path.to_string_lossy().to_string();
        let stale = read_cursor(&f.conn, &path_str).unwrap().unwrap();

        append(&f.path, &format!("{}\n", user_line("three")));
        index_file(&mut f.conn, f.sid, &f.root, &f.path).unwrap();
        // Simulate the upsert committing but the cursor write being lost.
        write_cursor(
            &f.conn,
            &path_str,
            stale.mtime,
            stale.size,
            stale.offset,
            stale.fingerprint.as_deref(),
        )
        .unwrap();
        append(&f.path, &format!("{}\n", user_line("four")));
        index_file(&mut f.conn, f.sid, &f.root, &f.path).unwrap();
        assert_eq!(texts(&f.conn), vec!["one", "two", "three", "four"]);
    }

    #[test]
    fn rewritten_or_truncated_files_fall_back_to_a_full_parse() {
        let mut f = fixture(&[user_line("first version"), user_line("second")]);
        index_file(&mut f.conn, f.sid, &f.root, &f.path).unwrap();

        // Same leading bytes are gone: the head fingerprint no longer matches.
        let body = format!(
            "{}\n{}\n{}\n",
            user_line("FIRST VERSION"),
            user_line("second"),
            user_line("third")
        );
        std::fs::write(&f.path, body).unwrap();
        index_file(&mut f.conn, f.sid, &f.root, &f.path).unwrap();
        assert_eq!(texts(&f.conn), vec!["FIRST VERSION", "second", "third"]);

        // Shrunk below the cursor: full parse again.
        std::fs::write(&f.path, format!("{}\n", user_line("only one"))).unwrap();
        index_file(&mut f.conn, f.sid, &f.root, &f.path).unwrap();
        assert_eq!(texts(&f.conn), vec!["only one"]);
    }

    fn assistant_line(text: &str, input_tokens: i64) -> String {
        serde_json::json!({
            "type": "assistant",
            "cwd": "/Users/me/proj",
            "timestamp": "2026-06-01T10:00:01.000Z",
            "message": {
                "role": "assistant",
                "model": "claude-opus-4-8",
                "content": [{"type": "text", "text": text}],
                "usage": {"input_tokens": input_tokens, "output_tokens": 5,
                          "cache_creation_input_tokens": 2, "cache_read_input_tokens": 3},
            },
        })
        .to_string()
    }

    fn usage_at(conn: &Connection, seq: i64) -> Option<String> {
        conn.query_row(
            "SELECT model || ' ' || input_tokens || ' ' || output_tokens || ' '
                    || cache_write_tokens || ' ' || cache_read_tokens
             FROM messages WHERE seq = ?1",
            [seq],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// The append path parses only the tail, so its usage indices are tail-relative: the
    /// write must land them on the right absolute seq and keep the earlier turns' usage.
    #[test]
    fn appended_turns_keep_usage_on_the_right_rows() {
        let mut f = fixture(&[user_line("question"), assistant_line("the answer", 10)]);
        index_file(&mut f.conn, f.sid, &f.root, &f.path).unwrap();
        append(
            &f.path,
            &format!(
                "{}\n{}\n",
                user_line("follow-up"),
                assistant_line("more", 20)
            ),
        );
        assert_eq!(
            index_file(&mut f.conn, f.sid, &f.root, &f.path).unwrap(),
            Some(2)
        );
        assert_eq!(
            usage_at(&f.conn, 1).as_deref(),
            Some("claude-opus-4-8 10 5 2 3")
        );
        assert_eq!(usage_at(&f.conn, 2), None);
        assert_eq!(
            usage_at(&f.conn, 3).as_deref(),
            Some("claude-opus-4-8 20 5 2 3")
        );
    }

    /// If another writer changed the stored thread between the cursor check and the write
    /// (here: a stored row went missing), the append backs off and a full parse rebuilds the
    /// thread from the file, usage included.
    #[test]
    fn an_append_that_loses_a_race_falls_back_to_a_full_parse() {
        let mut f = fixture(&[user_line("question"), assistant_line("the answer", 10)]);
        index_file(&mut f.conn, f.sid, &f.root, &f.path).unwrap();
        let tid: i64 = f
            .conn
            .query_row("SELECT id FROM threads", [], |r| r.get(0))
            .unwrap();
        // The rows no longer add up to the stored count the cursor agrees with.
        f.conn
            .execute("DELETE FROM messages WHERE seq = 1", [])
            .unwrap();
        let mut thread = ParsedThread {
            external_id: external_id_for(&f.root, &f.path),
            ..Default::default()
        };
        thread.messages.push(ParsedMessage {
            role: "user".into(),
            text: "x".into(),
            tool_name: None,
            ts: None,
        });
        assert_eq!(
            append_messages(&mut f.conn, f.sid, &thread, 2).unwrap(),
            None,
            "seq gap detected under the lock"
        );
        assert_eq!(
            append_messages(&mut f.conn, f.sid, &thread, 1).unwrap(),
            None,
            "a start that disagrees with the stored count is refused"
        );

        append(&f.path, &format!("{}\n", user_line("follow-up")));
        index_file(&mut f.conn, f.sid, &f.root, &f.path).unwrap();
        assert_eq!(texts(&f.conn), vec!["question", "the answer", "follow-up"]);
        assert_eq!(
            usage_at(&f.conn, 1).as_deref(),
            Some("claude-opus-4-8 10 5 2 3")
        );
        let count: i64 = f
            .conn
            .query_row(
                "SELECT message_count FROM threads WHERE id = ?1",
                [tid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 3);
    }

    /// A progress line (no message) must not touch the thread, and an append must never
    /// read the stored message text: blank every stored text and the append still lands.
    #[test]
    fn the_append_path_never_reads_stored_messages() {
        let mut f = fixture(&[user_line("one"), user_line("two")]);
        index_file(&mut f.conn, f.sid, &f.root, &f.path).unwrap();
        f.conn
            .execute("UPDATE messages SET text = 'scrubbed'", [])
            .unwrap();
        append(
            &f.path,
            "{\"type\":\"progress\",\"timestamp\":\"2026-06-01T10:00:09Z\"}\n",
        );
        assert_eq!(
            index_file(&mut f.conn, f.sid, &f.root, &f.path).unwrap(),
            None
        );
        append(&f.path, &format!("{}\n", user_line("three")));
        assert_eq!(
            index_file(&mut f.conn, f.sid, &f.root, &f.path).unwrap(),
            Some(1)
        );
        assert_eq!(texts(&f.conn), vec!["scrubbed", "scrubbed", "three"]);
    }

    #[test]
    fn deleted_threads_are_not_resurrected() {
        let mut f = fixture(&[user_line("delete me")]);
        index_file(&mut f.conn, f.sid, &f.root, &f.path).unwrap();
        let tid: i64 = f
            .conn
            .query_row("SELECT id FROM threads", [], |r| r.get(0))
            .unwrap();
        crate::cleanup::delete_threads(&mut f.conn, &[tid]).unwrap();

        append(&f.path, &format!("{}\n", user_line("the session went on")));
        assert_eq!(
            index_file(&mut f.conn, f.sid, &f.root, &f.path).unwrap(),
            None
        );
        let n: i64 = f
            .conn
            .query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "tombstoned thread stays deleted");
    }

    #[test]
    fn the_apps_own_cli_sessions_are_not_indexed() {
        let work = crate::agent::cli_work_dir_path();
        let line = serde_json::json!({
            "type": "user",
            "cwd": work.to_string_lossy(),
            "message": {"role": "user", "content": "anything at all"},
        });
        let mut f = fixture(&[line.to_string()]);
        assert_eq!(
            index_file(&mut f.conn, f.sid, &f.root, &f.path).unwrap(),
            None
        );
        let n: i64 = f
            .conn
            .query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    // ---- batched full scan ---------------------------------------------------------------

    /// `n` small transcripts under a fresh projects root, plus one over BATCH_MAX_FILE (a
    /// huge tool result, capped at parse time, so the index stays small).
    fn scan_fixture(n: usize) -> (PathBuf, Vec<PathBuf>) {
        let root = temp_path("scanroot");
        let dir = root.join("-Users-me-proj");
        std::fs::create_dir_all(&dir).unwrap();
        let mut files: Vec<PathBuf> = (0..n)
            .map(|i| {
                let p = dir.join(format!("s{i:02}.jsonl"));
                std::fs::write(&p, format!("{}\n", user_line(&format!("question {i}")))).unwrap();
                p
            })
            .collect();
        let big = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "content": "x".repeat(BATCH_MAX_FILE as usize)}
            ]}
        });
        let p = dir.join("big.jsonl");
        std::fs::write(&p, format!("{}\n{big}\n", user_line("big one"))).unwrap();
        files.push(p);
        (root, files)
    }

    #[test]
    fn a_full_scan_indexes_every_file_through_batches() {
        // More files than one batch holds, so at least one mid-scan commit happens.
        let (root, files) = scan_fixture(BATCH_FILES + 8);
        // An unreadable file fails alone: its savepoint rolls back, the batch carries on.
        let bad = root.join("-Users-me-proj").join("unreadable.jsonl");
        std::fs::write(&bad, format!("{}\n", user_line("locked away"))).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o000)).unwrap();
        }
        // Root (some CI containers) reads it anyway; then it is just one more good file.
        let bad_fails = std::fs::read(&bad).is_err();

        let mut conn = crate::db::open(&temp_path("batch.db")).unwrap();
        let report = scan_root(&mut conn, &root, &mut || {}).unwrap();
        assert!(conn.is_autocommit(), "the last batch was committed");
        assert_eq!(
            report.threads_indexed,
            files.len() + usize::from(!bad_fails)
        );
        assert_eq!(report.errors, usize::from(bad_fails));
        for f in &files {
            let c = read_cursor(&conn, &f.to_string_lossy()).unwrap();
            assert!(
                c.is_some_and(|c| c.offset > 0),
                "cursor for {}",
                f.display()
            );
        }
        if bad_fails {
            assert!(read_cursor(&conn, &bad.to_string_lossy())
                .unwrap()
                .is_none());
        }
        let threads: i64 = conn
            .query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0))
            .unwrap();
        assert_eq!(threads as usize, report.threads_indexed);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o644));
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A re-scan with nothing to do must not take the write lock: the watcher's launch
    /// catch-up runs one, and it would otherwise stall every other writer.
    #[test]
    fn a_no_op_scan_never_takes_the_write_lock() {
        let (root, files) = scan_fixture(3);
        let db = temp_path("noop.db");
        let mut conn = crate::db::open(&db).unwrap();
        scan_root(&mut conn, &root, &mut || {}).unwrap();

        let other = crate::db::open(&db).unwrap();
        other.execute_batch("BEGIN IMMEDIATE").unwrap();
        // Any attempt to take the lock would wait out this timeout.
        let busy = Duration::from_millis(500);
        conn.busy_timeout(busy).unwrap();
        let started = Instant::now();
        let report = scan_root(&mut conn, &root, &mut || {}).unwrap();
        assert!(started.elapsed() < busy, "nothing waited on the held lock");
        assert_eq!(report.errors, 0);
        assert_eq!(report.threads_skipped, files.len());
        other.execute_batch("ROLLBACK").unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The window is checked between files even when no further file changes, so a batch
    /// never holds the lock across a long run of unchanged files.
    #[test]
    fn an_open_batch_commits_once_its_window_is_up() {
        let (root, files) = scan_fixture(1);
        let mut conn = crate::db::open(&temp_path("window.db")).unwrap();
        let sid = source_id(&conn, KIND).unwrap();
        let mut batch = Batch::new(&mut conn);
        let size = changed_size(batch.conn(), &files[0]).unwrap().unwrap();
        batch
            .run(size, |c| index_file(c, sid, &root, &files[0]))
            .unwrap();
        assert!(batch.open.is_some(), "a small changed file opens a batch");

        batch.commit_if_due().unwrap();
        assert!(batch.open.is_some(), "not due yet: still open");
        assert!(!batch.conn().is_autocommit());

        let opened = Instant::now().checked_sub(BATCH_WINDOW).unwrap();
        batch.open = Some((opened, 1));
        batch.commit_if_due().unwrap();
        assert!(batch.open.is_none());
        assert!(
            batch.conn().is_autocommit(),
            "the window elapsed: committed"
        );
        drop(batch);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn data_roots_must_be_existing_absolute_folders_and_are_stored_canonical() {
        let dir = temp_path("dataroot");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let canonical = super::super::strip_verbatim_prefix(&fs::canonicalize(&dir).unwrap());
        let want = canonical.to_string_lossy().into_owned();
        assert_eq!(
            normalize_data_root(&format!("  {}  ", dir.display())).unwrap(),
            want
        );
        // `..` (and on macOS /var -> /private/var) resolve to one spelling.
        let dotted = dir.join("sub").join("..");
        assert_eq!(
            normalize_data_root(&dotted.to_string_lossy()).unwrap(),
            want
        );
        if let Some(home) = dirs::home_dir() {
            let home = super::super::strip_verbatim_prefix(&fs::canonicalize(home).unwrap());
            assert_eq!(normalize_data_root("~").unwrap(), home.to_string_lossy());
        }

        let err = |raw: &str| normalize_data_root(raw).unwrap_err().to_string();
        assert!(err("Dropbox/claude").contains("isn't a full path"));
        assert!(err("$HOME/.claude").contains("environment variable"));
        assert!(err("%USERPROFILE%\\.claude-work").contains("environment variable"));
        assert!(err(&dir.join("missing").to_string_lossy()).contains("no folder"));
        let file = dir.join("file.txt");
        std::fs::write(&file, "x").unwrap();
        assert!(err(&file.to_string_lossy()).contains("not a folder"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn index_paths_ignores_files_outside_the_root() {
        let mut conn = crate::db::open(&temp_path("paths.db")).unwrap();
        let elsewhere = temp_path("elsewhere.jsonl");
        std::fs::write(&elsewhere, format!("{}\n", user_line("not ours"))).unwrap();
        let r = index_paths(&mut conn, &[elsewhere]).unwrap();
        assert_eq!(r.threads_indexed, 0);
    }
}
