//! Facts imported from what the agents already write, so project memory has something in
//! it before (or without) any LLM distillation:
//!
//! - Claude Code auto memory: one topic file per memory under
//!   `<claude config dir>/projects/<slug>/memory/*.md` (MEMORY.md is only the index), each
//!   with YAML frontmatter `name`, `description` and `type` (`user | feedback | project |
//!   reference`; current Claude Code nests it as `metadata.type`). Feedback becomes a gotcha
//!   and a project memory a decision; user and reference memories are about the person or
//!   point elsewhere, so they are skipped. GUI only: the startup catch-up and the watcher's
//!   minute tick call [`harvest_claude_memory`].
//! - PreCompact compaction summaries (`cal snapshot-hook`), stored as the thread's summary
//!   fact by [`store_compaction_summary`].
//!
//! Both carry `facts.origin_ref` (migration 0032), the stable key they are replaced by.
//!
//! Imported auto memory reaches MCP results and the Projects tab, but never the SessionStart
//! hook or the managed AGENTS.md / CLAUDE.md block (Claude Code loads its own copy) nor
//! `.callimachus/memory.md` (a repo file people commit, while the memory is private to
//! `~/.claude` and often about the user).

use super::app_config;
use crate::context::redact_secrets;
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// `facts.extractor` of a fact imported from a Claude Code auto-memory topic file.
pub const EXTRACTOR_CLAUDE_MEMORY: &str = "claude-memory";
/// `facts.extractor` of a Claude Code compaction summary (PreCompact `compaction_summary`).
pub const EXTRACTOR_CLAUDE_COMPACT: &str = "claude-compact";

/// `app_config` flag: import Claude Code auto memory (default on; "0" turns it off).
pub const CLAUDE_MEMORY_FLAG: &str = "harvest.claude_memory";
/// `app_config` key: JSON `{ "<origin_ref>": "<mtime ms>:<size>" }` of every topic file seen,
/// imported or skipped, so an unchanged file is never re-read.
pub const CLAUDE_MEMORY_STATE: &str = "harvest.claude_memory.state";

/// Fact text cap. A memory's `description` is its one-line hook (Claude Code shows it in
/// MEMORY.md), but some run to ~900 chars of detail; the body is the rationale.
const FACT_TEXT_MAX: usize = 300;
/// Rationale cap: the body's opening, which is where a memory states its rule and why.
const RATIONALE_MAX: usize = 1_000;
/// Topic files above this are skipped. The largest seen is ~100 KB, and only the
/// frontmatter and the first 1,000 chars are used.
const MAX_FILE_BYTES: u64 = 1 << 20;
/// Compaction summary cap, in chars.
pub const COMPACT_SUMMARY_MAX: usize = 6_000;

/// Whether auto-memory import is on (the default).
pub fn claude_memory_enabled(conn: &Connection) -> Result<bool> {
    Ok(app_config::get(conn, CLAUDE_MEMORY_FLAG)?.as_deref() != Some("0"))
}

/// What one harvest pass changed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct HarvestReport {
    /// Facts written (new and replaced files).
    pub imported: usize,
    /// Facts deleted (changed, vanished or, when turned off, every imported one).
    pub removed: usize,
}

impl HarvestReport {
    pub fn changed(&self) -> bool {
        self.imported + self.removed > 0
    }
}

/// Turn auto-memory import on (and import now) or off (and remove every imported fact).
pub fn set_claude_memory_enabled(conn: &Connection, on: bool, now: i64) -> Result<HarvestReport> {
    app_config::put_flag(conn, CLAUDE_MEMORY_FLAG, on)?;
    if on {
        harvest_claude_memory(conn, now)
    } else {
        remove_all(conn)
    }
}

/// Visible imported auto-memory facts, for Settings.
pub fn imported_count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM facts WHERE extractor = ?1 AND hidden = 0",
        [EXTRACTOR_CLAUDE_MEMORY],
        |r| r.get(0),
    )?)
}

fn remove_all(conn: &Connection) -> Result<HarvestReport> {
    let removed = conn.execute(
        "DELETE FROM facts WHERE extractor = ?1",
        [EXTRACTOR_CLAUDE_MEMORY],
    )?;
    app_config::delete(conn, CLAUDE_MEMORY_STATE)?;
    Ok(HarvestReport {
        imported: 0,
        removed,
    })
}

/// Bring the imported auto-memory facts in line with the topic files on disk: import new
/// and changed files (by mtime and size), drop the facts of files that are gone, or drop
/// everything when import is off. Cheap when nothing changed: a directory walk and a stat
/// per file (680 files across 120 projects here), no file reads and no writes.
pub fn harvest_claude_memory(conn: &Connection, now: i64) -> Result<HarvestReport> {
    // The Settings override decides where Claude Code's config lives.
    crate::indexer::claude::load_config_dir_setting(conn);
    let root = crate::indexer::claude::projects_root();
    harvest_claude_memory_in(conn, root.as_deref(), now)
}

