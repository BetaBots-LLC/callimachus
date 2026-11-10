//! Small parsing helpers shared by the source indexers. Each one used to be copied into
//! several indexers, and the copies had started to drift.
//!
//! Also the parse counters ([`ParseStats`]): every indexer drops malformed lines and record
//! types it does not know, which is right for one odd line and silent data loss when an
//! agent changes its transcript format (the index is the only copy once the agent's own
//! cleanup deletes the file). Each indexer reports what it read through [`count_line`],
//! [`count_malformed`], [`count_skipped`] and [`begin_file`] / [`count_file`]; a pass collects
//! them per source with [`collect_parse_stats`], and `indexer::parse_health` judges them.

use super::ParsedMessage;
use serde_json::Value;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// What one pass over one source parsed, by count only: type names from the files and
/// numbers, never text from them (these land in logs, `app_config` and `cal doctor`, which a
/// user pastes into a public issue).
#[derive(
    Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "camelCase")]
pub struct ParseStats {
    /// Records read: a JSONL line, an item of a JSON array, a row of an agent's database.
    pub lines: u64,
    /// Records that were not valid JSON (counted in `lines` too).
    pub malformed_lines: u64,
    /// Record or part types the indexer does not know and dropped (or, for a role, misread),
    /// by name: at most 32 names of at most 48 chars, the rest under `(other)`. Types an
    /// indexer skips on purpose (thinking, images, snapshots) are not here.
    pub unknown_kinds: BTreeMap<String, u64>,
    /// Files (or conversations of a database source) parsed whole.
    pub files: u64,
    // An empty file is not one: all 807 of the 2,079 Cursor composers on the author's
    // machine that yield nothing hold no bubble at all (unused chat tabs, measured
    // 2026-11-09), and counted here they would read as a format change on a full reindex.
    /// Of those, the ones that held at least one record and yielded no message (an empty
    /// file is not one).
    pub files_with_zero_messages: u64,
}

/// Most distinct names [`ParseStats::unknown_kinds`] keeps, [`OTHER_KINDS`] included: a file
/// full of invented types must not grow the map (it is stored per source in `app_config`).
pub const MAX_KINDS: usize = 32;
/// Longest kept name, in chars.
pub const MAX_KIND_CHARS: usize = 48;
/// Where names past [`MAX_KINDS`] are counted.
pub const OTHER_KINDS: &str = "(other)";
/// The name of a record with no type field at all.
pub const NO_KIND: &str = "(none)";

impl ParseStats {
    /// Records dropped as unknown, all names together.
    pub fn unknown(&self) -> u64 {
        self.unknown_kinds.values().sum()
    }

    /// Nothing was parsed (no record, no file): the source has nothing on disk, or nothing
    /// changed since the last pass.
    pub fn is_empty(&self) -> bool {
        self.lines == 0 && self.files == 0
    }

    /// Fold another tally into this one.
    pub fn merge(&mut self, other: &ParseStats) {
        self.lines += other.lines;
        self.malformed_lines += other.malformed_lines;
        self.files += other.files;
        self.files_with_zero_messages += other.files_with_zero_messages;
        for (name, n) in &other.unknown_kinds {
            self.add_unknown(name, *n);
        }
    }

    fn add_unknown(&mut self, name: &str, n: u64) {
        if let Some(count) = self.unknown_kinds.get_mut(name) {
            *count += n;
            return;
        }
        let key = if self.unknown_kinds.len() + 1 >= MAX_KINDS {
            OTHER_KINDS
        } else {
            name
        };
        *self.unknown_kinds.entry(key.to_string()).or_insert(0) += n;
    }
}

thread_local! {
    /// The tally of the [`collect_parse_stats`] running on this thread, if any. A thread-local
    /// rather than a parameter: the parse functions are shared with paths nobody counts (the
    /// usage backfill, the compaction anchor, tests), and the scans of all 13 sources keep
    /// their signature. Every scan parses on the thread that runs it.
    static PARSE_STATS: RefCell<Option<ParseStats>> = const { RefCell::new(None) };
}

