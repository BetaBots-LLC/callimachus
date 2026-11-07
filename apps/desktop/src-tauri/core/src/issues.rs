//! Recurring-issue mining: find errors you keep hitting across every indexed agent session.
//! Two stages: an FTS candidate fetch (messages mentioning error-ish tokens) keeps the scan
//! cheap, then a precise per-line extractor pulls real error signatures and a normalizer
//! collapses the variable parts (paths, line:col, quoted identifiers, hashes) so the SAME error
//! recurring across runs groups together. Only Callimachus sees this across all your tools.

use crate::search::ProjectMatch;
use anyhow::Result;
use rusqlite::{Connection, ToSql};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

/// One recurring error, grouped by its normalized signature.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct IssueCluster {
    /// A representative raw error line (the latest raw sighting: candidates are read newest
    /// first and the first line seen for a signature is kept).
    pub example: String,
    /// How many messages this error appeared in (≈ times you hit it).
    pub count: i64,
    /// Distinct threads it spans.
    pub threads: i64,
    pub first_seen: i64,
    pub last_seen: i64,
    /// The thread of the latest sighting, for the Today view's project label. Not part of
    /// the JSON that `cal issues`, MCP and the app's lists return.
    #[serde(skip_serializing)]
    pub last_thread_id: i64,
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

/// Chars of a long message's head / tail scanned for errors (see `recurring_issues`).
const TEXT_HEAD_CHARS: usize = 6_000;
const TEXT_TAIL_CHARS: usize = 2_000;

/// Cache key: (project filter, since bucketed to the hour, limit).
type IssuesKey = (Option<String>, i64, usize);

static ISSUES_CACHE: std::sync::LazyLock<
    crate::search::cache::AggCache<IssuesKey, Vec<IssueCluster>>,
> = std::sync::LazyLock::new(crate::search::cache::AggCache::new);

/// [`recurring_issues`], reused for up to 10 minutes while the thread set is unchanged
/// (see `search::cache`), with `project` an exact key ([`ProjectMatch::Exact`]). `since` is
/// bucketed to the hour so "now minus 180 days" callers share an entry. For the app's
/// Project page; MCP and `cal` read live, and so does Today (an error hit again in a session
/// that is still running leaves the thread set unchanged).
pub fn recurring_issues_cached(
    conn: &Connection,
    project: Option<&str>,
    since: i64,
    limit: usize,
) -> Result<Vec<IssueCluster>> {
    let since = since - since.rem_euclid(3600);
    let key = (
        project.filter(|p| !p.is_empty()).map(str::to_string),
        since,
        limit,
    );
    ISSUES_CACHE.get_or_try(crate::search::cache::index_version(conn)?, key, || {
        recurring_issues_matching(conn, project, ProjectMatch::Exact, since, limit)
    })
}

/// The candidate fetch for [`recurring_issues`]: error-ish messages since `since`, newest
/// first, as (text head + tail, ts, thread_id).
fn candidates_sql(
    conn: &Connection,
    project: Option<&str>,
    mode: ProjectMatch,
    since: i64,
) -> (String, Vec<Box<dyn ToSql>>) {
    // Cap the candidate scan; errors lead the line, so the FTS pre-filter keeps this cheap.
    const CANDIDATE_CAP: i64 = 40_000;
    // Long tool output is read as its head and tail only: errors lead a command's output
    // or close it (the summary / final failure), and copying every multi-KB log body into
    // Rust is what made this view allocate 100+ MB. octet_length() reads only the record
    // header, so short rows skip the substr work entirely.
    //
    // The role / ts / subagent / project filters run off the covering index that
    // fts_message_join names, so only surviving rows read the table: 385k page reads down
    // to 51k, ~15 s down to ~3 s from a cold cache on a large index.
    let mut sql = format!(
        "SELECT CASE WHEN octet_length(m.text) > {long}
                     THEN substr(m.text, 1, {head}) || char(10) || substr(m.text, -{tail})
                     ELSE m.text END,
                m.ts, m.thread_id
         FROM messages_fts
         {join}
         JOIN threads t ON t.id = m.thread_id
         WHERE messages_fts MATCH ?1
           AND m.role IN ('user', 'tool')
           AND m.ts >= ?2
           AND t.is_subagent = 0",
        long = TEXT_HEAD_CHARS + TEXT_TAIL_CHARS,
        head = TEXT_HEAD_CHARS,
        tail = TEXT_TAIL_CHARS,
        join = crate::search::fts_message_join(conn),
    );
    let mut args: Vec<Box<dyn ToSql>> = vec![
        Box::new("error OR panicked OR exception OR traceback OR fatal".to_string()),
        Box::new(since),
    ];
    if let Some(p) = project.filter(|p| !p.is_empty()) {
        let cond = crate::search::project_condition("t", p, mode, &mut args);
        sql.push_str(&format!(" AND {cond}"));
    }
    args.push(Box::new(CANDIDATE_CAP));
    sql.push_str(&format!(" ORDER BY m.ts DESC LIMIT ?{}", args.len()));
    (sql, args)
}

/// Find recurring errors across messages since `since` (epoch secs), optionally scoped to a
/// project-path substring (`cal issues -p`, MCP). Returns clusters seen in >= 2 messages,
/// most-frequent first.
pub fn recurring_issues(
    conn: &Connection,
    project: Option<&str>,
    since: i64,
    limit: usize,
) -> Result<Vec<IssueCluster>> {
    recurring_issues_matching(conn, project, ProjectMatch::Substring, since, limit)
}