/// One topic file on disk.
struct TopicFile {
    origin_ref: String,
    slug: String,
    path: PathBuf,
    /// `<mtime ms>:<size>`, compared against the stored state.
    sig: String,
    mtime: i64,
}

/// What one walk of the projects root found. Only a successful listing can say a file is
/// gone: a project whose `memory/` dir could not be read (permissions, a volume that is
/// gone for a moment) keeps its state and facts until a later pass can read it.
struct Listing {
    files: Vec<TopicFile>,
    /// Slugs whose `memory/` dir exists but could not be listed.
    unreadable: HashSet<String>,
    /// Topic files that were listed but could not be stat'ed (not a vanished file).
    unstatable: HashSet<String>,
}

impl Listing {
    /// Whether a state entry's file must be kept as it was, because this walk could not see it.
    fn unseen(&self, origin_ref: &str) -> bool {
        self.unstatable.contains(origin_ref)
            || origin_slug(origin_ref).is_some_and(|s| self.unreadable.contains(s))
    }
}

/// The slug in `claude-memory:<slug>/<file>`.
fn origin_slug(origin_ref: &str) -> Option<&str> {
    origin_ref
        .strip_prefix(EXTRACTOR_CLAUDE_MEMORY)?
        .strip_prefix(':')?
        .split_once('/')
        .map(|(slug, _)| slug)
}

/// Every topic file under `projects_root`, or None when the root itself cannot be listed
/// (missing, a Settings override to an unmounted volume, a permissions error). None skips
/// the pass: an empty answer there would delete every imported fact, and the user's hides
/// and pins with them.
fn topic_files(projects_root: &Path) -> Option<Listing> {
    let mut out = Listing {
        files: Vec::new(),
        unreadable: HashSet::new(),
        unstatable: HashSet::new(),
    };
    let projects = std::fs::read_dir(projects_root).ok()?;
    for project in projects.flatten() {
        let slug = project.file_name().to_string_lossy().into_owned();
        let files = match std::fs::read_dir(project.path().join("memory")) {
            Ok(files) => files,
            // No memory dir: the project has no memories (left), which is a real answer.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                out.unreadable.insert(slug);
                continue;
            }
        };
        for file in files.flatten() {
            let name = file.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".md") || name.eq_ignore_ascii_case("MEMORY.md") {
                continue;
            }
            let origin_ref = format!("{EXTRACTOR_CLAUDE_MEMORY}:{slug}/{name}");
            let meta = match std::fs::metadata(file.path()) {
                Ok(meta) => meta,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => {
                    out.unstatable.insert(origin_ref);
                    continue;
                }
            };
            if !meta.is_file() {
                continue;
            }
            let mtime_ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_millis() as i64);
            out.files.push(TopicFile {
                origin_ref,
                slug: slug.clone(),
                path: file.path(),
                sig: format!("{mtime_ms}:{}", meta.len()),
                mtime: mtime_ms / 1000,
            });
        }
    }
    Some(out)
}

fn load_state(conn: &Connection) -> Result<BTreeMap<String, String>> {
    Ok(app_config::get(conn, CLAUDE_MEMORY_STATE)?
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default())
}

/// Suffix on a state entry whose facts went to a project decoded from the slug alone (see
/// [`project_for_slug`]). Each pass re-resolves those slugs, and once a transcript or the
/// directory pins the real project, the file is re-imported there.
const GUESSED: &str = ":guess";