/// Apply `f` to the running tally; a no-op when nothing collects. Per record this is a
/// thread-local read and a counter increment: a fresh index of 300 real Claude transcripts
/// (267 MiB) took 3.4 s before and after (release `cal index`, median of 5 runs).
#[inline]
fn with_stats(f: impl FnOnce(&mut ParseStats)) {
    PARSE_STATS.with(|cell| {
        if let Some(stats) = cell.borrow_mut().as_mut() {
            f(stats);
        }
    });
}

/// One record read.
#[inline]
pub fn count_line() {
    with_stats(|s| s.lines += 1);
}

/// One record that was not valid JSON (also a line).
#[inline]
pub fn count_malformed() {
    with_stats(|s| {
        s.lines += 1;
        s.malformed_lines += 1;
    });
}

/// A record (or a part of one) of type `kind` that the indexer dropped. Counted as unknown,
/// under `prefix` + the type's name, unless `on_purpose` lists the type (`NO_KIND` stands for
/// a record without one). The name is only built for an unknown type, so the common skips
/// cost a few comparisons.
pub fn count_skipped(prefix: &str, kind: Option<&str>, on_purpose: &[&str]) {
    let kind = kind.unwrap_or(NO_KIND);
    if on_purpose.contains(&kind) {
        return;
    }
    with_stats(|s| s.add_unknown(&kind_name(prefix, kind), 1));
}

/// Where one file's records start in the running tally ([`begin_file`]), so [`count_file`]
/// can tell an empty file from one whose records yielded nothing.
#[derive(Clone, Copy, Debug)]
#[must_use = "pass it to count_file once the file is parsed"]
pub struct FileStart(u64);

/// Mark the start of a whole file (or conversation), before its first record is counted.
#[inline]
pub fn begin_file() -> FileStart {
    FileStart(PARSE_STATS.with(|cell| cell.borrow().as_ref().map_or(0, |s| s.lines)))
}

/// The file begun at `start` parsed into `messages` messages. It counts as a file without
/// messages only when it held a record ([`ParseStats::files_with_zero_messages`]). A token
/// rather than "records since the last file": records read between files (a Claude
/// transcript's appended lines, a session file that is not JSON) would make an empty file
/// look full.
#[inline]
pub fn count_file(start: FileStart, messages: usize) {
    with_stats(|s| {
        s.files += 1;
        if messages == 0 && s.lines > start.0 {
            s.files_with_zero_messages += 1;
        }
    });
}

/// Run `f` and return what it parsed on this thread. Nested calls each see their own part,
/// and the outer one gets it too.
pub fn collect_parse_stats<T>(f: impl FnOnce() -> T) -> (T, ParseStats) {
    let outer = PARSE_STATS.with(|cell| cell.borrow_mut().replace(ParseStats::default()));
    // Restored on unwind as well: a panicking scan must not leave this thread collecting.
    struct Restore(Option<Option<ParseStats>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Some(outer) = self.0.take() {
                PARSE_STATS.with(|cell| *cell.borrow_mut() = outer);
            }
        }
    }
    let mut restore = Restore(Some(outer));
    let out = f();
    let mine = PARSE_STATS
        .with(|cell| cell.borrow_mut().take())
        .unwrap_or_default();
    let mut outer = restore.0.take().flatten();
    if let Some(o) = outer.as_mut() {
        o.merge(&mine);
    }
    PARSE_STATS.with(|cell| *cell.borrow_mut() = outer);
    (out, mine)
}

/// Run `f` without counting what it parses, for bytes an earlier pass already counted (a
/// Claude file re-read whole because it has no stored thread to append to).
pub fn uncounted<T>(f: impl FnOnce() -> T) -> T {
    let saved = PARSE_STATS.with(|cell| cell.borrow_mut().take());
    // Put back on unwind as well, as in `collect_parse_stats`.
    struct Restore(Option<ParseStats>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let saved = self.0.take();
            PARSE_STATS.with(|cell| *cell.borrow_mut() = saved);
        }
    }
    let _restore = Restore(saved);
    f()
}

/// `prefix` + `kind` as a stored name: only ASCII letters, digits, space and `_.:/@-()` kept
/// (anything else becomes `?`, so a type field holding text or control characters cannot
/// carry it into a log or an issue), cut at [`MAX_KIND_CHARS`]. A name can still be
/// token-shaped; `cal doctor` redacts it on the way out.
fn kind_name(prefix: &str, kind: &str) -> String {
    prefix
        .chars()
        .chain(kind.chars())
        .take(MAX_KIND_CHARS)
        .map(|c| {
            if c.is_ascii_alphanumeric() || "_.:/@-() ".contains(c) {
                c
            } else {
                '?'
            }
        })
        .collect()
}

