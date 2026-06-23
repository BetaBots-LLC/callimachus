//! Recurring-issue mining: find errors you keep hitting across every indexed agent session.
//! Two stages: an FTS candidate fetch (messages mentioning error-ish tokens) keeps the scan
//! cheap, then a precise per-line extractor pulls real error signatures and a normalizer
//! collapses the variable parts (paths, line:col, quoted identifiers, hashes) so the SAME error
//! recurring across runs groups together. Only Callimachus sees this across all your tools.

use anyhow::Result;
use rusqlite::{Connection, ToSql};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

/// One recurring error, grouped by its normalized signature.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueCluster {
    /// A representative raw error line (the first one seen).
    pub example: String,
    /// How many messages this error appeared in (≈ times you hit it).
    pub count: i64,
    /// Distinct threads it spans.
    pub threads: i64,
    pub first_seen: i64,
    pub last_seen: i64,
}

/// Pull distinct error signatures out of one message's text. High-precision: a line counts only
/// if it carries a strong error marker near its start (not just the word "error" in prose).
pub fn extract_errors(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.len() < 6 || line.len() > 400 {
            continue;
        }
        if is_error_line(line) && seen.insert(normalize(line)) {
            out.push(line.to_string());
        }
    }
    out
}

/// Marker check. Looks at the first ~60 chars so a stray "error:" deep in a prose paragraph
/// doesn't match, while real errors (which lead the line) do.
fn is_error_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    // Char-safe: a byte slice could land inside a multi-byte char (e.g. box-drawing `━` from
    // cargo output) and panic. Take the first 64 chars instead.
    let head: String = lower.chars().take(64).collect();
    head.starts_with("error[")
        || head.starts_with("error:")
        || head.starts_with("error ")
        || head.starts_with("fatal:")
        || head.starts_with("fatal error")
        || head.starts_with("panic:")
        || head.contains("error:")        // TypeError:/ValueError:/RuntimeError: ...
        || head.contains("exception:")
        || head.contains("panicked at")
        || head.contains("traceback (most recent call last)")
        || head.contains("command not found")
        || head.contains("undefined reference")
        || head.contains("cannot find")
        || head.contains("assertion failed")
        || head.contains("assertion `")
        || head.contains("no such file")
        || head.contains("segmentation fault")
}