fn harvest_claude_memory_in(
    conn: &Connection,
    root: Option<&Path>,
    now: i64,
) -> Result<HarvestReport> {
    if !claude_memory_enabled(conn)? {
        // Only once: after the first pass there is no state left to find.
        if app_config::get(conn, CLAUDE_MEMORY_STATE)?.is_some() {
            return remove_all(conn);
        }
        return Ok(HarvestReport::default());
    }
    let Some(listing) = root.and_then(topic_files) else {
        return Ok(HarvestReport::default());
    };
    let files = &listing.files;
    let state = load_state(conn)?;
    // Slug -> (project, sure). Resolved at most once per pass.
    let mut projects: HashMap<String, (String, bool)> = HashMap::new();
    let mut changed: Vec<&TopicFile> = Vec::new();
    for f in files {
        let stored = state.get(&f.origin_ref).map(String::as_str);
        let is_changed = match stored {
            Some(s) if s == f.sig => false,
            Some(s) if s.strip_suffix(GUESSED) == Some(f.sig.as_str()) => {
                cached_project(conn, &mut projects, &f.slug)?.1
            }
            _ => true,
        };
        if is_changed {
            changed.push(f);
        }
    }
    let gone: Vec<&String> = state
        .keys()
        .filter(|k| !listing.unseen(k) && !files.iter().any(|f| &f.origin_ref == *k))
        .collect();
    if changed.is_empty() && gone.is_empty() {
        return Ok(HarvestReport::default());
    }

    let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    // Re-checked under the write lock: turning the import off between the check above and
    // this transaction runs `remove_all`, and committing here would bring the facts back.
    if !claude_memory_enabled(&tx)? {
        return Ok(HarvestReport::default());
    }
    let mut report = HarvestReport::default();
    for origin in &gone {
        report.removed += tx.execute("DELETE FROM facts WHERE origin_ref = ?1", [origin])?;
    }
    let mut guessed: HashSet<&str> = HashSet::new();
    for f in &changed {
        let (project, sure) = cached_project(&tx, &mut projects, &f.slug)?;
        if !sure {
            guessed.insert(f.origin_ref.as_str());
        }
        let fact = read_topic_file(&f.path);
        let thread = super::notes_thread_id(&tx, &project, now)?;
        // A memory whose text the user edited keeps the edit, as a re-distill keeps an edited
        // fact: the file changing again must not silently undo it. It only follows the file
        // to the right project.
        let edited: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM facts WHERE origin_ref = ?1 AND edited = 1)",
            [&f.origin_ref],
            |r| r.get(0),
        )?;
        if edited && fact.is_some() {
            tx.execute(
                "UPDATE facts SET thread_id = ?1 WHERE origin_ref = ?2",
                params![thread, f.origin_ref],
            )?;
            continue;
        }
        // Keep what the user did to the old version: a hidden memory stays hidden, a
        // pinned one stays pinned.
        let (pinned, hidden): (i64, i64) = tx.query_row(
            "SELECT COALESCE(MAX(pinned), 0), COALESCE(MAX(hidden), 0) FROM facts
             WHERE origin_ref = ?1",
            [&f.origin_ref],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        report.removed += tx.execute("DELETE FROM facts WHERE origin_ref = ?1", [&f.origin_ref])?;
        let Some(fact) = fact else {
            continue;
        };
        tx.execute(
            "INSERT INTO facts (thread_id, kind, text, rationale, status, extractor, pinned,
                                hidden, origin_ref, created_at)
             VALUES (?1, ?2, ?3, ?4, 'open', ?5, ?6, ?7, ?8, ?9)",
            params![
                thread,
                fact.kind,
                fact.text,
                fact.rationale,
                EXTRACTOR_CLAUDE_MEMORY,
                pinned,
                hidden,
                f.origin_ref,
                f.mtime.clamp(0, now),
            ],
        )?;
        report.imported += 1;
    }
    let changed_refs: HashSet<&str> = changed.iter().map(|f| f.origin_ref.as_str()).collect();
    let mut next: BTreeMap<&str, String> = BTreeMap::new();
    for f in files {
        let sig = if !changed_refs.contains(f.origin_ref.as_str()) {
            // Unchanged: keep its entry as stored, guess mark included.
            state[&f.origin_ref].clone()
        } else if guessed.contains(f.origin_ref.as_str()) {
            format!("{}{GUESSED}", f.sig)
        } else {
            f.sig.clone()
        };
        next.insert(f.origin_ref.as_str(), sig);
    }
    // What this walk could not see keeps its entry, so a later pass can still compare.
    for (k, v) in &state {
        if listing.unseen(k) {
            next.entry(k.as_str()).or_insert_with(|| v.clone());
        }
    }
    if next.is_empty() {
        app_config::delete(&tx, CLAUDE_MEMORY_STATE)?;
    } else {
        app_config::put(&tx, CLAUDE_MEMORY_STATE, &serde_json::to_string(&next)?)?;
    }
    tx.commit()?;
    Ok(report)
}

fn cached_project(
    conn: &Connection,
    cache: &mut HashMap<String, (String, bool)>,
    slug: &str,
) -> Result<(String, bool)> {
    if let Some(p) = cache.get(slug) {
        return Ok(p.clone());
    }
    let p = project_for_slug(conn, slug)?;
    cache.insert(slug.to_string(), p.clone());
    Ok(p)
}

