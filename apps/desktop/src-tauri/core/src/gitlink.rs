//! Git linkage: infer which commits a thread produced, entirely on-device. A thread carries a
//! project, a time window (first..last message), and `file_mentions` (paths discussed). We read
//! the project's `git log` over that window and link a thread to a commit when their changed
//! files overlap — answering "which AI conversation led to this commit?". The shared-file count
//! (`overlap`) doubles as a confidence proxy. No git crate: we shell out to `git`.

use anyhow::{bail, Result};
use rusqlite::{Connection, Transaction, TransactionBehavior};
use serde::Serialize;
use std::collections::HashSet;
use std::time::Instant;

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

/// A thread's correlation inputs: its time window and the (lowercased) paths it touched.
#[derive(Debug, Default)]
pub struct ThreadWindow {
    pub thread_id: i64,
    pub start: i64,
    pub end: i64,
    /// Paths named in prose (`file_mentions`).
    pub mentions: HashSet<String>,
    /// Paths its tool calls edited or wrote (0034's `file_edits`, op `edit` / `write`),
    /// relative to its checkout, as git names them.
    pub edits: HashSet<String>,
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
#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CommitLink {
    pub sha: String,
    pub short_sha: String,
    pub subject: Option<String>,
    pub committed_at: i64,
    pub overlap: i64,
}

/// One commit in a project's timeline, grouped across the threads it was inferred from.
#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TimelineRow {
    pub sha: String,
    pub short_sha: String,
    pub subject: Option<String>,
    pub committed_at: i64,
    /// Strongest file-overlap among the linked threads (confidence proxy).
    pub best_overlap: i64,
    /// How many threads link to this commit.
    pub thread_count: i64,
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
/// Also how a fact names a working-set file (`knowledge::working_set`).
pub(crate) fn path_match(mention: &str, file: &str) -> bool {
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

/// A commit file the thread edited or wrote weighs this much in a link's score; one it only
/// named in prose weighs 1. An edit is the stronger signal: a session names dozens of files it
/// never changes, and a sibling session's commit often touches one of them.
const EDIT_WEIGHT: usize = 2;

/// How a commit's files meet a thread: (shared files, score). `shared` is what
/// `thread_commits.overlap` stores (the shared-file count the UI and `cal why` show); `score`
/// weighs each shared file by [`EDIT_WEIGHT`] when the thread edited it, and decides.
fn link_score(t: &ThreadWindow, files: &[String], edits: bool) -> (usize, usize) {
    let (mut shared, mut score) = (0, 0);
    for f in files {
        let fl = f.to_ascii_lowercase();
        if edits && t.edits.iter().any(|e| path_match(e, &fl)) {
            shared += 1;
            score += EDIT_WEIGHT;
        } else if t.mentions.iter().any(|m| path_match(m, &fl)) {
            shared += 1;
            score += 1;
        }
    }
    (shared, score)
}

/// Commits land slightly before, or well after, the discussion that drove them.
const SLACK_BEFORE: i64 = 3_600; // 1h
const MAX_PER_THREAD: usize = 5;

/// What a link needs. [`TUNING`] is the shipped rule; `real_commit_link_precision` sweeps
/// the others against ground truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Tuning {
    /// How long after a thread's last message a commit still counts as its own.
    pub slack_after: i64,
    /// Least [`link_score`] a commit needs.
    pub min_score: usize,
    /// Whether `file_edits` counts (false: prose mentions only).
    pub edits: bool,
}

/// The shipped rule: one shared mentioned file, up to a day after the thread's last message.
/// It stays the rule from before `file_edits` because no rule measured good enough to change
/// it: on the sessions whose transcripts show the commits they made (an APFS clone of a
/// 21,898-thread index, 2026-11-09), every combination of {mentions, mentions + edits} x min
/// score {1, 2} x slack {1 day, 6 h} kept a precision of 0.27 to 0.33 on git's own commit
/// lines and 0.14 to 0.19 on the wider set, far under the 0.8 a link needs before `cal why`,
/// cost per commit and Agent Trace lean on it or a history backfill spreads it. Most wrong
/// links are commits other sessions, or the user, made in the same checkout while the session
/// ran or in the hours after. Table and method: `real_commit_link_precision`.
pub(crate) const TUNING: Tuning = Tuning {
    slack_after: 86_400,
    min_score: 1,
    edits: false,
};

/// Correlate threads to commits: for each thread, commits whose timestamp falls in its (slacked)
/// window AND whose files meet its paths ([`link_score`] at least [`Tuning::min_score`]),
/// best-score-then-most-recent, capped per thread.
pub fn correlate(threads: &[ThreadWindow], commits: &[Commit]) -> Vec<Link> {
    correlate_with(threads, commits, &TUNING)
}

/// [`correlate`] under `tuning`.
pub(crate) fn correlate_with(
    threads: &[ThreadWindow],
    commits: &[Commit],
    tuning: &Tuning,
) -> Vec<Link> {
    let mut out = Vec::new();
    for t in threads {
        let mut cands: Vec<(usize, usize, &Commit)> = commits
            .iter()
            .filter(|c| {
                c.committed_at >= t.start - SLACK_BEFORE
                    && c.committed_at <= t.end + tuning.slack_after
            })
            .filter_map(|c| {
                let (shared, score) = link_score(t, &c.files, tuning.edits);
                (shared > 0 && score >= tuning.min_score).then_some((score, shared, c))
            })
            .collect();
        cands.sort_by(|a, b| b.0.cmp(&a.0).then(b.2.committed_at.cmp(&a.2.committed_at)));
        cands.truncate(MAX_PER_THREAD);
        for (_, shared, c) in cands {
            out.push(Link {
                thread_id: t.thread_id,
                sha: c.sha.clone(),
                short_sha: c.short_sha.clone(),
                subject: c.subject.clone(),
                committed_at: c.committed_at,
                overlap: shared,
            });
        }
    }
    out
}