/// Longest title derived from message text, in chars (an ellipsis marks the cut).
const TITLE_CHARS: usize = 80;

/// A thread title from free text (usually the first user message): trimmed, and cut at
/// [`TITLE_CHARS`] with an ellipsis.
pub fn truncate_title(s: &str) -> String {
    let s = s.trim();
    // Byte length bounds char count, so short titles skip the char walk.
    if s.len() <= TITLE_CHARS || s.chars().count() <= TITLE_CHARS {
        return s.to_string();
    }
    let mut out: String = s.chars().take(TITLE_CHARS).collect();
    out.push('…');
    out
}

/// An RFC 3339 / ISO 8601 timestamp as epoch seconds.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.timestamp())
}

/// The text of a content value that is either a plain string or an array of `{text}`
/// parts (Anthropic tool results, Continue messages), with parts joined by newlines.
/// Parts without text (images, tool calls) are dropped.
pub fn join_text_parts(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// A JSON file's parsed contents, or None when it is missing, unreadable or malformed.
pub fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Append a message with `text` trimmed, skipping it when nothing is left: an empty row
/// would only add noise to search and the incremental prefix check.
pub fn push_message(
    out: &mut Vec<ParsedMessage>,
    role: &str,
    text: &str,
    tool_name: Option<String>,
    ts: Option<i64>,
) {
    let text = text.trim();
    if !text.is_empty() {
        out.push(ParsedMessage {
            role: role.to_string(),
            text: text.to_string(),
            tool_name,
            ts,
        });
    }
}

/// Claude Code's (and Cursor's) slug for a path: every char that is not an ASCII letter or
/// digit as `-`.
pub fn slug_of(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The existing directory under `root` whose path encodes to `slug` (`-` plus the path
/// below `root`, Claude Code's form), found by matching one path segment at a time (with
/// backtracking, since `a-b` could be one directory or two). A few `read_dir`s per slug,
/// and only for a slug no indexed transcript explains. Platform-free: [`dir_for_project_slug`]
/// picks the root (`/`, or a Windows drive).
pub fn dir_for_slug(root: &Path, slug: &str) -> Option<PathBuf> {
    let rest = slug.strip_prefix('-')?;
    let mut budget = 64; // read_dirs, so a pathological tree cannot stall the minute tick
    walk_slug(root, rest, &mut budget)
}

/// The project directory a slug in Claude Code's form stands for on this machine. On Unix
/// that is `-` plus the absolute path (`-Users-me-x`). On Windows the path starts with the
/// drive letter, its `:\` becoming one or two dashes (`C--Users-me-x`; Cursor's names
/// read `c-Users-me-x`), so the walk starts at that drive's root.
pub fn dir_for_project_slug(slug: &str) -> Option<PathBuf> {
    if cfg!(windows) {
        let (drive, rest) = split_drive_slug(slug.strip_prefix('-').unwrap_or(slug))?;
        dir_for_slug(Path::new(&format!("{drive}:\\")), &format!("-{rest}"))
    } else {
        dir_for_slug(Path::new("/"), slug)
    }
}

/// `(drive letter, rest)` of a Windows slug: a letter, then the one or two dashes its `:\`
/// encodes to. Upper-cased, since Cursor writes the letter in either case.
pub fn split_drive_slug(slug: &str) -> Option<(char, &str)> {
    let drive = slug.chars().next().filter(char::is_ascii_alphabetic)?;
    let rest = slug[1..].strip_prefix('-')?;
    let rest = rest.strip_prefix('-').unwrap_or(rest);
    (!rest.is_empty()).then(|| (drive.to_ascii_uppercase(), rest))
}

/// Whether `s` is an absolute Windows path with a drive (`C:\x`, `c:/x`).
pub fn is_drive_path(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}

/// A slug in the form both agents' spellings agree on, for comparing them: no leading dash,
/// and a Windows drive prefix as `<lower letter>-` whether it was written with one dash or
/// two. `X--` normalizes on every host (no Unix path encodes that way short of a dotted
/// one-letter root dir), so Windows paths compare correctly in tests run anywhere.
pub fn normalize_slug(slug: &str) -> String {
    let s = slug.trim_start_matches('-');
    match split_drive_slug(s) {
        Some((drive, rest)) if cfg!(windows) || s.get(1..3) == Some("--") => {
            format!("{}-{rest}", drive.to_ascii_lowercase())
        }
        _ => s.to_string(),
    }
}

fn walk_slug(dir: &Path, rest: &str, budget: &mut u32) -> Option<PathBuf> {
    if rest.is_empty() || *budget == 0 {
        return None;
    }
    *budget -= 1;
    let mut candidates: Vec<(PathBuf, usize)> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let enc = slug_of(&e.file_name().to_string_lossy());
            let fits = !enc.is_empty()
                && rest.starts_with(&enc)
                && (rest.len() == enc.len() || rest.as_bytes()[enc.len()] == b'-');
            // Only a name that fits pays for a stat. `metadata` follows symlinks, unlike
            // `DirEntry::file_type`: a linked project dir (Fedora Silverblue's `/home` is a
            // link to `/var/home`, a symlinked `~/code`) must resolve under the spelling the
            // agent recorded. The read_dir budget bounds a link cycle.
            (fits && std::fs::metadata(e.path()).is_ok_and(|m| m.is_dir()))
                .then(|| (e.path(), enc.len()))
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

/// FNV-1a 64: tiny, dependency-free, and stable across Rust releases (std's hasher is not
/// guaranteed to be, and byte-cursor fingerprints built on it are persisted).
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_are_trimmed_and_cut_on_char_boundaries() {
        assert_eq!(truncate_title("  fix the build \n"), "fix the build");
        let exact = "a".repeat(TITLE_CHARS);
        assert_eq!(truncate_title(&exact), exact, "at the limit: kept whole");
        let long = "é".repeat(TITLE_CHARS + 5);
        let t = truncate_title(&long);
        assert_eq!(t.chars().count(), TITLE_CHARS + 1);
        assert!(t.ends_with('…'));
        // Multi-byte text under the char limit but over it in bytes is kept whole.
        let wide = "é".repeat(TITLE_CHARS);
        assert_eq!(truncate_title(&wide), wide);
    }

    #[test]
    fn parses_rfc3339_with_offsets() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:10Z"), Some(10));
        assert_eq!(parse_rfc3339("1970-01-01T01:00:10+01:00"), Some(10));
        assert_eq!(parse_rfc3339("yesterday"), None);
    }

    #[test]
    fn joins_string_or_text_parts() {
        let v =
            serde_json::json!([{"type": "text", "text": "a"}, {"type": "image"}, {"text": "b"}]);
        assert_eq!(join_text_parts(Some(&v)), "a\nb");
        assert_eq!(join_text_parts(Some(&serde_json::json!("plain"))), "plain");
        assert_eq!(join_text_parts(Some(&serde_json::json!(3))), "");
        assert_eq!(join_text_parts(None), "");
    }

    #[test]
    fn windows_drive_slugs_split_and_compare_either_way() {
        assert_eq!(split_drive_slug("C--Users-me-x"), Some(('C', "Users-me-x")));
        assert_eq!(split_drive_slug("c-Users-me-x"), Some(('C', "Users-me-x")));
        assert_eq!(split_drive_slug("C-"), None);
        assert_eq!(split_drive_slug("1-x"), None);
        assert_eq!(
            normalize_slug("C--Users-me-x"),
            normalize_slug("c-Users-me-x")
        );
        assert_eq!(normalize_slug("-Users-me-x"), "Users-me-x");
        assert!(is_drive_path(r"C:\Users\me") && is_drive_path("d:/w"));
        assert!(!is_drive_path("/Users/me") && !is_drive_path("C:") && !is_drive_path("ab/c"));
    }

    /// The walk starts at whatever root it is given (`/`, or a Windows drive's), so a drive
    /// slug's rest resolves against a stand-in root here on every host.
    #[test]
    fn a_drive_slug_walks_from_its_root() {
        let root = std::env::temp_dir().join(format!("calli_slugdrive_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("Users").join("me").join("my-app")).unwrap();
        let (_, rest) = split_drive_slug("C--Users-me-my-app").unwrap();
        assert_eq!(
            dir_for_slug(&root, &format!("-{rest}")),
            Some(root.join("Users").join("me").join("my-app"))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A project reached through a symlinked directory resolves under the linked spelling
    /// (`/home` -> `/var/home` on Fedora Silverblue, a linked `~/code`).
    #[cfg(unix)]
    #[test]
    fn slug_walk_follows_symlinked_directories() {
        let root = std::env::temp_dir().join(format!("calli_sluglink_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("var").join("home").join("me").join("app")).unwrap();
        std::os::unix::fs::symlink(root.join("var").join("home"), root.join("home")).unwrap();
        assert_eq!(
            dir_for_slug(&root, "-home-me-app"),
            Some(root.join("home").join("me").join("app"))
        );
        // A link to a file is not a directory.
        std::fs::write(root.join("note"), "x").unwrap();
        std::os::unix::fs::symlink(root.join("note"), root.join("linked")).unwrap();
        assert_eq!(dir_for_slug(&root, "-linked"), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_stats_count_only_inside_a_collector() {
        count_line(); // nobody collects: dropped
        let ((), stats) = collect_parse_stats(|| {
            let full = begin_file();
            count_line();
            count_malformed();
            count_skipped("", Some("thinking"), &["thinking"]);
            count_skipped("", Some("brand_new"), &["thinking"]);
            count_skipped("block:", Some("brand_new"), &[]);
            count_skipped("", None, &[]);
            // Records but no message: a file that lost its messages.
            count_file(full, 0);
            // No record at all: an empty file, not one without messages.
            let empty = begin_file();
            count_file(empty, 0);
            // Records read between files (an appended transcript) do not fill the next one.
            count_line();
            let empty = begin_file();
            count_file(empty, 0);
            let good = begin_file();
            count_file(good, 3);
            // A nested collector sees its own part; the outer one gets it as well.
            let ((), inner) = collect_parse_stats(count_line);
            assert_eq!(inner.lines, 1);
        });
        assert_eq!(stats.lines, 4);
        assert_eq!(stats.malformed_lines, 1);
        assert_eq!(stats.files, 4);
        assert_eq!(stats.files_with_zero_messages, 1);
        assert_eq!(
            stats.unknown_kinds,
            BTreeMap::from([
                ("(none)".to_string(), 1),
                ("block:brand_new".to_string(), 1),
                ("brand_new".to_string(), 1),
            ])
        );
        let ((), after) = collect_parse_stats(|| {});
        assert!(after.is_empty(), "nothing leaks into the next collector");
    }

    /// Type names come from the files: anything but a short, plain identifier is cut and
    /// masked, and a flood of invented types folds into one bucket.
    #[test]
    fn unknown_kind_names_are_capped_and_masked() {
        let ((), stats) = collect_parse_stats(|| {
            count_skipped("", Some("secret text\nwith a newline and é"), &[]);
            count_skipped("", Some(&"x".repeat(200)), &[]);
            for i in 0..100 {
                count_skipped("", Some(&format!("kind{i}")), &[]);
            }
        });
        assert!(stats.unknown_kinds.len() <= MAX_KINDS);
        assert_eq!(stats.unknown(), 102);
        assert!(stats
            .unknown_kinds
            .contains_key("secret text?with a newline and ?"));
        assert!(stats
            .unknown_kinds
            .keys()
            .all(|k| k.chars().count() <= MAX_KIND_CHARS && !k.contains('\n')));
        assert!(stats.unknown_kinds[OTHER_KINDS] > 0);
    }

    /// A scan that panics must not leave its thread collecting into a dead tally.
    #[test]
    fn a_panicking_collector_restores_the_outer_one() {
        let ((), outer) = collect_parse_stats(|| {
            let caught = std::panic::catch_unwind(|| {
                collect_parse_stats(|| {
                    count_line();
                    panic!("scan failed");
                })
            });
            assert!(caught.is_err());
            count_line();
        });
        assert_eq!(
            outer.lines, 1,
            "the outer tally survives and keeps counting"
        );
    }

    #[test]
    fn push_message_trims_and_drops_blank_text() {
        let mut out = Vec::new();
        push_message(&mut out, "user", "  hi  ", None, Some(1));
        push_message(&mut out, "user", " \n ", None, None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "hi");
        assert_eq!(out[0].ts, Some(1));
    }
}
