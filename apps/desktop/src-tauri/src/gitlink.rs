//! Git linkage: infer which commits a thread produced, entirely on-device. A thread carries a
//! project, a time window (first..last message), and `file_mentions` (paths discussed). We read
//! the project's `git log` over that window and link a thread to a commit when their changed
//! files overlap — answering "which AI conversation led to this commit?". The shared-file count
//! (`overlap`) doubles as a confidence proxy. No git crate: we shell out to `git`.

use crate::indexer::canonical_project;
use anyhow::{bail, Result};
use rusqlite::Connection;
use serde::Serialize;
use std::collections::HashSet;

/// Field/record separators embedded in the `git log --pretty` format (unlikely in commit text).
const RS: char = '\u{1e}'; // record (per-commit) separator
const US: char = '\u{1f}'; // field separator

/// A commit parsed from `git log`, with the files it changed.
#[derive(Debug, Clone)]
pub struct Commit {
    pub sha: String,
    pub short_sha: String,
    pub committed_at: i64,
    pub subject: String,
    pub files: Vec<String>,
}

/// A thread's correlation inputs: its time window and the (lowercased) paths it mentioned.
#[derive(Debug)]
pub struct ThreadWindow {
    pub thread_id: i64,
    pub start: i64,
    pub end: i64,
    pub mentions: HashSet<String>,
}

/// An inferred thread↔commit link.
#[derive(Debug, Clone)]
pub struct Link {
    pub thread_id: i64,
    pub sha: String,
    pub short_sha: String,
    pub subject: String,
    pub committed_at: i64,
    pub overlap: usize,
}

/// One commit linked to a thread (for `linked_commits` / the timeline), serialized for the UI.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitLink {
    pub sha: String,
    pub short_sha: String,
    pub subject: Option<String>,
    pub committed_at: i64,
    pub overlap: i64,
}

/// One commit in a project's timeline, with the thread it was inferred from.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TimelineRow {
    pub sha: String,
    pub short_sha: String,
    pub subject: Option<String>,
    pub committed_at: i64,
    pub overlap: i64,
    pub thread_id: i64,
    pub thread_title: Option<String>,
}

/// Parse `git log --name-only --pretty=format:%x1eCOMMIT%x1f%H%x1f%ct%x1f%s` output. Each record
/// starts with RS; its first line carries the fields, the remaining non-empty lines are files.
pub fn parse_git_log(output: &str) -> Vec<Commit> {
    let mut commits = Vec::new();
    for rec in output.split(RS) {
        let rec = rec.trim_matches('\n');
        if rec.is_empty() {
            continue;
        }
        let mut lines = rec.lines();
        let header = lines.next().unwrap_or("");
        let mut f = header.split(US);
        if f.next() != Some("COMMIT") {
            continue;
        }
        let sha = f.next().unwrap_or("").to_string();
        let committed_at: i64 = f.next().unwrap_or("0").trim().parse().unwrap_or(0);
        let subject = f.next().unwrap_or("").to_string();
        if sha.is_empty() {
            continue;
        }
        let files: Vec<String> = lines
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        let short_sha: String = sha.chars().take(10).collect();
        commits.push(Commit {
            sha,
            short_sha,
            committed_at,
            subject,
            files,
        });
    }
    commits
}

/// True if a mentioned path and a commit file refer to the same file. Exact, or one is a
/// path-suffix of the other at a directory boundary — but only when the shorter side carries a
/// `/` (so a bare "mod.rs" mention doesn't match every mod.rs in the repo). Inputs lowercased.
fn path_match(mention: &str, file: &str) -> bool {
    if mention == file {
        return true;
    }
    if file.len() > mention.len()
        && mention.contains('/')
        && file.ends_with(mention)
        && file.as_bytes()[file.len() - mention.len() - 1] == b'/'
    {
        return true;
    }
    if mention.len() > file.len()
        && file.contains('/')
        && mention.ends_with(file)
        && mention.as_bytes()[mention.len() - file.len() - 1] == b'/'
    {
        return true;
    }
    false
}