/// The project key a Claude Code project slug stands for, and whether it is sure. The slug
/// is the session's cwd with every non-alphanumeric char turned into `-`, so decoding it is
/// lossy (`callimachus-batch-d` would come back as `callimachus/batch/d`). In order: the
/// real cwd an indexed transcript in the same directory carries (one seek into the
/// (source_id, external_id) unique index), then the directory on disk whose encoded path
/// is the slug, then (not sure) every `-` read as `/`, which the next pass retries.
fn project_for_slug(conn: &Connection, slug: &str) -> Result<(String, bool)> {
    let indexed: Option<String> = conn
        .query_row(
            "SELECT COALESCE(t.project_key, t.project_path) FROM threads t
             WHERE t.source_id = (SELECT id FROM sources WHERE kind = 'claude_code')
               AND t.external_id >= ?1 AND t.external_id < ?2
               AND t.project_path IS NOT NULL AND t.project_path != ''
             LIMIT 1",
            // '0' sorts right after '/', so this is every id under `<slug>/`.
            params![format!("{slug}/"), format!("{slug}0")],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(key) = indexed {
        return Ok((key, true));
    }
    if let Some(dir) = dir_for_slug(Path::new("/"), slug) {
        return Ok((super::project_key(&dir.to_string_lossy()), true));
    }
    Ok((super::project_key(&slug.replace('-', "/")), false))
}

/// Claude Code's slug for a path: every char that is not an ASCII letter or digit as `-`.
fn slug_of(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The existing directory under `root` whose path encodes to `slug`, found by matching one
/// path segment at a time (with backtracking, since `a-b` could be one directory or two).
/// A few `read_dir`s per slug, and only for a slug no indexed transcript explains. Unix
/// paths only: a Windows slug starts with the drive letter.
fn dir_for_slug(root: &Path, slug: &str) -> Option<PathBuf> {
    if cfg!(windows) {
        return None;
    }
    let rest = slug.strip_prefix('-')?;
    let mut budget = 64; // read_dirs, so a pathological tree cannot stall the minute tick
    walk_slug(root, rest, &mut budget)
}

fn walk_slug(dir: &Path, rest: &str, budget: &mut u32) -> Option<PathBuf> {
    if rest.is_empty() || *budget == 0 {
        return None;
    }
    *budget -= 1;
    let mut candidates: Vec<(PathBuf, usize)> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let enc = slug_of(&e.file_name().to_string_lossy());
            let fits = !enc.is_empty()
                && rest.starts_with(&enc)
                && (rest.len() == enc.len() || rest.as_bytes()[enc.len()] == b'-');
            fits.then(|| (e.path(), enc.len()))
        })
        .collect();
    // Longest first: `callimachus-batch-d` before `callimachus`.
    candidates.sort_by_key(|c| std::cmp::Reverse(c.1));
    for (path, len) in candidates {
        if len == rest.len() {
            return Some(path);
        }
        if let Some(found) = walk_slug(&path, &rest[len + 1..], budget) {
            return Some(found);
        }
    }
    None
}

/// The fact one topic file contributes.
#[derive(Debug, PartialEq, Eq)]
struct TopicFact {
    kind: &'static str,
    text: String,
    rationale: Option<String>,
}

fn read_topic_file(path: &Path) -> Option<TopicFact> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    topic_fact(&String::from_utf8_lossy(&bytes))
}

/// Map a topic file's text to a fact, or None for a memory type that is not imported.
fn topic_fact(text: &str) -> Option<TopicFact> {
    let (fm, body) = parse_frontmatter(text);
    let kind = match fm.get("type").or_else(|| fm.get("metadata.type"))?.as_str() {
        "feedback" => "gotcha",
        "project" => "decision",
        _ => return None,
    };
    let body = body.trim();
    let description = fm
        .get("description")
        .map(|d| d.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|d| !d.is_empty());
    let text = description.or_else(|| {
        body.lines()
            .map(|l| l.trim().trim_start_matches('#').trim())
            .find(|l| !l.is_empty())
            .map(str::to_string)
    })?;
    let rationale = Some(cap(&redact_secrets(body), RATIONALE_MAX)).filter(|r| !r.is_empty());
    Some(TopicFact {
        kind,
        text: cap(&redact_secrets(&text), FACT_TEXT_MAX),
        rationale,
    })
}