/// [`recurring_issues`] with the project matched as `mode` says.
pub fn recurring_issues_matching(
    conn: &Connection,
    project: Option<&str>,
    mode: ProjectMatch,
    since: i64,
    limit: usize,
) -> Result<Vec<IssueCluster>> {
    let (sql, args) = candidates_sql(conn, project, mode, since);

    struct Acc {
        example: String,
        count: i64,
        threads: HashSet<i64>,
        first_seen: i64,
        last_seen: i64,
        last_thread_id: i64,
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
                last_thread_id: thread_id,
            });
            acc.count += 1;
            acc.threads.insert(thread_id);
            acc.first_seen = acc.first_seen.min(ts);
            if ts > acc.last_seen {
                acc.last_seen = ts;
                acc.last_thread_id = thread_id;
            }
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
            last_thread_id: a.last_thread_id,
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
    fn finds_errors_at_both_ends_of_long_output() {
        let p = std::env::temp_dir().join(format!("calli_issues_{}.db", std::process::id()));
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        let conn = crate::db::open(&p).unwrap();
        conn.execute(
            "INSERT INTO threads (source_id, external_id, title) VALUES (1, 't', 't')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        let filler = "compiling some crate\n".repeat(2_000); // ~42 KB
        let body = format!(
            "error[E0425]: cannot find value `x` in this scope\n{filler}\
             error: could not compile `app` due to 1 previous error"
        );
        for seq in 0..2 {
            conn.execute(
                "INSERT INTO messages (thread_id, seq, role, text, ts) VALUES (?1, ?2, 'tool', ?3, 100)",
                rusqlite::params![tid, seq, body],
            )
            .unwrap();
        }
        let clusters = recurring_issues(&conn, None, 0, 10).unwrap();
        assert!(clusters
            .iter()
            .any(|c| c.example.starts_with("error[E0425]")));
        assert!(clusters
            .iter()
            .any(|c| c.example.contains("could not compile")));
        // The cached variant returns the same answer.
        let cached = recurring_issues_cached(&conn, None, 0, 10).unwrap();
        assert_eq!(cached.len(), clusters.len());
    }

    fn plan(conn: &Connection, sql: &str, args: &[Box<dyn ToSql>]) -> String {
        let refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let rows = stmt
            .query_map(refs.as_slice(), |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        rows.join(" | ")
    }

    /// The candidate scan must stay FTS-driven, with each hit's filters answered by the
    /// covering index (not a table read per hit), and must still run on an index that
    /// predates the covering index.
    #[test]
    fn candidate_scan_uses_the_covering_index_and_falls_back_without_it() {
        let p = std::env::temp_dir().join(format!("calli_issues_plan_{}.db", std::process::id()));
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        let conn = crate::db::open(&p).unwrap();
        for (project, mode) in [
            (None, ProjectMatch::Substring),
            (Some("proj"), ProjectMatch::Substring),
            (Some("/r/proj"), ProjectMatch::Exact),
        ] {
            let (sql, args) = candidates_sql(&conn, project, mode, 0);
            let plan = plan(&conn, &sql, &args);
            assert!(
                plan.starts_with("SCAN messages_fts VIRTUAL TABLE"),
                "{plan}"
            );
            assert!(
                plan.contains("SEARCH m USING INDEX idx_messages_cover (id=?)"),
                "{plan}"
            );
        }

        conn.execute_batch("DROP INDEX idx_messages_cover").unwrap();
        let (sql, _) = candidates_sql(&conn, None, ProjectMatch::Substring, 0);
        assert!(!sql.contains("INDEXED BY"), "{sql}");
        conn.execute(
            "INSERT INTO threads (source_id, external_id, title) VALUES (1, 't', 't')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        for seq in 0..2 {
            conn.execute(
                "INSERT INTO messages (thread_id, seq, role, text, ts)
                 VALUES (?1, ?2, 'tool', 'error: linker failed', 100)",
                rusqlite::params![tid, seq],
            )
            .unwrap();
        }
        assert_eq!(recurring_issues(&conn, None, 0, 10).unwrap().len(), 1);
    }

    /// The app's Project page scopes to an exact key, so a sibling repo whose path starts
    /// with it (`/r/app-web` beside `/r/app`) stays out; `cal issues -p` keeps the substring.
    #[test]
    fn the_app_scope_matches_the_project_key_exactly() {
        let p = std::env::temp_dir().join(format!("calli_issues_exact_{}.db", std::process::id()));
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        let conn = crate::db::open(&p).unwrap();
        for (ext, path, err) in [
            ("a", "/r/app", "error: linker failed"),
            ("w", "/r/app-web", "fatal: bad object HEAD"),
        ] {
            conn.execute(
                "INSERT INTO threads (source_id, external_id, title, project_path)
                 VALUES (1, ?1, 't', ?2)",
                [ext, path],
            )
            .unwrap();
            let tid = conn.last_insert_rowid();
            for seq in 0..2 {
                conn.execute(
                    "INSERT INTO messages (thread_id, seq, role, text, ts)
                     VALUES (?1, ?2, 'tool', ?3, 100)",
                    rusqlite::params![tid, seq, err],
                )
                .unwrap();
            }
        }
        assert_eq!(
            recurring_issues(&conn, Some("/r/app"), 0, 10)
                .unwrap()
                .len(),
            2
        );
        let exact =
            recurring_issues_matching(&conn, Some("/r/app"), ProjectMatch::Exact, 0, 10).unwrap();
        assert_eq!(exact.len(), 1, "{exact:?}");
        assert!(exact[0].example.contains("linker"));
        assert_eq!(
            recurring_issues_cached(&conn, Some("/r/app"), 0, 10)
                .unwrap()
                .len(),
            1
        );
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