/// Count distinct commit files that match any of the thread's mentions.
pub fn path_overlap(mentions: &HashSet<String>, files: &[String]) -> usize {
    files
        .iter()
        .filter(|f| {
            let fl = f.to_ascii_lowercase();
            mentions.iter().any(|m| path_match(m, &fl))
        })
        .count()
}

/// Commits land slightly before, or well after, the discussion that drove them.
const SLACK_BEFORE: i64 = 3_600; // 1h
const SLACK_AFTER: i64 = 86_400; // 1 day
const MIN_OVERLAP: usize = 1;
const MAX_PER_THREAD: usize = 5;

/// Correlate threads to commits: for each thread, commits whose timestamp falls in its (slacked)
/// window AND whose files overlap its mentions, best-overlap-then-most-recent, capped per thread.
pub fn correlate(threads: &[ThreadWindow], commits: &[Commit]) -> Vec<Link> {
    let mut out = Vec::new();
    for t in threads {
        let mut cands: Vec<(usize, &Commit)> = commits
            .iter()
            .filter(|c| {
                c.committed_at >= t.start - SLACK_BEFORE && c.committed_at <= t.end + SLACK_AFTER
            })
            .filter_map(|c| {
                let ov = path_overlap(&t.mentions, &c.files);
                (ov >= MIN_OVERLAP).then_some((ov, c))
            })
            .collect();
        cands.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.committed_at.cmp(&a.1.committed_at)));
        cands.truncate(MAX_PER_THREAD);
        for (ov, c) in cands {
            out.push(Link {
                thread_id: t.thread_id,
                sha: c.sha.clone(),
                short_sha: c.short_sha.clone(),
                subject: c.subject.clone(),
                committed_at: c.committed_at,
                overlap: ov,
            });
        }
    }
    out
}