/// Collapse a raw error line to a recurrence key: lowercase, with paths, quoted identifiers,
/// line:col, and hashes/long numbers replaced by placeholders, so the same error with different
/// specifics groups together. Token-based so it stays dependency-free.
pub fn normalize(line: &str) -> String {
    line.split_whitespace()
        .map(canon_token)
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn canon_token(tok: &str) -> String {
    let t = tok.trim_matches(|c: char| matches!(c, ',' | ';' | ')' | '(' | '[' | ']'));
    if t.is_empty() {
        return String::new();
    }
    // Path-like (has a separator) -> <path>; also catches file:line:col.
    if t.contains('/') || (t.contains('\\') && t.len() > 3) {
        return "<path>".into();
    }
    // Quoted identifier / string -> '…' (groups "named `foo`" with "named `bar`").
    if matches!(t.as_bytes()[0], b'`' | b'\'' | b'"') {
        return "'…'".into();
    }
    // Hash / hex / long number -> <n> (but keep short codes like an error number on its own).
    let core = t.strip_prefix("0x").unwrap_or(t);
    if core.len() >= 6 && core.chars().all(|c| c.is_ascii_hexdigit()) {
        return "<n>".into();
    }
    t.to_ascii_lowercase()
}

/// Find recurring errors across messages since `since` (epoch secs), optionally scoped to a
/// project-path substring. Returns clusters seen in >= 2 messages, most-frequent first.
pub fn recurring_issues(
    conn: &Connection,
    project: Option<&str>,
    since: i64,
    limit: usize,
) -> Result<Vec<IssueCluster>> {
    // Cap the candidate scan; errors lead the line, so the FTS pre-filter keeps this cheap.
    const CANDIDATE_CAP: i64 = 40_000;
    let mut sql = String::from(
        "SELECT m.text, m.ts, m.thread_id
         FROM messages_fts
         JOIN messages m ON m.id = messages_fts.rowid
         JOIN threads t ON t.id = m.thread_id
         WHERE messages_fts MATCH ?1
           AND m.role IN ('user', 'tool')
           AND m.ts >= ?2
           AND t.is_subagent = 0",
    );
    let mut args: Vec<Box<dyn ToSql>> = vec![
        Box::new("error OR panicked OR exception OR traceback OR fatal".to_string()),
        Box::new(since),
    ];
    if let Some(p) = project.filter(|p| !p.is_empty()) {
        args.push(Box::new(format!("%{p}%")));
        sql.push_str(&format!(
            " AND COALESCE(t.project_key, t.project_path) LIKE ?{}",
            args.len()
        ));
    }
    args.push(Box::new(CANDIDATE_CAP));
    sql.push_str(&format!(" ORDER BY m.ts DESC LIMIT ?{}", args.len()));

    struct Acc {
        example: String,
        count: i64,
        threads: HashSet<i64>,
        first_seen: i64,
        last_seen: i64,
    }
    let mut clusters: HashMap<String, Acc> = HashMap::new();

    let arg_refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(arg_refs.as_slice(), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    for row in rows {
        let (text, ts, thread_id) = row?;
        for err in extract_errors(&text) {
            let key = normalize(&err);
            let acc = clusters.entry(key).or_insert_with(|| Acc {
                example: err.clone(),
                count: 0,
                threads: HashSet::new(),
                first_seen: ts,
                last_seen: ts,
            });
            acc.count += 1;
            acc.threads.insert(thread_id);
            acc.first_seen = acc.first_seen.min(ts);
            acc.last_seen = acc.last_seen.max(ts);
        }
    }

    let mut out: Vec<IssueCluster> = clusters
        .into_values()
        .filter(|a| a.count >= 2) // "recurring" = seen at least twice
        .map(|a| IssueCluster {
            example: a.example,
            count: a.count,
            threads: a.threads.len() as i64,
            first_seen: a.first_seen,
            last_seen: a.last_seen,
        })
        .collect();
    out.sort_by(|a, b| b.count.cmp(&a.count).then(b.last_seen.cmp(&a.last_seen)));
    out.truncate(limit);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_catches_real_errors_skips_prose() {
        let text = "Here's the build output:\n\
            error[E0599]: no method named `frobnicate` found for struct `Widget`\n\
            we should handle that error gracefully later\n\
            thread 'main' panicked at src/lib.rs:42:5\n\
            TypeError: Cannot read properties of undefined (reading 'x')\n\
            just some normal text about error handling in general";
        let errs = extract_errors(text);
        assert!(errs.iter().any(|e| e.starts_with("error[E0599]")));
        assert!(errs.iter().any(|e| e.contains("panicked at")));
        assert!(errs.iter().any(|e| e.starts_with("TypeError:")));
        // Prose mentioning "error" mid-sentence is NOT captured.
        assert!(!errs.iter().any(|e| e.contains("gracefully")));
        assert!(!errs.iter().any(|e| e.contains("normal text")));
    }

    #[test]
    fn normalize_groups_same_error_with_different_specifics() {
        // Same error, different identifier + path + line -> same key.
        let a = "error[E0599]: no method named `foo` found at /Users/me/src/a.rs:12:5";
        let b = "error[E0599]: no method named `bar` found at /home/x/src/b.rs:99:1";
        assert_eq!(normalize(a), normalize(b));
        // A genuinely different error does NOT collapse to the same key.
        let c = "error[E0277]: the trait bound `T: Clone` is not satisfied";
        assert_ne!(normalize(a), normalize(c));
    }

    #[test]
    fn normalize_replaces_paths_quotes_hashes() {
        let k = normalize("panicked at /a/b/c.rs:1:2 with hash deadbeef1234 and `ident`");
        assert!(k.contains("<path>"), "{k}");
        assert!(k.contains("'…'"), "{k}");
        assert!(k.contains("<n>"), "{k}");
        assert!(!k.contains("deadbeef"), "{k}");
    }
}