/// At most `max` chars, an ellipsis marking a cut.
fn cap(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

/// The frontmatter's keys (nested ones as `parent.child`) and the body after it. A small
/// reader for the flat `key: value` shape Claude Code writes, not a YAML parser: scalars,
/// quoted strings, one level of nesting, and `|` / `>` block scalars. No frontmatter gives
/// no keys and the whole text as the body.
fn parse_frontmatter(text: &str) -> (HashMap<String, String>, &str) {
    let mut keys = HashMap::new();
    let t = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = t
        .strip_prefix("---\r\n")
        .or_else(|| t.strip_prefix("---\n"))
    else {
        return (keys, t);
    };
    let mut lines: Vec<&str> = Vec::new();
    let mut body = "";
    let mut offset = 0;
    let mut closed = false;
    for line in rest.split_inclusive('\n') {
        offset += line.len();
        if line.trim_end() == "---" {
            body = &rest[offset..];
            closed = true;
            break;
        }
        lines.push(line.trim_end_matches(['\n', '\r']));
    }
    if !closed {
        return (HashMap::new(), t);
    }
    let indent = |l: &str| l.len() - l.trim_start().len();
    let mut parent: Option<String> = None;
    // The last plain scalar and its indent: YAML folds a plain value's more-indented
    // continuation lines into it with spaces (a multi-line `description:`).
    let mut plain: Option<(String, usize)> = None;
    let mut blocks: HashSet<String> = HashSet::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let level = indent(line);
        let key_line = trimmed
            .split_once(':')
            .filter(|(k, v)| is_plain_key(k) && (v.is_empty() || v.starts_with([' ', '\t'])));
        let Some((k, v)) = key_line else {
            if let Some((key, _)) = plain.as_ref().filter(|(_, l)| level > *l) {
                if let Some(value) = keys.get_mut(key) {
                    value.push(' ');
                    value.push_str(trimmed);
                }
            }
            continue;
        };
        plain = None;
        let key = match (&parent, level) {
            (_, 0) => k.trim().to_string(),
            (Some(p), _) => format!("{p}.{}", k.trim()),
            (None, _) => continue,
        };
        let v = v.trim();
        if matches!(v, "|" | "|-" | ">" | ">-") {
            let mut block = Vec::new();
            while i < lines.len() && (lines[i].trim().is_empty() || indent(lines[i]) > level) {
                block.push(lines[i].trim());
                i += 1;
            }
            let sep = if v.starts_with('|') { "\n" } else { " " };
            blocks.insert(key.clone());
            keys.insert(key, block.join(sep).trim().to_string());
            continue;
        }
        if level == 0 {
            parent = v.is_empty().then(|| key.clone());
        }
        if !v.is_empty() {
            plain = Some((key.clone(), level));
            keys.insert(key, v.to_string());
        }
    }
    // Unquoted once the continuation lines are in, so a quoted value can span lines too.
    for (key, value) in keys.iter_mut() {
        if !blocks.contains(key) {
            *value = unquote(value);
        }
    }
    (keys, body)
}

/// A frontmatter key as Claude Code writes them: letters, digits, `_`, `-`, `.`.
fn is_plain_key(k: &str) -> bool {
    let k = k.trim();
    !k.is_empty()
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

fn unquote(v: &str) -> String {
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        let inner = &v[1..v.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => {}
            }
        }
        return out;
    }
    if v.len() >= 2 && v.starts_with('\'') && v.ends_with('\'') {
        return v[1..v.len() - 1].replace("''", "'");
    }
    v.to_string()
}