/// Run `git log` over `repo` since `since` (epoch secs), returning raw output for `parse_git_log`.
fn run_git_log(repo: &str, since: i64) -> Result<String> {
    let since_iso = chrono::DateTime::from_timestamp(since.max(0), 0)
        .map(|d| d.to_rfc3339())
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string());
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("log")
        .arg("--no-merges")
        .arg(format!("--since={since_iso}"))
        .arg("--name-only")
        .arg(format!("--pretty=format:{RS}COMMIT{US}%H{US}%ct{US}%s"))
        .output()
        .map_err(|e| anyhow::anyhow!("running git: {e} (is git installed?)"))?;
    if !out.status.success() {
        bail!(
            "git log failed in {repo}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Load every thread's time window + mentions for a canonical project key.
fn thread_windows(conn: &Connection, project_key: &str) -> Result<Vec<ThreadWindow>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, MIN(m.ts), MAX(m.ts)
         FROM threads t JOIN messages m ON m.thread_id = t.id
         WHERE COALESCE(t.project_key, t.project_path) = ?1 AND m.ts IS NOT NULL
         GROUP BY t.id",
    )?;
    let rows: Vec<(i64, i64, i64)> = stmt
        .query_map([project_key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;

    let mut windows = Vec::with_capacity(rows.len());
    for (id, start, end) in rows {
        let mut ms = conn.prepare("SELECT path FROM file_mentions WHERE thread_id = ?1")?;
        let mentions: HashSet<String> = ms
            .query_map([id], |r| r.get::<_, String>(0))?
            .filter_map(rusqlite::Result::ok)
            .map(|p| p.to_ascii_lowercase())
            .collect();
        if mentions.is_empty() {
            continue; // nothing to correlate on
        }
        windows.push(ThreadWindow {
            thread_id: id,
            start,
            end,
            mentions,
        });
    }
    Ok(windows)
}

/// Compute + persist thread↔commit links for the git repo at `repo_path`. Returns the number of
/// links stored. Re-runs cleanly: it replaces the links for the threads it reconsiders.
pub fn link_project(conn: &Connection, repo_path: &str) -> Result<usize> {
    let key = canonical_project(repo_path).unwrap_or_else(|| repo_path.to_string());
    let windows = thread_windows(conn, &key)?;
    if windows.is_empty() {
        return Ok(0);
    }
    let earliest = windows.iter().map(|w| w.start).min().unwrap_or(0);
    let commits = parse_git_log(&run_git_log(repo_path, earliest - SLACK_BEFORE)?);
    let links = correlate(&windows, &commits);

    let now = chrono::Utc::now().timestamp();
    // Replace links for the threads we just reconsidered (so a re-run doesn't stack duplicates).
    for w in &windows {
        conn.execute(
            "DELETE FROM thread_commits WHERE thread_id = ?1",
            [w.thread_id],
        )?;
    }
    let mut ins = conn.prepare(
        "INSERT OR REPLACE INTO thread_commits
            (thread_id, sha, short_sha, subject, committed_at, overlap, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    for l in &links {
        ins.execute(rusqlite::params![
            l.thread_id,
            l.sha,
            l.short_sha,
            l.subject,
            l.committed_at,
            l.overlap as i64,
            now,
        ])?;
    }
    Ok(links.len())
}

/// The commits inferred for one thread, most recent first.
pub fn linked_commits(conn: &Connection, thread_id: i64) -> Result<Vec<CommitLink>> {
    let mut stmt = conn.prepare(
        "SELECT sha, short_sha, subject, committed_at, overlap
         FROM thread_commits WHERE thread_id = ?1
         ORDER BY committed_at DESC",
    )?;
    let rows = stmt.query_map([thread_id], |r| {
        Ok(CommitLink {
            sha: r.get(0)?,
            short_sha: r.get(1)?,
            subject: r.get(2)?,
            committed_at: r.get(3)?,
            overlap: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// A project's commit timeline (each linked commit with the thread it came from), newest first.
pub fn commit_timeline(
    conn: &Connection,
    project_key: &str,
    limit: usize,
) -> Result<Vec<TimelineRow>> {
    let mut stmt = conn.prepare(
        "SELECT tc.sha, tc.short_sha, tc.subject, tc.committed_at, tc.overlap, t.id, t.title
         FROM thread_commits tc JOIN threads t ON t.id = tc.thread_id
         WHERE COALESCE(t.project_key, t.project_path) = ?1
         ORDER BY tc.committed_at DESC, tc.sha
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![project_key, limit as i64], |r| {
        Ok(TimelineRow {
            sha: r.get(0)?,
            short_sha: r.get(1)?,
            subject: r.get(2)?,
            committed_at: r.get(3)?,
            overlap: r.get(4)?,
            thread_id: r.get(5)?,
            thread_title: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    fn temp_db() -> Connection {
        let p = std::env::temp_dir().join(format!(
            "calli_gl_{}_{}.db",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        crate::db::open(&p).unwrap()
    }

    #[test]
    fn parse_git_log_reads_commits_and_files() {
        let out = format!(
            "{RS}COMMIT{US}abc123def456{US}1700000000{US}fix auth refresh\nsrc/auth.rs\nsrc/lib.rs\n\n\
             {RS}COMMIT{US}99887766{US}1700000500{US}tweak css\nweb/app.css\n"
        );
        let commits = parse_git_log(&out);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].sha, "abc123def456");
        assert_eq!(commits[0].short_sha, "abc123def4");
        assert_eq!(commits[0].committed_at, 1_700_000_000);
        assert_eq!(commits[0].subject, "fix auth refresh");
        assert_eq!(commits[0].files, vec!["src/auth.rs", "src/lib.rs"]);
        assert_eq!(commits[1].files, vec!["web/app.css"]);
    }

    #[test]
    fn path_overlap_matches_at_boundaries_only() {
        let mentions: HashSet<String> = ["src/embed/mod.rs", "package.json"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // Suffix at a dir boundary (git path is longer) counts.
        assert_eq!(
            path_overlap(&mentions, &["apps/desktop/src/embed/mod.rs".into()]),
            1
        );
        // Exact match counts.
        assert_eq!(path_overlap(&mentions, &["package.json".into()]), 1);
        // A different mod.rs must NOT match the bare-name-less mention (no false positive).
        assert_eq!(path_overlap(&mentions, &["src/other/mod.rs".into()]), 0);
        // Non-boundary suffix ("xmod.rs") must not match.
        assert_eq!(path_overlap(&mentions, &["src/xembed/mod.rs".into()]), 0);
    }

    #[test]
    fn correlate_links_by_overlap_within_window() {
        let threads = vec![ThreadWindow {
            thread_id: 1,
            start: 1_000,
            end: 2_000,
            mentions: ["src/auth.rs".to_string()].into_iter().collect(),
        }];
        let commits = vec![
            // In-window, file overlaps -> linked.
            Commit {
                sha: "in".into(),
                short_sha: "in".into(),
                committed_at: 2_500, // within end + SLACK_AFTER
                subject: "the one".into(),
                files: vec!["src/auth.rs".into(), "README.md".into()],
            },
            // Overlaps but WAY outside the window -> not linked.
            Commit {
                sha: "late".into(),
                short_sha: "late".into(),
                committed_at: 2_000 + SLACK_AFTER + 10,
                subject: "too late".into(),
                files: vec!["src/auth.rs".into()],
            },
            // In-window but no file overlap -> not linked.
            Commit {
                sha: "nofiles".into(),
                short_sha: "nofiles".into(),
                committed_at: 1_500,
                subject: "unrelated".into(),
                files: vec!["docs/x.md".into()],
            },
        ];
        let links = correlate(&threads, &commits);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].sha, "in");
        assert_eq!(links[0].overlap, 1);
    }

    #[test]
    fn link_project_end_to_end_with_real_git() {
        use crate::indexer::{source_id, upsert_thread, ParsedMessage, ParsedThread};

        // Skip cleanly where git isn't available.
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }

        let dir = std::env::temp_dir().join(format!(
            "calli_gitrepo_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let repo = dir.to_str().unwrap().to_string();

        let git = |args: &[&str], env: &[(&str, &str)]| {
            let mut c = std::process::Command::new("git");
            c.arg("-C").arg(&repo).args(args);
            for (k, v) in env {
                c.env(k, v);
            }
            let out = c.output().unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q"], &[]);
        git(&["config", "user.email", "t@t"], &[]);
        git(&["config", "user.name", "t"], &[]);
        std::fs::write(dir.join("src/auth.rs"), "fn a() {}").unwrap();
        git(&["add", "."], &[]);
        let ts = 1_700_000_000_i64;
        let date = format!("{ts} +0000");
        git(
            &["commit", "-q", "-m", "wire auth"],
            &[
                ("GIT_AUTHOR_DATE", date.as_str()),
                ("GIT_COMMITTER_DATE", date.as_str()),
            ],
        );

        // Seed a thread in the same project, a message at the commit time mentioning src/auth.rs.
        let mut conn = temp_db();
        let sid = source_id(&conn, "claude_code").unwrap();
        upsert_thread(
            &mut conn,
            sid,
            &ParsedThread {
                external_id: "g1".into(),
                title: Some("auth work".into()),
                project_path: Some(repo.clone()),
                messages: vec![ParsedMessage {
                    role: "user".into(),
                    text: "editing src/auth.rs for the refresh".into(),
                    tool_name: None,
                    ts: Some(ts),
                }],
                ..Default::default()
            },
        )
        .unwrap();
        let tid: i64 = conn
            .query_row("SELECT id FROM threads WHERE external_id = 'g1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        // Pin project_key to the same canonical value link_project derives, so they join.
        let key = canonical_project(&repo).unwrap_or_else(|| repo.clone());
        conn.execute(
            "UPDATE threads SET project_key = ?1 WHERE id = ?2",
            rusqlite::params![key, tid],
        )
        .unwrap();

        let n_links = link_project(&conn, &repo).unwrap();
        assert!(n_links >= 1, "the commit touching src/auth.rs should link");

        let links = linked_commits(&conn, tid).unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].subject.as_deref(), Some("wire auth"));
        assert!(links[0].overlap >= 1);

        let tl = commit_timeline(&conn, &key, 10).unwrap();
        assert_eq!(tl.len(), 1);
        assert_eq!(tl[0].thread_id, tid);
        assert_eq!(tl[0].subject.as_deref(), Some("wire auth"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