/// `git` with no console window on Windows (a GUI app spawning a console program would
/// otherwise flash one for every link pass).
pub(crate) fn git_command() -> std::process::Command {
    #[allow(unused_mut)]
    let mut cmd = std::process::Command::new("git");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Drop Windows' verbatim `\\?\` prefix (what `std::fs::canonicalize` returns there), which
/// git and most tools don't accept as a working directory. Unchanged elsewhere.
pub(crate) fn plain_path(p: &str) -> String {
    if let Some(rest) = p.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    p.strip_prefix(r"\\?\").unwrap_or(p).to_string()
}

/// The `git log` over `repo` since `since` (epoch secs) whose output `parse_git_log` reads.
fn git_log_command(repo: &str, since: i64) -> std::process::Command {
    let since_iso = chrono::DateTime::from_timestamp(since.max(0), 0)
        .map(|d| d.to_rfc3339())
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string());
    let mut cmd = git_command();
    cmd.arg("-C")
        .arg(plain_path(repo))
        .arg("log")
        .arg("--no-merges")
        .arg(format!("--since={since_iso}"))
        .arg("--name-only")
        .arg(format!("--pretty=format:{RS}COMMIT{US}%H{US}%ct{US}%s"));
    cmd
}

/// Run `git log` over `repo` since `since` (epoch secs), returning raw output for `parse_git_log`.
fn run_git_log(repo: &str, since: i64) -> Result<String> {
    let out = git_log_command(repo, since)
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

/// Each top-level thread's first and last message time for a canonical project key.
/// Index-only through 0031's (thread_id, ts, role) index: 53k page reads down to 1k for a
/// big project from a cold cache.
const THREAD_WINDOWS_SQL: &str = "SELECT t.id, MIN(m.ts), MAX(m.ts)
     FROM threads t JOIN messages m ON m.thread_id = t.id
     WHERE COALESCE(t.project_key, t.project_path) = ?1 AND m.ts IS NOT NULL
       AND t.is_subagent = 0
     GROUP BY t.id";

/// A thread's paths: the ones named in prose (kind 0) and, from 0034 on, the ones its tool
/// calls edited or wrote (kind 1). Never `file_edits` op `mention`: those rows repeat
/// `file_mentions`, and op `read` is a file looked at, not changed. Both arms are index
/// searches on the thread (`file_mentions`' primary key, `idx_file_edits_thread`).
const THREAD_PATHS_SQL: &str = "SELECT path, 0 FROM file_mentions WHERE thread_id = ?1
     UNION SELECT path, 1 FROM file_edits WHERE thread_id = ?1 AND op IN ('edit', 'write')";
/// [`THREAD_PATHS_SQL`] on an index from before 0034, which has no `file_edits`.
const THREAD_MENTIONS_SQL: &str = "SELECT path, 0 FROM file_mentions WHERE thread_id = ?1";

/// The statement that reads a thread's paths: with its edits when `edits` (a [`Tuning`] that
/// counts them) and `conn`'s schema has `file_edits`, else its mentions only.
fn thread_paths_sql(conn: &Connection, edits: bool) -> &'static str {
    if edits && crate::db::has_table(conn, "file_edits") {
        THREAD_PATHS_SQL
    } else {
        THREAD_MENTIONS_SQL
    }
}

/// Fill `w`'s mentions and edits from `stmt` (a prepared [`thread_paths_sql`]), lowercased.
/// False when the thread has no path at all (nothing to correlate on).
fn fill_paths(stmt: &mut rusqlite::CachedStatement<'_>, w: &mut ThreadWindow) -> Result<bool> {
    let rows = stmt.query_map([w.thread_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    })?;
    for (path, kind) in rows.filter_map(rusqlite::Result::ok) {
        let path = path.to_ascii_lowercase();
        if kind == 1 {
            w.edits.insert(path);
        } else {
            w.mentions.insert(path);
        }
    }
    Ok(!w.mentions.is_empty() || !w.edits.is_empty())
}

/// Load every thread's time window + paths (with its edits when `edits`) for a canonical
/// project key.
fn thread_windows(conn: &Connection, project_key: &str, edits: bool) -> Result<Vec<ThreadWindow>> {
    // Only top-level threads: a subagent transcript's work is attributed to its parent session,
    // and subagent prompts (e.g. orchestration scouts) mention many files, which over-links.
    let mut stmt = conn.prepare(THREAD_WINDOWS_SQL)?;
    let rows: Vec<(i64, i64, i64)> = stmt
        .query_map([project_key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;

    let mut windows = Vec::with_capacity(rows.len());
    let mut paths = conn.prepare_cached(thread_paths_sql(conn, edits))?;
    for (id, start, end) in rows {
        let mut w = ThreadWindow {
            thread_id: id,
            start,
            end,
            ..Default::default()
        };
        if fill_paths(&mut paths, &mut w)? {
            windows.push(w);
        }
    }
    Ok(windows)
}

/// Compute + persist thread↔commit links for the git repo at `repo_path`. Returns the number of
/// links stored. Re-runs cleanly: it replaces the links for the threads it reconsiders.
pub fn link_project(conn: &Connection, repo_path: &str) -> Result<usize> {
    let key = crate::knowledge::project_key(repo_path);
    let windows = thread_windows(conn, &key, TUNING.edits)?;
    if windows.is_empty() {
        return Ok(0);
    }
    let earliest = windows.iter().map(|w| w.start).min().unwrap_or(0);
    let commits = parse_git_log(&run_git_log(repo_path, earliest - SLACK_BEFORE)?);
    let links = correlate(&windows, &commits);

    let now = chrono::Utc::now().timestamp();
    // One IMMEDIATE transaction for the replace: a reader (the app's timeline) never sees the
    // project with zero links mid-run, and 600 links commit once instead of 600 times (about
    // 250 ms of write-lock time down to a few ms). git ran above, outside the lock.
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    // Drop only the project's links to commits this log contains that no longer match, then
    // reinsert the fresh set. A link to a commit outside this log (one found by a worktree
    // scan, which this checkout's `git log` never walks) is kept: clearing everything used to
    // wipe those links on every whole-repo scan.
    {
        let fresh: HashSet<(i64, String)> =
            links.iter().map(|l| (l.thread_id, l.sha.clone())).collect();
        let in_log: HashSet<&str> = commits.iter().map(|c| c.sha.as_str()).collect();
        let stale: Vec<(i64, String)> = {
            let mut sel = tx.prepare(
                "SELECT thread_id, sha FROM thread_commits WHERE thread_id IN (
                    SELECT id FROM threads WHERE COALESCE(project_key, project_path) = ?1
                 )",
            )?;
            // Collected first: the row iterator borrows `sel`, which ends with this block,
            // while the block's value outlives it.
            let rows = sel
                .query_map([&key], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter()
                .filter(|(tid, sha)| {
                    in_log.contains(sha.as_str()) && !fresh.contains(&(*tid, sha.clone()))
                })
                .collect()
        };
        let mut del = tx.prepare("DELETE FROM thread_commits WHERE thread_id = ?1 AND sha = ?2")?;
        for (tid, sha) in &stale {
            del.execute(rusqlite::params![tid, sha])?;
        }
    }
    {
        let mut ins = tx.prepare(
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
    }
    tx.commit()?;
    Ok(links.len())
}

/// [`run_git_log`] that gives up at `deadline`: None when git had not finished by then (the
/// process is killed). For the Today view, which must answer in bounded time however big
/// the repo is.
pub(crate) fn run_git_log_until(
    repo: &str,
    since: i64,
    deadline: Instant,
) -> Result<Option<String>> {
    let out = run_until(git_log_command(repo, since), deadline)?;
    match out {
        Some((true, stdout)) => Ok(Some(stdout)),
        Some((false, _)) => bail!("git log failed in {repo}"),
        None => Ok(None),
    }
}

/// `git -C dir <args>` that gives up at `deadline`: its trimmed stdout, None when git had not
/// finished by then (the process is killed), an error when it exited non-zero. For the
/// watcher's probes, which run on the thread that indexes: a git hung on a sleeping network
/// mount or a slow fsmonitor hook must not stall indexing.
pub(crate) fn run_git_until(dir: &str, args: &[&str], deadline: Instant) -> Result<Option<String>> {
    let mut cmd = git_command();
    cmd.arg("-C").arg(plain_path(dir)).args(args);
    match run_until(cmd, deadline)? {
        Some((true, stdout)) => Ok(Some(stdout.trim().to_string())),
        Some((false, _)) => bail!("git {} failed in {dir}", args.join(" ")),
        None => Ok(None),
    }
}

/// Run `cmd` to completion or `deadline`, whichever comes first: Some((success, stdout)), or
/// None when it was still running at the deadline (it is killed and reaped).
fn run_until(mut cmd: std::process::Command, deadline: Instant) -> Result<Option<(bool, String)>> {
    use std::io::Read;
    use std::process::Stdio;
    if Instant::now() >= deadline {
        return Ok(None);
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("running git: {e} (is git installed?)"))?;
    // Drain stdout on a helper thread: a log bigger than the pipe buffer would otherwise
    // block git on write and it would never exit.
    let mut stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    loop {
        let polled = match child.try_wait() {
            Ok(polled) => polled,
            Err(e) => {
                // Reap git before giving up, or it and the reader thread keep running.
                let _ = child.kill();
                let _ = child.wait();
                return Err(anyhow::anyhow!("waiting for git: {e}"));
            }
        };
        if let Some(status) = polled {
            let out = reader.join().unwrap_or_default();
            return Ok(Some((
                status.success(),
                String::from_utf8_lossy(&out).into_owned(),
            )));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// First and last message time of one thread, from the (thread_id, ts, role) index.
pub(crate) const THREAD_WINDOW_SQL: &str =
    "SELECT MIN(ts), MAX(ts) FROM messages WHERE thread_id = ?1 AND ts IS NOT NULL";

/// [`link_project`] for a few threads of the checkout at `repo_path` (the Today view's
/// recent sessions), so `git log` reads only their window instead of the project's whole
/// history. Adds the links found and drops only these threads' links to commits that this
/// log holds and that no longer match; a link to a commit outside it (found by
/// `link_thread_commits` in a worktree, whose branch this checkout's log does not walk) is
/// kept. None when `deadline` passed before git finished (nothing is written then).
pub fn link_threads(
    conn: &Connection,
    repo_path: &str,
    thread_ids: &[i64],
    deadline: Instant,
) -> Result<Option<usize>> {
    link_threads_with(conn, repo_path, thread_ids, deadline, run_git_log_until)
}

/// [`link_threads`] with the `git log` runner injected: (repo, since, deadline) to the log,
/// or None past the deadline. Tests pass a fake log.
pub(crate) fn link_threads_with(
    conn: &Connection,
    repo_path: &str,
    thread_ids: &[i64],
    deadline: Instant,
    mut git_log: impl FnMut(&str, i64, Instant) -> Result<Option<String>>,
) -> Result<Option<usize>> {
    let mut windows = Vec::new();
    {
        let mut win = conn.prepare_cached(THREAD_WINDOW_SQL)?;
        let mut paths = conn.prepare_cached(thread_paths_sql(conn, TUNING.edits))?;
        for &id in thread_ids {
            let (start, end): (Option<i64>, Option<i64>) =
                win.query_row([id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            let (Some(start), Some(end)) = (start, end) else {
                continue;
            };
            let mut w = ThreadWindow {
                thread_id: id,
                start,
                end,
                ..Default::default()
            };
            if fill_paths(&mut paths, &mut w)? {
                windows.push(w);
            }
        }
    }
    if windows.is_empty() {
        return Ok(Some(0));
    }
    let earliest = windows.iter().map(|w| w.start).min().unwrap_or(0);
    let Some(log) = git_log(repo_path, earliest - SLACK_BEFORE, deadline)? else {
        return Ok(None);
    };
    let commits = parse_git_log(&log);
    let links = correlate(&windows, &commits);
    let in_log: HashSet<&str> = commits.iter().map(|c| c.sha.as_str()).collect();
    let fresh: HashSet<(i64, &str)> = links
        .iter()
        .map(|l| (l.thread_id, l.sha.as_str()))
        .collect();
    let now = chrono::Utc::now().timestamp();
    // git ran above, outside the write lock; the update is one short transaction.
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    {
        let mut existing = tx.prepare("SELECT sha FROM thread_commits WHERE thread_id = ?1")?;
        let mut del = tx.prepare("DELETE FROM thread_commits WHERE thread_id = ?1 AND sha = ?2")?;
        for w in &windows {
            let stale: Vec<String> = existing
                .query_map([w.thread_id], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .filter(|sha| {
                    in_log.contains(sha.as_str()) && !fresh.contains(&(w.thread_id, sha.as_str()))
                })
                .collect();
            for sha in stale {
                del.execute(rusqlite::params![w.thread_id, sha])?;
            }
        }
        let mut ins = tx.prepare(
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
    }
    tx.commit()?;
    Ok(Some(links.len()))
}

/// One thread inferred to have produced a given commit (the inverse of `linked_commits`).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitThread {
    pub sha: String,
    pub thread_id: i64,
    pub title: Option<String>,
    /// Shared-file count between the thread and the commit (confidence proxy, raw not normalized).
    pub overlap: i64,
}

/// The inverse of `linked_commits`: for each of `shas`, the thread(s) inferred to have produced it,
/// most-overlap first. SHAs with no link are simply absent (the caller fills the null). Requires
/// `link_project` to have run for the repo first. Matches full OR abbreviated SHAs: each input is
/// matched as a prefix of the stored full 40-char sha, so the short hashes that `git log --oneline`
/// / PR tooling hand to `cal audit-pr` resolve instead of silently returning nothing.
pub fn commits_by_sha(conn: &Connection, shas: &[String]) -> Result<Vec<CommitThread>> {
    // Normalize: git SHAs are lowercase hex; trim and drop empties so a blank never matches all.
    let needles: Vec<String> = shas
        .iter()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if needles.is_empty() {
        return Ok(Vec::new());
    }
    // Prefix-match each input against the stored full sha. SHAs are [0-9a-f], so no LIKE
    // metacharacters (`%`/`_`) can appear in a needle — concatenating '%' is safe.
    let clauses = (1..=needles.len())
        .map(|i| format!("tc.sha LIKE ?{i} || '%'"))
        .collect::<Vec<_>>()
        .join(" OR ");
    let sql = format!(
        "SELECT tc.sha, tc.thread_id, t.title, tc.overlap
         FROM thread_commits tc JOIN threads t ON t.id = tc.thread_id
         WHERE {clauses}
         ORDER BY tc.overlap DESC, tc.committed_at DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(needles.iter()), |r| {
        Ok(CommitThread {
            sha: r.get(0)?,
            thread_id: r.get(1)?,
            title: r.get(2)?,
            overlap: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
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
        "SELECT tc.sha, tc.short_sha, tc.subject, tc.committed_at,
                MAX(tc.overlap) AS best_overlap, COUNT(*) AS thread_count
         FROM thread_commits tc JOIN threads t ON t.id = tc.thread_id
         WHERE COALESCE(t.project_key, t.project_path) = ?1
         GROUP BY tc.sha
         ORDER BY tc.committed_at DESC, tc.sha
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![project_key, limit as i64], |r| {
        Ok(TimelineRow {
            sha: r.get(0)?,
            short_sha: r.get(1)?,
            subject: r.get(2)?,
            committed_at: r.get(3)?,
            best_overlap: r.get(4)?,
            thread_count: r.get(5)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    fn temp_db() -> crate::db::test_support::TestIndex {
        crate::db::test_support::TestIndex::new("gl")
    }

    fn commit(sha: &str, at: i64, files: &[&str]) -> Commit {
        Commit {
            sha: sha.into(),
            short_sha: sha.into(),
            committed_at: at,
            subject: sha.into(),
            files: files.iter().map(|f| f.to_string()).collect(),
        }
    }

    fn window(mentions: &[&str], edits: &[&str]) -> ThreadWindow {
        ThreadWindow {
            thread_id: 1,
            start: 1_000,
            end: 2_000,
            mentions: mentions.iter().map(|p| p.to_string()).collect(),
            edits: edits.iter().map(|p| p.to_string()).collect(),
        }
    }

    /// The shipped rule is still the one from before `file_edits` (the precision gate kept
    /// it); a tuning that counts edits weighs an edited file double, so with min score 2 an
    /// edit links alone while a single mentioned file does not.
    #[test]
    fn edits_weigh_double_only_under_a_tuning_that_counts_them() {
        let edits_2 = Tuning {
            slack_after: TUNING.slack_after,
            min_score: 2,
            edits: true,
        };
        let one_file = [commit("c1", 1_500, &["src/auth.rs", "README.md"])];
        let edited = [window(&[], &["src/auth.rs"])];
        assert!(correlate(&edited, &one_file).is_empty());
        let links = correlate_with(&edited, &one_file, &edits_2);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].overlap, 1, "overlap stays the shared-file count");

        let mentioned = [window(&["src/auth.rs"], &[])];
        assert_eq!(correlate(&mentioned, &one_file).len(), 1);
        assert!(correlate_with(&mentioned, &one_file, &edits_2).is_empty());
        let two = [window(&["src/auth.rs", "readme.md"], &[])];
        assert_eq!(correlate_with(&two, &one_file, &edits_2)[0].overlap, 2);

        // An edit outranks a mention: the commit touching the edited file comes first.
        let both = [window(&["docs/a.md"], &["src/b.rs"])];
        let commits = [
            commit("mention", 1_900, &["docs/a.md"]),
            commit("edit", 1_100, &["src/b.rs"]),
        ];
        let tuning = Tuning {
            min_score: 1,
            ..edits_2
        };
        let order: Vec<String> = correlate_with(&both, &commits, &tuning)
            .into_iter()
            .map(|l| l.sha)
            .collect();
        assert_eq!(order, ["edit", "mention"]);
    }

    /// A thread's paths: its mentions, and its edit / write rows only (never read or
    /// mention), each arm off the thread's index; an index from before 0034 reads mentions.
    #[test]
    fn thread_paths_read_edits_off_the_thread_index() {
        let conn = temp_db();
        conn.execute(
            "INSERT INTO threads (id, source_id, external_id) VALUES (7, 1, 'tp')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO file_mentions (thread_id, path) VALUES (7, 'Docs/A.md')",
            [],
        )
        .unwrap();
        for (path, op) in [
            ("src/b.rs", "edit"),
            ("src/c.rs", "write"),
            ("src/d.rs", "read"),
            ("src/e.rs", "mention"),
        ] {
            conn.execute(
                "INSERT INTO file_edits (thread_id, path, op) VALUES (7, ?1, ?2)",
                [path, op],
            )
            .unwrap();
        }
        let mut stmt = conn.prepare_cached(thread_paths_sql(&conn, true)).unwrap();
        let mut w = ThreadWindow {
            thread_id: 7,
            ..Default::default()
        };
        assert!(fill_paths(&mut stmt, &mut w).unwrap());
        assert_eq!(w.mentions, HashSet::from(["docs/a.md".to_string()]));
        assert_eq!(
            w.edits,
            HashSet::from(["src/b.rs".to_string(), "src/c.rs".to_string()])
        );
        assert_eq!(thread_paths_sql(&conn, false), THREAD_MENTIONS_SQL);

        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {THREAD_PATHS_SQL}"))
            .unwrap()
            .query_map([7], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let plan = plan.join(" | ");
        assert!(
            plan.contains("SEARCH file_mentions USING COVERING INDEX")
                && plan.contains("(thread_id=?)")
                && plan
                    .contains("SEARCH file_edits USING INDEX idx_file_edits_thread (thread_id=?)"),
            "{plan}"
        );

        let old = crate::db::test_support::TestPath::new("gl_old");
        crate::db::test_support::index_at_version(&old, 33, |_| {});
        let old = crate::db::open_readonly(&old).unwrap();
        assert_eq!(thread_paths_sql(&old, true), THREAD_MENTIONS_SQL);
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
            ..Default::default()
        }];
        let commits = vec![
            // In-window, file overlaps -> linked.
            Commit {
                sha: "in".into(),
                short_sha: "in".into(),
                committed_at: 2_500, // within end + TUNING.slack_after
                subject: "the one".into(),
                files: vec!["src/auth.rs".into(), "README.md".into()],
            },
            // Overlaps but WAY outside the window -> not linked.
            Commit {
                sha: "late".into(),
                short_sha: "late".into(),
                committed_at: 2_000 + TUNING.slack_after + 10,
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
    fn commits_by_sha_matches_abbreviated_and_full() {
        use crate::indexer::{source_id, upsert_thread, ParsedMessage, ParsedThread};

        let mut conn = temp_db();
        let sid = source_id(&conn, "claude_code").unwrap();
        upsert_thread(
            &mut conn,
            sid,
            &ParsedThread {
                external_id: "t-sha".into(),
                title: Some("commit producer".into()),
                project_path: Some("/tmp/repo".into()),
                messages: vec![ParsedMessage {
                    role: "user".into(),
                    text: "did the thing".into(),
                    tool_name: None,
                    ts: Some(1_500),
                }],
                ..Default::default()
            },
        )
        .unwrap();
        let tid: i64 = conn
            .query_row(
                "SELECT id FROM threads WHERE external_id = 't-sha'",
                [],
                |r| r.get(0),
            )
            .unwrap();

        // Stored as the full 40-char sha, exactly as link_project writes it.
        let full = "eb80a83cafef00dd00aa11bb22cc33dd44ee55ff";
        conn.execute(
            "INSERT INTO thread_commits
                (thread_id, sha, short_sha, subject, committed_at, overlap, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![tid, full, &full[..10], "do thing", 1_600, 2_i64, 1_700],
        )
        .unwrap();

        // The 7-char form `git log --oneline` hands to `cal audit-pr` must resolve.
        let short = commits_by_sha(&conn, &["eb80a83".into()]).unwrap();
        assert_eq!(short.len(), 1);
        assert_eq!(short[0].sha, full);
        assert_eq!(short[0].thread_id, tid);
        // Uppercase input is normalized to lowercase hex.
        assert_eq!(commits_by_sha(&conn, &["EB80A83".into()]).unwrap().len(), 1);
        // The full sha still matches.
        assert_eq!(commits_by_sha(&conn, &[full.to_string()]).unwrap().len(), 1);
        // A non-matching prefix yields nothing (no false positives).
        assert!(commits_by_sha(&conn, &["deadbee".into()])
            .unwrap()
            .is_empty());
        // Blank/empty inputs never match everything.
        assert!(commits_by_sha(&conn, &["".into(), "   ".into()])
            .unwrap()
            .is_empty());
    }

    /// The per-thread MIN/MAX(ts) must come from the (thread_id, ts, role) index alone, not a
    /// read of every message row in the project.
    #[test]
    fn thread_windows_are_index_only() {
        let conn = temp_db();
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {THREAD_WINDOWS_SQL}"))
            .unwrap()
            .query_map(["/p"], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let plan = plan.join(" | ");
        assert!(
            plan.contains("COVERING INDEX idx_messages_thread_ts (thread_id=?"),
            "{plan}"
        );
    }

    #[test]
    fn plain_path_strips_windows_verbatim_prefixes() {
        assert_eq!(plain_path(r"\\?\C:\Users\me\proj"), r"C:\Users\me\proj");
        assert_eq!(plain_path(r"\\?\UNC\server\share\x"), r"\\server\share\x");
        assert_eq!(plain_path("/Users/me/proj"), "/Users/me/proj");
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
        let key = crate::knowledge::project_key(&repo);
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

        // Inverse lookup (cal audit-pr): the commit's sha maps back to its originating thread.
        let inv = commits_by_sha(&conn, &[links[0].sha.clone()]).unwrap();
        assert_eq!(inv.len(), 1);
        assert_eq!(inv[0].thread_id, tid);
        assert_eq!(inv[0].title.as_deref(), Some("auth work"));
        assert!(inv[0].overlap >= 1);
        // A sha with no link returns nothing (the caller fills the explicit null).
        assert!(commits_by_sha(&conn, &["deadbeef".into()])
            .unwrap()
            .is_empty());
        assert!(commits_by_sha(&conn, &[]).unwrap().is_empty());

        let tl = commit_timeline(&conn, &key, 10).unwrap();
        assert_eq!(tl.len(), 1, "one commit in the timeline");
        assert_eq!(tl[0].thread_count, 1);
        assert_eq!(tl[0].subject.as_deref(), Some("wire auth"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The short shas of git's own commit lines in a tool output: `[<branch> <sha>] <subject>`
    /// (`[main (root-commit) <sha>]`, `[detached HEAD <sha>]` too) with git's stat line
    /// (`N file(s) changed`) within the next three lines, so a bracketed line in a log or a
    /// file is not taken for one.
    fn commit_lines(text: &str) -> Vec<String> {
        let lines: Vec<&str> = text.lines().collect();
        let mut out = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            let Some(rest) = line.trim_start().strip_prefix('[') else {
                continue;
            };
            let Some((inside, subject)) = rest.split_once("] ") else {
                continue;
            };
            let Some((branch, sha)) = inside.rsplit_once(' ') else {
                continue;
            };
            let hex = (7..=40).contains(&sha.len())
                && sha
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
            let stat = lines[i + 1..]
                .iter()
                .take(3)
                .any(|l| l.contains(" file changed") || l.contains(" files changed"));
            if hex && !branch.trim().is_empty() && !subject.trim().is_empty() && stat {
                out.push(sha.to_string());
            }
        }
        out
    }

    #[test]
    fn commit_lines_read_only_gits_own_line() {
        let out = "[main 1a2b3c4] fix: the thing\n 2 files changed, 3 insertions(+)\n\
                   [feat/x (root-commit) abcdef12] init\n 1 file changed\n\
                   [not a commit] text\n 1 file changed\n\
                   [main 1a2b3c4d] no stat line follows\nsomething else\n";
        assert_eq!(commit_lines(out), vec!["1a2b3c4", "abcdef12"]);
    }

    /// Batch L1's precision gate for `correlate`, on an APFS clone of a real index (read-only)
    /// and `git log` in the checkouts its sessions ran in (read-only). Prints counts, ids,
    /// short shas, commit subjects and overlaps; never transcript text or paths.
    ///
    /// Ground truth: the sessions whose tool output shows git's own `[<branch> <sha>]` line
    /// (a subagent's counts for its session), plus, as a wider second set, the sessions whose
    /// `git commit` call printed exactly one sha of a commit made while the session ran. A
    /// link from one of these sessions is right when its sha is one the session made; a
    /// sibling's is one another ground-truth session made. Precision counts every other link
    /// as wrong, so it is a floor (a commit the session made without printing it counts
    /// against it); "own by call time" also counts a commit the session made within ten
    /// minutes of calling `git commit`. Then the two projects with the most sessions: links
    /// per rule, and 15 sampled links each for a hand check.
    ///
    /// Measured 2026-11-09 on a clone of a 21,898-thread index (1,788 top-level; 33 stored
    /// links): git's commit line gives 5 sessions and 12 shas, 3 sessions and 8 commits in a
    /// checkout still on disk; the wider set 22 sessions and 30 commits. Precision / own by
    /// call time / recall:
    ///
    /// | paths | min score | slack | commit line (3 sessions) | wider set (22 sessions) |
    /// |---|---|---|---|---|
    /// | mentions | 1 | 1 d | 0.33 / 0.50 / 0.25 | 0.14 / 0.21 / 0.40 |
    /// | mentions | 1 | 6 h | 0.33 / 0.50 / 0.25 | 0.16 / 0.25 / 0.43 |
    /// | mentions | 2 | 1 d | 0.33 / 0.50 / 0.25 | 0.14 / 0.20 / 0.37 |
    /// | mentions | 2 | 6 h | 0.33 / 0.50 / 0.25 | 0.15 / 0.22 / 0.37 |
    /// | mentions + edits | 1 | 1 d | 0.27 / 0.45 / 0.38 | 0.14 / 0.23 / 0.47 |
    /// | mentions + edits | 1 | 6 h | 0.27 / 0.55 / 0.38 | 0.19 / 0.30 / 0.60 |
    /// | mentions + edits | 2 | 1 d | 0.27 / 0.45 / 0.38 | 0.14 / 0.22 / 0.43 |
    /// | mentions + edits | 2 | 6 h | 0.27 / 0.55 / 0.38 | 0.18 / 0.29 / 0.53 |
    ///
    /// No rule reaches the 0.8 gate, so [`TUNING`] did not change. Only 1 of the wider set's
    /// wrong links is another ground-truth session's commit; the rest are commits made in the
    /// same checkout during the session (56 of 93 for mentions + edits, 1, 6 h) or after it
    /// (18), by sessions outside the set or the user: on the busiest project 79 of 228 linked
    /// commits are claimed by 3 or more sessions. Filtering the per-link numbers
    /// of mentions + edits, 1, 1 d (`CALLIMACHUS_LINK_DETAIL=1`) does not close the gap
    /// either: keeping only commits whose files the session touched for 90% or more leaves 19
    /// of 97 links at 0.42 (0.53 own by call time). The hand check of 30 sampled links
    /// (mentions + edits, 1, 6 h) found about 7 clearly right, 9 plausible and 14 unrelated.
    ///
    /// CALLIMACHUS_GROUNDING_DB=<an APFS clone of index.db> cargo test -p callimachus-core \
    ///   --lib -- --ignored real_commit_link_precision --nocapture
    #[test]
    #[ignore]
    fn real_commit_link_precision() {
        use std::collections::{BTreeMap, HashMap};
        let Ok(path) = std::env::var("CALLIMACHUS_GROUNDING_DB") else {
            eprintln!("set CALLIMACHUS_GROUNDING_DB to an index clone");
            return;
        };
        let conn = crate::db::open_readonly(std::path::Path::new(&path)).unwrap();
        let parents: HashMap<i64, i64> = conn
            .prepare(
                "SELECT c.id, p.id FROM threads c JOIN threads p ON p.source_id = c.source_id
                  AND p.external_id = substr(c.external_id, 1,
                      instr(c.external_id, '/subagents/') - 1) || '.jsonl'
                 WHERE c.is_subagent = 1 AND instr(c.external_id, '/subagents/') > 0",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let top = |id: i64| parents.get(&id).copied().unwrap_or(id);

        // Ground truth 1: git's commit line.
        let mut line_gt: BTreeMap<i64, HashSet<String>> = BTreeMap::new();
        {
            let mut stmt = conn
                .prepare(
                    "SELECT m.thread_id, m.text FROM messages_fts
                     JOIN messages m ON m.id = messages_fts.rowid
                     WHERE messages_fts MATCH '\"files changed\" OR \"file changed\"'
                       AND m.role = 'tool'",
                )
                .unwrap();
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
                .unwrap();
            for (tid, text) in rows.filter_map(Result::ok) {
                for sha in commit_lines(&text) {
                    line_gt.entry(top(tid)).or_default().insert(sha);
                }
            }
        }
        // Ground truth 2: the hex words the output of a `git commit` call printed, verified
        // against the log below.
        let mut cmd_gt: BTreeMap<i64, Vec<HashSet<String>>> = BTreeMap::new();
        // When each session called `git commit` (its own commits land within minutes after).
        let mut commit_calls: HashMap<i64, Vec<i64>> = HashMap::new();
        {
            let mut stmt = conn
                .prepare(
                    "SELECT m.thread_id, m.seq, m.text, COALESCE(m.ts, 0) FROM messages_fts
                     JOIN messages m ON m.id = messages_fts.rowid
                     WHERE messages_fts MATCH 'commit' AND m.role = 'assistant'
                       AND m.tool_name IS NOT NULL",
                )
                .unwrap();
            let calls: Vec<(i64, i64, String, i64)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .unwrap()
                .filter_map(Result::ok)
                .collect();
            let mut result = conn
                .prepare(
                    "SELECT text FROM messages WHERE thread_id = ?1 AND seq > ?2
                       AND role = 'tool' ORDER BY seq LIMIT 1",
                )
                .unwrap();
            for (tid, seq, text, ts) in calls {
                let t = text.to_ascii_lowercase();
                if !(t.contains("git commit") || (t.contains("git -c ") && t.contains(" commit"))) {
                    continue;
                }
                commit_calls.entry(top(tid)).or_default().push(ts);
                let Ok(out) =
                    result.query_row(rusqlite::params![tid, seq], |r| r.get::<_, String>(0))
                else {
                    continue;
                };
                let words: HashSet<String> = out
                    .split(|c: char| !c.is_ascii_alphanumeric())
                    .filter(|w| {
                        (7..=40).contains(&w.len())
                            && w.bytes()
                                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                            && w.bytes().any(|b| b.is_ascii_digit())
                            && w.bytes().any(|b| b.is_ascii_alphabetic())
                    })
                    .map(str::to_string)
                    .collect();
                if !words.is_empty() {
                    cmd_gt.entry(top(tid)).or_default().push(words);
                }
            }
        }

        // Each ground-truth session, by the checkout its commits land in.
        let mut by_checkout: BTreeMap<String, Vec<(i64, i64, i64)>> = BTreeMap::new();
        let mut no_checkout = 0;
        let ids: std::collections::BTreeSet<i64> =
            line_gt.keys().chain(cmd_gt.keys()).copied().collect();
        for &id in &ids {
            let (path, key): (String, String) = conn
                .query_row(
                    "SELECT COALESCE(project_path, ''), COALESCE(project_key, project_path, '')
                     FROM threads WHERE id = ?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            let (start, end): (Option<i64>, Option<i64>) = conn
                .query_row(THREAD_WINDOW_SQL, [id], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap();
            match (crate::jobs::checkout_for(&path, &key), start, end) {
                (Some(repo), Some(s), Some(e)) => {
                    by_checkout.entry(repo).or_default().push((id, s, e))
                }
                _ => no_checkout += 1,
            }
        }

        let configs: Vec<(&str, Tuning)> = [("mentions", false), ("mentions+edits", true)]
            .into_iter()
            .flat_map(|(name, edits)| {
                [1, 2].into_iter().flat_map(move |min_score| {
                    [(86_400, "1d"), (21_600, "6h")]
                        .into_iter()
                        .map(move |(slack_after, _)| {
                            (
                                name,
                                Tuning {
                                    slack_after,
                                    min_score,
                                    edits,
                                },
                            )
                        })
                })
            })
            .collect();

        // Two ground truths: git's commit line alone (the gate), and with the shas `git commit`
        // calls printed. Per set and config: links, right, a sibling's, other in the session's
        // window, other after it, and right or made by the session by call time (it called
        // `git commit` in the ten minutes before: a commit it made without printing the sha).
        let mut tally = vec![vec![[0usize; 6]; configs.len()]; 2];
        let mut gt = [[0usize; 2]; 2];
        let mut unreachable = 0;
        let mut paths = conn.prepare_cached(thread_paths_sql(&conn, true)).unwrap();
        let detail = std::env::var_os("CALLIMACHUS_LINK_DETAIL").is_some();
        for (rank, (repo, threads)) in by_checkout.iter().enumerate() {
            let earliest = threads.iter().map(|t| t.1).min().unwrap();
            let Ok(log) = run_git_log(repo, earliest - SLACK_BEFORE) else {
                no_checkout += threads.len();
                continue;
            };
            let commits = parse_git_log(&log);
            let resolve = |prefix: &str| {
                commits
                    .iter()
                    .find(|c| c.sha.starts_with(prefix))
                    .map(|c| (c.sha.clone(), c.committed_at))
            };
            let mut truths: [HashMap<i64, HashSet<String>>; 2] = Default::default();
            for &(id, start, end) in threads {
                let mut line = HashSet::new();
                for p in line_gt.get(&id).into_iter().flatten() {
                    match resolve(p) {
                        Some((sha, _)) => {
                            line.insert(sha);
                        }
                        None => unreachable += 1,
                    }
                }
                let mut all = line.clone();
                for words in cmd_gt.get(&id).into_iter().flatten() {
                    let hits: HashSet<String> = words
                        .iter()
                        .filter_map(|w| resolve(w))
                        .filter(|(_, at)| *at >= start - 600 && *at <= end + 600)
                        .map(|(sha, _)| sha)
                        .collect();
                    if hits.len() == 1 {
                        all.extend(hits);
                    }
                }
                if !line.is_empty() {
                    truths[0].insert(id, line);
                }
                if !all.is_empty() {
                    truths[1].insert(id, all);
                }
            }
            if !truths[1].is_empty() {
                eprintln!(
                    "checkout #{rank}: {} + {} ground-truth sessions, {} commits in the log",
                    truths[0].len(),
                    truths[1].len() - truths[0].len(),
                    commits.len()
                );
            }
            for (set, truth) in truths.iter().enumerate() {
                if truth.is_empty() {
                    continue;
                }
                gt[set][0] += truth.len();
                gt[set][1] += truth.values().map(HashSet::len).sum::<usize>();
                let windows: Vec<ThreadWindow> = threads
                    .iter()
                    .filter(|t| truth.contains_key(&t.0))
                    .filter_map(|&(id, start, end)| {
                        let mut w = ThreadWindow {
                            thread_id: id,
                            start,
                            end,
                            ..Default::default()
                        };
                        fill_paths(&mut paths, &mut w).unwrap().then_some(w)
                    })
                    .collect();
                for (i, (_, tuning)) in configs.iter().enumerate() {
                    for l in correlate_with(&windows, &commits, tuning) {
                        let w = windows.iter().find(|w| w.thread_id == l.thread_id).unwrap();
                        let slot = if truth[&l.thread_id].contains(&l.sha) {
                            1
                        } else if truth.values().any(|s| s.contains(&l.sha)) {
                            2
                        } else if l.committed_at <= w.end {
                            3
                        } else {
                            4
                        };
                        let call = commit_calls
                            .get(&l.thread_id)
                            .into_iter()
                            .flatten()
                            .filter(|ts| **ts <= l.committed_at + 60)
                            .map(|ts| l.committed_at - ts)
                            .min();
                        let t = &mut tally[set][i];
                        t[0] += 1;
                        t[slot] += 1;
                        t[5] += usize::from(slot == 1 || call.is_some_and(|d| d <= 600));
                        if !(detail && set == 1 && i == 4) {
                            continue;
                        }
                        // Per link of mentions+edits, min 1, 1 day: numbers only.
                        let c = commits.iter().find(|c| c.sha == l.sha).unwrap();
                        let (_, score) = link_score(w, &c.files, true);
                        let near: i64 = conn
                            .query_row(
                                "SELECT MIN(ABS(ts - ?2)) FROM messages WHERE thread_id = ?1
                                   AND ts IS NOT NULL",
                                rusqlite::params![l.thread_id, l.committed_at],
                                |r| r.get(0),
                            )
                            .unwrap();
                        let claimed = windows
                            .iter()
                            .filter(|o| o.thread_id != l.thread_id)
                            .filter(|o| {
                                c.committed_at >= o.start - SLACK_BEFORE
                                    && c.committed_at <= o.end + tuning.slack_after
                                    && link_score(o, &c.files, true).0 > 0
                            })
                            .count();
                        eprintln!(
                            "detail {} t{} {} win {:.1}h from_start {:.0}m past_end {:.1}h \
                             near {:.0}m call {:?}m shared {} score {} files {} edits {} \
                             others {}",
                            ["", "RIGHT", "SIBLING", "INWIN", "AFTER"][slot],
                            l.thread_id,
                            &l.short_sha[..7],
                            (w.end - w.start) as f64 / 3600.0,
                            (l.committed_at - w.start) as f64 / 60.0,
                            (l.committed_at - w.end) as f64 / 3600.0,
                            near as f64 / 60.0,
                            call.map(|d| d / 60),
                            l.overlap,
                            score,
                            c.files.len(),
                            w.edits.len(),
                            claimed
                        );
                    }
                }
            }
        }
        eprintln!(
            "{unreachable} commit-line shas not in their checkout's log; {no_checkout} \
             ground-truth sessions with no checkout here"
        );
        for (set, name) in ["git's commit line", "commit line + `git commit` output"]
            .iter()
            .enumerate()
        {
            let [sessions, pairs] = gt[set];
            eprintln!("\nground truth: {name}: {sessions} sessions, {pairs} commits");
            eprintln!(
                "| paths | min score | slack after | links | right | sibling's | other in \
                 window | other after | precision | own by call time | recall |"
            );
            eprintln!("|---|---|---|---|---|---|---|---|---|---|---|");
            for (i, (paths, t)) in configs.iter().enumerate() {
                let [n, right, sib, inw, after, own] = tally[set][i];
                eprintln!(
                    "| {paths} | {} | {}h | {n} | {right} | {sib} | {inw} | {after} | {:.2} | \
                     {:.2} | {:.2} |",
                    t.min_score,
                    t.slack_after / 3600,
                    right as f64 / n.max(1) as f64,
                    own as f64 / n.max(1) as f64,
                    right as f64 / pairs.max(1) as f64
                );
            }
        }

        // The two checkouts with the most top-level sessions: links per config, then a sample
        // of the shipped rule's links for a hand check.
        let keys: Vec<String> = conn
            .prepare(
                "SELECT COALESCE(project_key, project_path) AS k FROM threads
                 WHERE is_subagent = 0 AND k IS NOT NULL GROUP BY k ORDER BY COUNT(*) DESC
                 LIMIT 20",
            )
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let mut shown = 0;
        for (rank, key) in keys.iter().enumerate() {
            if shown == 2 {
                break;
            }
            let Some(repo) = crate::jobs::checkout_for(key, key) else {
                continue;
            };
            let windows = thread_windows(&conn, key, true).unwrap();
            let Some(earliest) = windows.iter().map(|w| w.start).min() else {
                continue;
            };
            let Ok(log) = run_git_log(&repo, earliest - SLACK_BEFORE) else {
                continue;
            };
            shown += 1;
            let commits = parse_git_log(&log);
            eprintln!(
                "\nproject #{rank}: {} sessions with paths, {} commits",
                windows.len(),
                commits.len()
            );
            for (name, t) in &configs {
                let links = correlate_with(&windows, &commits, t);
                let threads: HashSet<i64> = links.iter().map(|l| l.thread_id).collect();
                let per_commit: HashMap<&str, usize> =
                    links.iter().fold(HashMap::new(), |mut m, l| {
                        *m.entry(l.sha.as_str()).or_default() += 1;
                        m
                    });
                eprintln!(
                    "  {name} min {} slack {}h: {} links, {} sessions, {} commits, {} commits claimed by 3+ sessions",
                    t.min_score,
                    t.slack_after / 3600,
                    links.len(),
                    threads.len(),
                    per_commit.len(),
                    per_commit.values().filter(|n| **n >= 3).count()
                );
            }
            // The best-measured candidate (mentions + edits, min 1, 6 h), for a hand check.
            let candidate = Tuning {
                slack_after: 21_600,
                min_score: 1,
                edits: true,
            };
            let mut links = correlate_with(&windows, &commits, &candidate);
            links.sort_by_key(|l| std::cmp::Reverse(l.committed_at));
            let step = (links.len() / 15).max(1);
            for l in links.iter().step_by(step).take(15) {
                eprintln!(
                    "  sample: thread {} {} overlap {} | {}",
                    l.thread_id,
                    &l.short_sha[..7.min(l.short_sha.len())],
                    l.overlap,
                    l.subject
                );
            }
        }
    }
}