/// Store a Claude Code compaction summary as `thread_id`'s summary fact, replacing the one
/// an earlier compaction of the thread left. Redacted (the summary is model output over the
/// whole transcript, tool output included) and capped at [`COMPACT_SUMMARY_MAX`] chars.
/// Returns whether a summary was stored (false for a blank one).
pub fn store_compaction_summary(
    conn: &Connection,
    thread_id: i64,
    session_id: &str,
    summary: &str,
    now: i64,
) -> Result<bool> {
    let text = cap(&redact_secrets(summary.trim()), COMPACT_SUMMARY_MAX);
    if text.is_empty() {
        return Ok(false);
    }
    let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    tx.execute(
        "DELETE FROM facts WHERE thread_id = ?1 AND extractor = ?2",
        params![thread_id, EXTRACTOR_CLAUDE_COMPACT],
    )?;
    tx.execute(
        "INSERT INTO facts (thread_id, kind, text, status, extractor, origin_ref, created_at)
         VALUES (?1, 'summary', ?2, 'open', ?3, ?4, ?5)",
        params![
            thread_id,
            text,
            EXTRACTOR_CLAUDE_COMPACT,
            format!("{EXTRACTOR_CLAUDE_COMPACT}:{session_id}"),
            now
        ],
    )?;
    tx.commit()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> Connection {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "calli_harvest_{}_{}.db",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        crate::db::open(&p).unwrap()
    }

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("calli_harvest_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(root: &Path, slug: &str, name: &str, text: &str) -> PathBuf {
        let dir = root.join(slug).join("memory");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, text).unwrap();
        p
    }

    fn facts(conn: &Connection) -> Vec<(String, String, String, i64)> {
        let mut stmt = conn
            .prepare(
                "SELECT f.kind, f.text, COALESCE(t.project_key, t.project_path), f.hidden
                 FROM facts f JOIN threads t ON t.id = f.thread_id
                 WHERE f.extractor = 'claude-memory' ORDER BY f.text",
            )
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    #[test]
    fn frontmatter_parses_both_type_shapes_and_maps_kinds() {
        // Current Claude Code: `type` nested under `metadata`, a quoted description.
        let nested = "---\nname: no-em-dashes\ndescription: \"User dislikes em-dashes; \\\"never\\\" use them\"\nmetadata: \n  node_type: memory\n  type: feedback\n  originSessionId: 5a25\n---\n\nNever use em-dashes.\n\n**Why:** style.\n";
        let f = topic_fact(nested).unwrap();
        assert_eq!(f.kind, "gotcha");
        assert_eq!(f.text, "User dislikes em-dashes; \"never\" use them");
        assert_eq!(
            f.rationale.as_deref(),
            Some("Never use em-dashes.\n\n**Why:** style.")
        );
        // Older shape: top-level type, single quotes, a folded description.
        let top = "---\nname: x\ntype: project\ndescription: >\n  Ships as a direct\n  download, it''s signed\n---\nbody";
        let f = topic_fact(top).unwrap();
        assert_eq!(f.kind, "decision");
        assert_eq!(f.text, "Ships as a direct download, it''s signed");
        let quoted = "---\ntype: project\ndescription: 'it''s fine'\n---\n";
        assert_eq!(topic_fact(quoted).unwrap().text, "it's fine");
        assert_eq!(topic_fact(quoted).unwrap().rationale, None);
        // No description: the first body line, heading marks dropped, capped.
        let long = format!("---\ntype: feedback\n---\n\n# {}\n", "word ".repeat(100));
        let f = topic_fact(&long).unwrap();
        assert!(f.text.starts_with("word word"), "{}", f.text);
        assert_eq!(f.text.chars().count(), FACT_TEXT_MAX);
        assert!(f.text.ends_with('…'));
        // Skipped: user and reference memories, unknown types, no frontmatter, unclosed.
        for skip in [
            "---\ntype: user\ndescription: d\n---\n",
            "---\nmetadata:\n  type: reference\ndescription: d\n---\n",
            "---\ntype: other\ndescription: d\n---\n",
            "just a note",
            "---\ntype: project\ndescription: d\n",
        ] {
            assert_eq!(topic_fact(skip), None, "{skip}");
        }
        // Secrets never reach the index.
        let secret = "---\ntype: project\ndescription: key is sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n---\n";
        assert!(!topic_fact(secret).unwrap().text.contains("AAAAAAAAAAAA"));
    }

    #[test]
    fn harvest_replaces_on_change_removes_on_delete_and_on_disable() {
        let conn = temp_db();
        let root = temp_root("cycle");
        let now = 1_800_000_000;
        let slug = "-Users-me-home-club";
        // An indexed transcript under the slug carries the real (hyphenated) cwd.
        let src: i64 = conn
            .query_row(
                "SELECT id FROM sources WHERE kind = 'claude_code'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO threads (source_id, external_id, title, project_path, project_key)
             VALUES (?1, ?2, 't', '/Users/me/home-club', '/Users/me/home-club')",
            params![src, format!("{slug}/abc.jsonl")],
        )
        .unwrap();
        let a = write(
            &root,
            slug,
            "a.md",
            "---\ntype: feedback\ndescription: run biome on touched files only\n---\nwhy",
        );
        write(
            &root,
            slug,
            "b.md",
            "---\nmetadata:\n  type: project\ndescription: ship direct download\n---\n",
        );
        write(&root, slug, "MEMORY.md", "- [a](a.md)");
        write(
            &root,
            slug,
            "me.md",
            "---\ntype: user\ndescription: senior dev\n---\n",
        );
        write(
            &root,
            "-Users-me-other",
            "c.md",
            "---\ntype: project\ndescription: unindexed project\n---\n",
        );

        let r = harvest_claude_memory_in(&conn, Some(&root), now).unwrap();
        assert_eq!(r.imported, 3);
        let got = facts(&conn);
        assert_eq!(got.len(), 3);
        assert!(got.contains(&(
            "gotcha".into(),
            "run biome on touched files only".into(),
            "/Users/me/home-club".into(),
            0
        )));
        assert!(got
            .iter()
            .any(|f| f.1 == "unindexed project" && f.2 == "/Users/me/other"));
        assert_eq!(imported_count(&conn).unwrap(), 3);
        // They show in the project's memory, tagged with their origin.
        let mem = super::super::get_project_memory_facts(&conn, "/Users/me/home-club", 10).unwrap();
        assert_eq!(mem.gotchas[0].extractor, EXTRACTOR_CLAUDE_MEMORY);
        assert_eq!(
            mem.gotchas[0].origin_ref.as_deref(),
            Some("claude-memory:-Users-me-home-club/a.md")
        );
        let agents = super::super::get_project_memory_facts_excluding(
            &conn,
            "/Users/me/home-club",
            10,
            EXTRACTOR_CLAUDE_MEMORY,
        )
        .unwrap();
        assert!(agents.gotchas.is_empty() && agents.decisions.is_empty());

        // Nothing changed: nothing written.
        assert_eq!(
            harvest_claude_memory_in(&conn, Some(&root), now).unwrap(),
            HarvestReport::default()
        );

        // A changed file is replaced, keeping the user's hide.
        conn.execute(
            "UPDATE facts SET hidden = 1 WHERE origin_ref = 'claude-memory:-Users-me-home-club/a.md'",
            [],
        )
        .unwrap();
        std::fs::write(
            &a,
            "---\ntype: feedback\ndescription: run biome on touched files, never the tree\n---\n",
        )
        .unwrap();
        let r = harvest_claude_memory_in(&conn, Some(&root), now).unwrap();
        assert_eq!((r.imported, r.removed), (1, 1));
        let got = facts(&conn);
        assert!(got.contains(&(
            "gotcha".into(),
            "run biome on touched files, never the tree".into(),
            "/Users/me/home-club".into(),
            1
        )));
        assert_eq!(got.len(), 3);

        // A deleted file's fact goes.
        std::fs::remove_file(&a).unwrap();
        let r = harvest_claude_memory_in(&conn, Some(&root), now).unwrap();
        assert_eq!((r.imported, r.removed), (0, 1));
        assert_eq!(facts(&conn).len(), 2);

        // Turned off: every imported fact goes, and the state with it.
        app_config::put_flag(&conn, CLAUDE_MEMORY_FLAG, false).unwrap();
        let r = harvest_claude_memory_in(&conn, Some(&root), now).unwrap();
        assert_eq!(r.removed, 2);
        assert!(facts(&conn).is_empty());
        assert!(app_config::get(&conn, CLAUDE_MEMORY_STATE)
            .unwrap()
            .is_none());
        // Back on: imported again.
        let r = set_claude_memory_enabled(&conn, true, now).unwrap();
        assert!(claude_memory_enabled(&conn).unwrap());
        let _ = r;
        assert_eq!(
            harvest_claude_memory_in(&conn, Some(&root), now)
                .unwrap()
                .imported,
            2
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn plain_multi_line_description_folds() {
        let text = "---\ntype: feedback\ndescription: Run biome on the files\n  you touched, see https://biomejs.dev\n  only\nname: x\n---\nbody";
        let f = topic_fact(text).unwrap();
        assert_eq!(
            f.text,
            "Run biome on the files you touched, see https://biomejs.dev only"
        );
        let quoted = "---\ntype: project\ndescription: \"Ships as a\n  direct download\"\n---\n";
        assert_eq!(
            topic_fact(quoted).unwrap().text,
            "Ships as a direct download"
        );
    }

    /// A walk that cannot see a file never counts it as gone: a projects root that cannot
    /// be listed skips the pass, and an unreadable `memory/` dir keeps that project's facts.
    #[test]
    fn unreadable_listing_keeps_facts_and_state() {
        let conn = temp_db();
        let root = temp_root("unreadable");
        let now = 1_800_000_000;
        write(
            &root,
            "-Users-me-a",
            "a.md",
            "---\ntype: feedback\ndescription: keep me\n---\n",
        );
        write(
            &root,
            "-Users-me-b",
            "b.md",
            "---\ntype: project\ndescription: and me\n---\n",
        );
        assert_eq!(
            harvest_claude_memory_in(&conn, Some(&root), now)
                .unwrap()
                .imported,
            2
        );
        let state = app_config::get(&conn, CLAUDE_MEMORY_STATE).unwrap();
        for missing in [None, Some(root.join("not-mounted"))] {
            assert_eq!(
                harvest_claude_memory_in(&conn, missing.as_deref(), now).unwrap(),
                HarvestReport::default()
            );
        }
        assert_eq!(facts(&conn).len(), 2);
        assert_eq!(app_config::get(&conn, CLAUDE_MEMORY_STATE).unwrap(), state);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = root.join("-Users-me-a").join("memory");
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
            // Running as root reads it anyway; the check only means something otherwise.
            let blocked = std::fs::read_dir(&dir).is_err();
            let r = harvest_claude_memory_in(&conn, Some(&root), now).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            if blocked {
                assert_eq!(r, HarvestReport::default());
                assert_eq!(facts(&conn).len(), 2);
                assert_eq!(app_config::get(&conn, CLAUDE_MEMORY_STATE).unwrap(), state);
            }
        }
        // A project whose memory dir is really gone loses its facts.
        std::fs::remove_dir_all(root.join("-Users-me-b")).unwrap();
        let r = harvest_claude_memory_in(&conn, Some(&root), now).unwrap();
        assert_eq!(r.removed, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn slug_resolves_against_the_directory_on_disk() {
        let root = temp_root("slugwalk");
        std::fs::create_dir_all(root.join("code").join("callimachus-batch-d")).unwrap();
        std::fs::create_dir_all(root.join("code").join("callimachus").join("x")).unwrap();
        std::fs::create_dir_all(root.join(".config").join("app")).unwrap();
        let rel = |p: &str| root.join(p);
        assert_eq!(
            dir_for_slug(&root, "-code-callimachus-batch-d"),
            Some(rel("code/callimachus-batch-d"))
        );
        assert_eq!(
            dir_for_slug(&root, "-code-callimachus-x"),
            Some(rel("code/callimachus/x"))
        );
        assert_eq!(
            dir_for_slug(&root, "--config-app"),
            Some(rel(".config/app"))
        );
        assert_eq!(dir_for_slug(&root, "-code-nothing"), None);
        assert_eq!(
            dir_for_slug(&root, "code"),
            None,
            "a slug starts with the root"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A slug nothing explains yet goes to its decoded guess, and moves to the real project
    /// once a transcript under it is indexed, with no change to the file.
    #[test]
    fn guessed_project_is_re_resolved_and_edits_survive_changes() {
        let conn = temp_db();
        let root = temp_root("guess");
        let now = 1_800_000_000;
        let slug = "-Users-me-my-repo";
        let file = write(
            &root,
            slug,
            "a.md",
            "---\ntype: feedback\ndescription: first\n---\n",
        );
        harvest_claude_memory_in(&conn, Some(&root), now).unwrap();
        assert_eq!(facts(&conn)[0].2, "/Users/me/my/repo", "the guess");
        assert_eq!(
            harvest_claude_memory_in(&conn, Some(&root), now).unwrap(),
            HarvestReport::default(),
            "still a guess: nothing to do"
        );
        conn.execute(
            "INSERT INTO threads (source_id, external_id, title, project_path, project_key)
             VALUES ((SELECT id FROM sources WHERE kind = 'claude_code'), ?1, 't',
                     '/Users/me/my-repo', '/Users/me/my-repo')",
            [format!("{slug}/s.jsonl")],
        )
        .unwrap();
        let r = harvest_claude_memory_in(&conn, Some(&root), now).unwrap();
        assert_eq!(r.imported, 1);
        assert_eq!(facts(&conn)[0].2, "/Users/me/my-repo");
        assert_eq!(
            harvest_claude_memory_in(&conn, Some(&root), now).unwrap(),
            HarvestReport::default()
        );

        // The user's edit outlives the next change to the file.
        conn.execute(
            "UPDATE facts SET text = 'my wording', edited = 1 WHERE extractor = 'claude-memory'",
            [],
        )
        .unwrap();
        std::fs::write(
            &file,
            "---\ntype: feedback\ndescription: second, longer\n---\n",
        )
        .unwrap();
        harvest_claude_memory_in(&conn, Some(&root), now).unwrap();
        let got = facts(&conn);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1, "my wording");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Real data: imports this machine's Claude Code auto memory into a temp index and
    /// times the first pass and the no-change pass the watcher's minute tick runs. Run by
    /// name: `cargo test -- --ignored real_claude_memory_harvest --nocapture`.
    #[test]
    #[ignore]
    fn real_claude_memory_harvest() {
        let conn = temp_db();
        let root = crate::indexer::claude::projects_root().unwrap();
        let now = chrono::Utc::now().timestamp();
        let t = std::time::Instant::now();
        let files = topic_files(&root).map_or(0, |l| l.files.len());
        let walk = t.elapsed();
        let t = std::time::Instant::now();
        let first = harvest_claude_memory_in(&conn, Some(&root), now).unwrap();
        let first_t = t.elapsed();
        let t = std::time::Instant::now();
        let again = harvest_claude_memory_in(&conn, Some(&root), now).unwrap();
        let again_t = t.elapsed();
        println!(
            "{files} topic files, walk {walk:?}; first pass {first:?} in {first_t:?}; \
             no-change pass {again:?} in {again_t:?}"
        );
        assert!(!again.changed());
    }

    #[test]
    fn compaction_summary_is_stored_redacted_capped_and_replaced() {
        let conn = temp_db();
        conn.execute(
            "INSERT INTO threads (source_id, external_id, title) VALUES (1, 'p/s1.jsonl', 't')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        assert!(!store_compaction_summary(&conn, tid, "s1", "   ", 1).unwrap());
        let long = format!(
            "Refactored auth. token sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA {}",
            "x".repeat(7_000)
        );
        assert!(store_compaction_summary(&conn, tid, "s1", &long, 1).unwrap());
        assert!(store_compaction_summary(&conn, tid, "s1", "Second compaction.", 2).unwrap());
        let rows: Vec<(String, String, String, String)> = conn
            .prepare("SELECT kind, text, extractor, origin_ref FROM facts WHERE thread_id = ?1")
            .unwrap()
            .query_map([tid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![(
                "summary".into(),
                "Second compaction.".into(),
                "claude-compact".into(),
                "claude-compact:s1".into()
            )]
        );
        store_compaction_summary(&conn, tid, "s1", &long, 3).unwrap();
        let text: String = conn
            .query_row("SELECT text FROM facts WHERE thread_id = ?1", [tid], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(text.chars().count(), COMPACT_SUMMARY_MAX);
        assert!(!text.contains("AAAAAAAAAAAA"));
    }
}
