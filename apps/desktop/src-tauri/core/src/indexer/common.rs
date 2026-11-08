//! Small parsing helpers shared by the source indexers. Each one used to be copied into
//! several indexers, and the copies had started to drift.

use super::ParsedMessage;
use serde_json::Value;
use std::path::{Path, PathBuf};

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
    fn push_message_trims_and_drops_blank_text() {
        let mut out = Vec::new();
        push_message(&mut out, "user", "  hi  ", None, Some(1));
        push_message(&mut out, "user", " \n ", None, None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "hi");
        assert_eq!(out[0].ts, Some(1));
    }
}
