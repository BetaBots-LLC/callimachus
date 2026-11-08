//! `cal why <file>[:<line>]`: from a line of code back to the sessions that wrote it.
//! `git blame` names the commits that last changed the line (or, for a whole file, the
//! distinct commits that own its lines); `thread_commits` maps each commit to the threads
//! inferred to have produced it; `file_edits` ranks those threads by what they did to this
//! file (an edit beats a read, which beats a mention); and the threads' decisions and
//! gotchas say why. Read-only: it never links commits itself (`cal commits` scans the repo,
//! and the watcher links each index batch), so it is safe from any shell and from the MCP
//! server. The MCP tool `threads_for_file` answers the same question when given a `line`.

use anyhow::{bail, Result};
use rusqlite::Connection;
use schemars::JsonSchema;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::cli_contract::CAL_SCHEMA_VERSION;

/// Commits shown for a whole file. A long-lived file is owned by hundreds of commits; the
/// ones a session produced are the answer, so they are picked first (see [`pick_commits`]).
pub const MAX_COMMITS: usize = 10;
/// Threads shown per commit (a commit can link to many sessions on a busy day).
pub const MAX_THREADS_PER_COMMIT: usize = 5;
/// Decisions and gotchas shown per thread, pinned first.
pub const MAX_FACTS_PER_THREAD: usize = 6;

/// `cal why --json`: the commits behind a file or line and the sessions behind each commit.
#[derive(Debug, Serialize, JsonSchema)]
pub struct WhyOutput {
    /// Major version of this payload's shape. A reader refuses a version newer than it knows.
    pub schema_version: u32,
    /// The file, relative to the repo root, with `/` separators.
    pub file: String,
    /// The 1-based line asked about; null for the whole file.
    pub line: Option<u32>,
    /// The repo root (git's toplevel) the file was blamed in.
    pub repo: String,
    /// The commits that last changed the line, or the file's lines, newest first.
    pub commits: Vec<WhyCommit>,
    /// True when no commit here links to a session yet: run `cal commits` in the repo.
    pub unlinked: bool,
    /// True when the line (or some of the file) has changes no commit owns yet.
    pub uncommitted: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WhyCommit {
    pub sha: String,
    pub short_sha: String,
    pub subject: String,
    /// Committer time, epoch seconds.
    pub committed_at: i64,
    /// Lines of the file this commit last changed (1 for a single line).
    pub lines: usize,
    /// The sessions inferred to have produced the commit, best first: the ones that edited
    /// this file, then read it, then mentioned it.
    pub threads: Vec<WhyThread>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WhyThread {
    pub id: i64,
    pub title: Option<String>,
    pub source: String,
    /// When the thread last changed, epoch seconds.
    pub last_active: Option<i64>,
    /// Edit and write tool calls on this file inside the thread.
    pub edits: i64,
    /// The strongest thing the thread did to this file: `write`, `edit`, `read` or
    /// `mention`; null when only the commit's other files linked it.
    pub touch: Option<String>,
    /// Files the thread and the commit share (the link's confidence, not normalized).
    pub overlap: i64,
    /// The thread's decisions and gotchas, pinned first.
    pub facts: Vec<WhyFact>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WhyFact {
    pub id: i64,
    pub thread_id: i64,
    /// `decision` or `gotcha`.
    pub kind: String,
    pub text: String,
    pub rationale: Option<String>,
    /// Who wrote it: llm, heuristic, user, agent, or an importer such as claude-memory.
    pub extractor: String,
    pub origin_ref: Option<String>,
}

/// A file inside a git checkout, as `git blame` names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The checkout's toplevel, as git prints it (forward slashes on Windows too).
    pub repo: String,
    /// The path relative to `repo`, `/`-separated.
    pub rel: String,
}

/// One commit from `git blame --porcelain`, with how many of the blamed lines it owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameCommit {
    pub sha: String,
    pub committed_at: i64,
    pub subject: String,
    pub lines: usize,
}

/// `<file>[:<line>]`, or `<file> <line>`, from `cal why`'s positionals. The line is split
/// off the last `:` only when what follows is a number, so `C:\repo\a.rs` stays a path.
pub fn parse_target(positional: &[String]) -> Result<(String, Option<u32>)> {
    let usage = "usage: cal why <file>[:<line>]  (e.g. `cal why src/auth.rs:42`)";
    let (first, rest) = positional
        .split_first()
        .ok_or_else(|| anyhow::anyhow!(usage))?;
    let line_of = |s: &str| -> Result<u32> {
        match s.trim().parse::<u32>() {
            Ok(n) if n >= 1 => Ok(n),
            _ => bail!("the line must be a number from 1, got '{s}'"),
        }
    };
    let (file, mut line) = match first.rsplit_once(':') {
        Some((f, l)) if !f.is_empty() && !l.is_empty() && l.bytes().all(|b| b.is_ascii_digit()) => {
            (f.to_string(), Some(line_of(l)?))
        }
        _ => (first.clone(), None),
    };
    match rest {
        [] => {}
        [l] if line.is_none() => line = Some(line_of(l)?),
        _ => bail!(usage),
    }
    Ok((file, line))
}

/// Find `file` (absolute, or relative to `base`; failing that, relative to the root of the
/// checkout `base` is in, which is how an agent names files) and its checkout.
pub fn resolve(file: &str, base: &Path, deadline: Instant) -> Result<Target> {
    let given = Path::new(file.trim());
    let mut path = if given.is_absolute() {
        given.to_path_buf()
    } else {
        base.join(given)
    };
    if !path.is_file() && !given.is_absolute() {
        if let Ok(top) = run_git(base, &["rev-parse", "--show-toplevel"], deadline) {
            let alt = PathBuf::from(top.trim()).join(given);
            if alt.is_file() {
                path = alt;
            }
        }
    }
    if !path.is_file() {
        bail!("no file at {}", path.display());
    }
    let dir = path.parent().unwrap_or(base);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let out = run_git(
        dir,
        &["rev-parse", "--show-toplevel", "--show-prefix"],
        deadline,
    )
    .map_err(|e| anyhow::anyhow!("{} is not in a git repository ({e})", path.display()))?;
    let mut lines = out.lines();
    let repo = lines.next().unwrap_or("").trim().to_string();
    let prefix = lines.next().unwrap_or("").trim();
    if repo.is_empty() {
        bail!("{} is not in a git repository", path.display());
    }
    Ok(Target {
        repo,
        rel: format!("{prefix}{name}"),
    })
}

/// `git blame --porcelain` of one line, or of the whole file. `-w` and `-M` look through
/// reindenting and lines moved within the file to the commit that wrote them: those
/// formatting commits are rarely linked to a session, so stopping at them answered "no
/// session is linked" for a line a session did write, and `cal commits` could not fix it.
/// (`-C`, across files, is left out: it costs a pass over each commit's other files, and
/// the MCP server answers within a short deadline.)
///
/// A file HEAD does not have (untracked, or added and not committed yet) makes git fail;
/// it is answered as one uncommitted line instead, the same state a modified line gets.
pub fn blame(target: &Target, line: Option<u32>, deadline: Instant) -> Result<String> {
    let range = line.map(|n| format!("{n},{n}"));
    let mut args = vec!["blame", "--porcelain", "-w", "-M"];
    if let Some(r) = &range {
        args.extend(["-L", r]);
    }
    args.extend(["--", &target.rel]);
    let repo = Path::new(&target.repo);
    match run_git(repo, &args, deadline) {
        Ok(out) => Ok(out),
        // Asked by exit status, not by git's message, which is translated.
        Err(e) => match run_git(
            repo,
            &["cat-file", "-e", &format!("HEAD:{}", target.rel)],
            deadline,
        ) {
            Err(_) if Instant::now() < deadline => Ok(not_committed_porcelain(line)),
            _ => Err(e),
        },
    }
}

/// Porcelain for a line no commit owns, as `git blame` prints a working-tree change.
fn not_committed_porcelain(line: Option<u32>) -> String {
    let n = line.unwrap_or(1);
    format!(
        "{zero} {n} {n} 1\nauthor Not Committed Yet\nsummary Not Committed Yet\n\t\n",
        zero = "0".repeat(40)
    )
}

/// The distinct commits in `git blame --porcelain` output, in order of first appearance,
/// with the lines each owns. Each commit's header lines (`committer-time`, `summary`) come
/// once, after its first `<sha> <orig> <final> [<count>]` line; every blamed line is a
/// tab-prefixed content line under the latest sha line.
pub fn parse_blame(porcelain: &str) -> Vec<BlameCommit> {
    let mut order: Vec<BlameCommit> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    let mut current: Option<usize> = None;
    for line in porcelain.lines() {
        if line.starts_with('\t') {
            if let Some(i) = current {
                order[i].lines += 1;
            }
            continue;
        }
        let mut parts = line.split(' ');
        let head = parts.next().unwrap_or("");
        if is_sha(head) && parts.next().is_some_and(|n| n.parse::<u64>().is_ok()) {
            let i = *at.entry(head.to_string()).or_insert_with(|| {
                order.push(BlameCommit {
                    sha: head.to_string(),
                    committed_at: 0,
                    subject: String::new(),
                    lines: 0,
                });
                order.len() - 1
            });
            current = Some(i);
            continue;
        }
        let Some(i) = current else { continue };
        if let Some(t) = line.strip_prefix("committer-time ") {
            order[i].committed_at = t.trim().parse().unwrap_or(0);
        } else if let Some(s) = line.strip_prefix("summary ") {
            order[i].subject = s.to_string();
        }
    }
    order
}

/// A full object name: 40 hex chars (SHA-1) or 64 (SHA-256 repositories).
fn is_sha(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// What `git blame` prints for lines in the working tree that no commit owns yet.
fn is_uncommitted(sha: &str) -> bool {
    sha.bytes().all(|b| b == b'0')
}

/// [`blame`] then [`explain`]: the answer for a [`resolve`]d file.
pub fn why(
    conn: &Connection,
    target: &Target,
    line: Option<u32>,
    deadline: Instant,
) -> Result<WhyOutput> {
    let porcelain = blame(target, line, deadline)?;
    explain(conn, target, line, &parse_blame(&porcelain))
}

/// The answer for blamed commits: each commit's linked threads, ranked by what they did to
/// the file, with their decisions and gotchas.
pub fn explain(
    conn: &Connection,
    target: &Target,
    line: Option<u32>,
    blamed: &[BlameCommit],
) -> Result<WhyOutput> {
    let uncommitted = blamed.iter().any(|c| is_uncommitted(&c.sha));
    let has_edits = crate::db::has_table(conn, "file_edits");
    let any_linked = blamed.iter().any(|c| !is_uncommitted(&c.sha));
    let paths = if has_edits && any_linked {
        path_forms(target, &project_checkouts(conn, target))
    } else {
        path_forms(target, &[])
    };
    let origin_ref = crate::knowledge::origin_ref_sql(conn);
    let fact_sql = format!(
        "SELECT f.id, f.kind, f.text, f.rationale, f.extractor, {origin_ref} FROM facts f
         WHERE f.thread_id = ?1 AND f.hidden = 0 AND f.kind IN ('decision', 'gotcha')
         ORDER BY f.pinned DESC, f.seq, f.id LIMIT ?2"
    );

    let mut linked: Vec<(BlameCommit, Vec<WhyThread>)> = Vec::new();
    for c in blamed.iter().filter(|c| !is_uncommitted(&c.sha)) {
        let mut threads = commit_threads(conn, &c.sha)?;
        for t in &mut threads {
            rank_touch(conn, t, &paths, has_edits)?;
        }
        threads.sort_by(|a, b| {
            touch_rank(b.touch.as_deref())
                .cmp(&touch_rank(a.touch.as_deref()))
                .then(b.edits.cmp(&a.edits))
                .then(b.overlap.cmp(&a.overlap))
                .then(b.last_active.cmp(&a.last_active))
        });
        threads.truncate(MAX_THREADS_PER_COMMIT);
        linked.push((c.clone(), threads));
    }
    let picked = pick_commits(linked, MAX_COMMITS);

    let mut facts = conn.prepare_cached(&fact_sql)?;
    let mut commits = Vec::with_capacity(picked.len());
    for (c, mut threads) in picked {
        for t in &mut threads {
            t.facts = facts
                .query_map(rusqlite::params![t.id, MAX_FACTS_PER_THREAD as i64], |r| {
                    Ok(WhyFact {
                        id: r.get(0)?,
                        thread_id: t.id,
                        kind: r.get(1)?,
                        text: r.get(2)?,
                        rationale: r.get(3)?,
                        extractor: r.get(4)?,
                        origin_ref: r.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?;
        }
        commits.push(WhyCommit {
            short_sha: c.sha.chars().take(10).collect(),
            sha: c.sha,
            subject: c.subject,
            committed_at: c.committed_at,
            lines: c.lines,
            threads,
        });
    }
    let unlinked = commits.iter().all(|c| c.threads.is_empty());
    Ok(WhyOutput {
        schema_version: CAL_SCHEMA_VERSION,
        file: target.rel.clone(),
        line,
        repo: target.repo.clone(),
        commits,
        unlinked,
        uncommitted,
    })
}

/// Up to `cap` commits, newest first, taking the ones a session produced before the rest:
/// "why" is answered by a linked commit, and a file's newest commits are often unlinked
/// formatting or merges that would crowd the linked ones out of a newest-first cut.
fn pick_commits(
    mut all: Vec<(BlameCommit, Vec<WhyThread>)>,
    cap: usize,
) -> Vec<(BlameCommit, Vec<WhyThread>)> {
    all.sort_by_key(|c| std::cmp::Reverse(c.0.committed_at));
    let (mut picked, rest): (Vec<_>, Vec<_>) = all.into_iter().partition(|(_, t)| !t.is_empty());
    picked.truncate(cap);
    let room = cap - picked.len();
    picked.extend(rest.into_iter().take(room));
    picked.sort_by_key(|c| std::cmp::Reverse(c.0.committed_at));
    picked
}

/// The threads linked to one commit, through `idx_thread_commits_sha` (0023).
pub(crate) const COMMIT_THREADS_SQL: &str =
    "SELECT tc.thread_id, tc.overlap, t.title, s.kind, t.updated_at
     FROM thread_commits tc
     JOIN threads t ON t.id = tc.thread_id
     JOIN sources s ON s.id = t.source_id
     WHERE tc.sha = ?1";

fn commit_threads(conn: &Connection, sha: &str) -> Result<Vec<WhyThread>> {
    let mut stmt = conn.prepare_cached(COMMIT_THREADS_SQL)?;
    let rows = stmt.query_map([sha], |r| {
        Ok(WhyThread {
            id: r.get(0)?,
            overlap: r.get(1)?,
            title: r.get(2)?,
            source: r.get(3)?,
            last_active: r.get(4)?,
            edits: 0,
            touch: None,
            facts: Vec::new(),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// What one thread did to the file, per op, for every spelling of its path (see
/// [`path_forms`], passed as a JSON array so the statement stays one cached plan).
pub(crate) const TOUCH_SQL: &str = "SELECT op, COUNT(*) FROM file_edits
     WHERE thread_id = ?1 AND path IN (SELECT value FROM json_each(?2)) GROUP BY op";

/// How long finding the project's other checkouts may take (`git worktree list` per clone).
const CHECKOUTS_GIT_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// The checkouts of the target's project, toplevel paths with `/` separators: this one, the
/// main repo and every linked worktree of each clone the project has on this machine (the
/// clones share its portable id, `knowledge::local_keys_for_portable`), and the roots the
/// project's sessions anchored their edits to, which still name worktrees removed after
/// their merge.
fn project_checkouts(conn: &Connection, target: &Target) -> Vec<String> {
    let deadline = Instant::now() + CHECKOUTS_GIT_BUDGET;
    let mut keys = vec![
        crate::indexer::canonical_project(&target.repo).unwrap_or_else(|| target.repo.clone())
    ];
    // The key the index gave sessions that ran in this checkout: for a linked worktree, the
    // main repo it belonged to when they were indexed (off idx_threads_project).
    if let Ok(mut stmt) = conn.prepare_cached(
        "SELECT DISTINCT project_key FROM threads WHERE project_path = ?1 AND project_key IS NOT NULL",
    ) {
        let stored: Vec<String> = stmt
            .query_map([&target.repo], |r| r.get(0))
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default();
        for k in stored {
            if !keys.contains(&k) {
                keys.push(k);
            }
        }
    }
    if let Some(id) = crate::knowledge::portable_project_id(conn, &keys[0]) {
        for k in crate::knowledge::local_keys_for_portable(conn, &id) {
            if !keys.contains(&k) {
                keys.push(k);
            }
        }
    }
    let mut roots = vec![target.repo.clone()];
    for key in &keys {
        roots.push(key.clone());
        if let Ok(out) = run_git(
            Path::new(key),
            &["worktree", "list", "--porcelain"],
            deadline,
        ) {
            roots.extend(
                out.lines()
                    .filter_map(|l| l.strip_prefix("worktree "))
                    .map(str::to_string),
            );
        }
        if let Ok(mut stmt) =
            conn.prepare_cached("SELECT DISTINCT project_path FROM threads WHERE project_key = ?1")
        {
            let cwds: Vec<String> = stmt
                .query_map([key], |r| r.get::<_, Option<String>>(0))
                .map(|rows| rows.filter_map(|r| r.ok().flatten()).collect())
                .unwrap_or_default();
            roots.extend(
                cwds.iter()
                    .map(|c| crate::indexer::file_edits::checkout_root(c)),
            );
        }
    }
    let mut out: Vec<String> = Vec::new();
    for r in roots {
        let r = r.replace('\\', "/").trim_end_matches('/').to_string();
        if !r.is_empty() && !out.contains(&r) {
            out.push(r);
        }
    }
    out
}

/// Every spelling `file_edits` can hold for the target. A session stores a path inside its
/// own checkout relative to that checkout's root, so `rel` matches whichever checkout of the
/// project the session ran in; a path outside it stays absolute (`<checkout>/<rel>` for each
/// checkout); and a session in the main repo editing a worktree nested inside it stores the
/// path relative to the main repo (`.claude/worktrees/x/<rel>`).
fn path_forms(target: &Target, checkouts: &[String]) -> Vec<String> {
    let repo = target
        .repo
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_string();
    let mut roots: Vec<&str> = vec![repo.as_str()];
    roots.extend(checkouts.iter().map(String::as_str).filter(|c| *c != repo));
    let mut out = vec![target.rel.clone()];
    let mut add = |p: String| {
        if !out.contains(&p) {
            out.push(p);
        }
    };
    for r in &roots {
        add(format!("{r}/{}", target.rel));
    }
    for outer in &roots {
        for inner in &roots {
            if let Some(sub) = inner
                .strip_prefix(outer)
                .and_then(|s| s.strip_prefix('/'))
                .filter(|s| !s.is_empty())
            {
                add(format!("{sub}/{}", target.rel));
            }
        }
    }
    out
}

fn rank_touch(
    conn: &Connection,
    t: &mut WhyThread,
    paths: &[String],
    has_edits: bool,
) -> Result<()> {
    if has_edits {
        let forms = serde_json::to_string(paths)?;
        let mut stmt = conn.prepare_cached(TOUCH_SQL)?;
        let ops = stmt.query_map(rusqlite::params![t.id, forms], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        for op in ops {
            let (op, n) = op?;
            if matches!(op.as_str(), "edit" | "write") {
                t.edits += n;
            }
            if touch_rank(Some(&op)) > touch_rank(t.touch.as_deref()) {
                t.touch = Some(op);
            }
        }
    }
    if t.touch.is_none() && mentions_file(conn, t.id, &paths[0])? {
        // Threads indexed before 0034 have no file_edits rows until the one-time backfill
        // reaches them; the prose mentions every thread has still say it named the file.
        t.touch = Some("mention".into());
    }
    Ok(())
}

/// Whether the thread's `file_mentions` name `rel`, matched as gitlink matches a commit's
/// files: case-insensitive, exact or one path a suffix of the other at a directory boundary
/// (a bare `auth.rs` never matches; a longer `apps/x/src/auth.rs` does, as it does for
/// gitlink, since a mention does not say which checkout it was relative to). Mention extraction drops a
/// leading dot (`.github/workflows/ci.yml` is stored as `github/workflows/ci.yml`), so `rel`
/// is also tried with each segment's leading dots stripped.
fn mentions_file(conn: &Connection, thread_id: i64, rel: &str) -> Result<bool> {
    let mut stmt = conn.prepare_cached("SELECT path FROM file_mentions WHERE thread_id = ?1")?;
    let mentions = stmt
        .query_map([thread_id], |r| r.get::<_, String>(0))?
        .map(|p| p.map(|p| p.to_ascii_lowercase()))
        .collect::<rusqlite::Result<std::collections::HashSet<String>>>()?;
    let dotless: String = rel
        .split('/')
        .map(|seg| seg.trim_start_matches('.'))
        .collect::<Vec<_>>()
        .join("/");
    let mut files = vec![rel.to_string()];
    if dotless != rel {
        files.push(dotless);
    }
    Ok(crate::gitlink::path_overlap(&mentions, &files) > 0)
}

/// Write and edit are one tier (the spec ranks a change to the file above a read above a
/// mention): a thread that created the file once must not outrank one that edited it 20
/// times, so within the tier the `edits` count orders them.
fn touch_rank(op: Option<&str>) -> u8 {
    match op {
        Some("write" | "edit") => 3,
        Some("read") => 2,
        Some("mention") => 1,
        _ => 0,
    }
}

/// Run git in `dir` and return its stdout, or fail with its first stderr line. Killed at
/// `deadline`: blame of a long file with deep history can take seconds, and the MCP server
/// answers an agent waiting on it.
fn run_git(dir: &Path, args: &[&str], deadline: Instant) -> Result<String> {
    use std::io::Read;
    use std::process::Stdio;
    let mut child = crate::gitlink::git_command()
        .arg("-C")
        .arg(crate::gitlink::plain_path(&dir.to_string_lossy()))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("running git: {e} (is git installed?)"))?;
    // Drained on helper threads: output bigger than the pipe buffer would otherwise block
    // git on write and it would never exit.
    let drain = |mut pipe: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            buf
        })
    };
    let out = drain(Box::new(child.stdout.take().expect("piped stdout")));
    let err = drain(Box::new(child.stderr.take().expect("piped stderr")));
    loop {
        let polled = match child.try_wait() {
            Ok(polled) => polled,
            Err(e) => {
                // Reap git before giving up, or it and the reader threads keep running.
                let _ = child.kill();
                let _ = child.wait();
                bail!("waiting for git: {e}");
            }
        };
        if let Some(status) = polled {
            let stdout = out.join().unwrap_or_default();
            let stderr = err.join().unwrap_or_default();
            if !status.success() {
                let msg = String::from_utf8_lossy(&stderr);
                let first = msg.lines().next().unwrap_or("").trim();
                let first = first.strip_prefix("fatal: ").unwrap_or(first);
                bail!("git {} failed: {first}", args[0]);
            }
            return Ok(String::from_utf8_lossy(&stdout).into_owned());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("git {} did not finish in time", args[0]);
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::{source_id, upsert_thread, ParsedMessage, ParsedThread};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    static N: AtomicU32 = AtomicU32::new(0);

    fn temp_db() -> Connection {
        let p = std::env::temp_dir().join(format!(
            "calli_why_{}_{}.db",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        crate::db::open(&p).unwrap()
    }

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const ZERO: &str = "0000000000000000000000000000000000000000";

    /// Real `git blame --porcelain` shape: headers once per commit, then repeats.
    fn porcelain() -> String {
        format!(
            "{A} 1 1 2\nauthor x\nauthor-time 1700000000\ncommitter x\ncommitter-time 1700000100\n\
             summary feat: rotate refresh tokens\nfilename src/auth.rs\n\tfn a() {{}}\n\
             {A} 2 2\n\tfn b() {{}}\n\
             {B} 3 3 1\nauthor y\ncommitter-time 1700000900\nsummary chore: fmt\nboundary\n\
             previous {A} src/auth.rs\nfilename src/auth.rs\n\tfn c() {{}}\n\
             {ZERO} 4 4 1\nauthor Not Committed Yet\ncommitter-time 1700001000\n\
             summary Version of src/auth.rs from src/auth.rs\nfilename src/auth.rs\n\tlocal\n"
        )
    }

    #[test]
    fn blame_porcelain_parses_commits_once_with_line_counts() {
        let got = parse_blame(&porcelain());
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].sha, A);
        assert_eq!(got[0].committed_at, 1_700_000_100);
        assert_eq!(got[0].subject, "feat: rotate refresh tokens");
        assert_eq!(got[0].lines, 2);
        assert_eq!(got[1].subject, "chore: fmt");
        assert_eq!(got[1].lines, 1);
        assert!(is_uncommitted(&got[2].sha));
        // A content line that looks like a header is still content (it is tab-prefixed).
        let tricky = format!("{A} 1 1 1\nsummary x\n\t{B} 1 1 1\n");
        let got = parse_blame(&tricky);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].lines, 1);
    }

    #[test]
    fn targets_split_the_line_off_only_when_numeric() {
        let p = |a: &[&str]| parse_target(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(p(&["src/a.rs:42"]).unwrap(), ("src/a.rs".into(), Some(42)));
        assert_eq!(p(&["src/a.rs"]).unwrap(), ("src/a.rs".into(), None));
        assert_eq!(p(&["src/a.rs", "7"]).unwrap(), ("src/a.rs".into(), Some(7)));
        assert_eq!(
            p(&[r"C:\repo\a.rs:3"]).unwrap(),
            (r"C:\repo\a.rs".into(), Some(3))
        );
        assert_eq!(
            p(&[r"C:\repo\a.rs"]).unwrap(),
            (r"C:\repo\a.rs".into(), None)
        );
        assert!(p(&["src/a.rs:0"]).is_err());
        assert!(p(&["src/a.rs:4", "5"]).is_err());
        assert!(p(&[]).is_err());
    }

    fn thread(conn: &mut Connection, ext: &str, title: &str, ts: i64) -> i64 {
        let sid = source_id(conn, "claude_code").unwrap();
        upsert_thread(
            conn,
            sid,
            &ParsedThread {
                external_id: ext.into(),
                title: Some(title.into()),
                project_path: Some("/repo".into()),
                messages: vec![ParsedMessage {
                    role: "user".into(),
                    text: "work".into(),
                    tool_name: None,
                    ts: Some(ts),
                }],
                ..Default::default()
            },
        )
        .unwrap();
        conn.query_row(
            "SELECT id FROM threads WHERE external_id = ?1",
            [ext],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn link(conn: &Connection, tid: i64, sha: &str, overlap: i64) {
        conn.execute(
            "INSERT INTO thread_commits
                (thread_id, sha, short_sha, subject, committed_at, overlap, created_at)
             VALUES (?1, ?2, ?3, 's', 1, ?4, 1)",
            rusqlite::params![tid, sha, &sha[..10], overlap],
        )
        .unwrap();
    }

    fn edit(conn: &Connection, tid: i64, path: &str, op: &str) {
        conn.execute(
            "INSERT INTO file_edits (thread_id, message_id, path, op, ts) VALUES (?1, NULL, ?2, ?3, 1)",
            rusqlite::params![tid, path, op],
        )
        .unwrap();
    }

    fn fact(conn: &Connection, tid: i64, kind: &str, text: &str, pinned: bool, hidden: bool) {
        conn.execute(
            "INSERT INTO facts (thread_id, kind, text, extractor, created_at, pinned, hidden)
             VALUES (?1, ?2, ?3, 'llm', 1, ?4, ?5)",
            rusqlite::params![tid, kind, text, pinned, hidden],
        )
        .unwrap();
    }

    fn target() -> Target {
        Target {
            repo: "/repo".into(),
            rel: "src/auth.rs".into(),
        }
    }

    #[test]
    fn an_edit_of_the_file_outranks_a_mention_and_a_bare_link() {
        let mut conn = temp_db();
        let mentioner = thread(&mut conn, "m", "talked about it", 300);
        let editor = thread(&mut conn, "e", "rewrote auth", 100);
        let bare = thread(&mut conn, "b", "touched other files", 400);
        let reader = thread(&mut conn, "r", "read it", 200);
        // The mentioner and the bare link share more files with the commit: the file's own
        // edits still win.
        link(&conn, mentioner, A, 5);
        link(&conn, editor, A, 1);
        link(&conn, bare, A, 9);
        link(&conn, reader, A, 1);
        edit(&conn, mentioner, "src/auth.rs", "mention");
        edit(&conn, editor, "src/auth.rs", "read");
        edit(&conn, editor, "src/auth.rs", "edit");
        // An absolute path (a session outside the checkout) is the same file.
        edit(&conn, editor, "/repo/src/auth.rs", "edit");
        edit(&conn, editor, "src/other.rs", "write");
        edit(&conn, reader, "src/auth.rs", "read");
        fact(
            &conn,
            editor,
            "decision",
            "rotate on every use",
            false,
            false,
        );
        fact(
            &conn,
            editor,
            "gotcha",
            "clock skew breaks expiry",
            true,
            false,
        );
        fact(
            &conn,
            editor,
            "decision",
            "deleted by the user",
            false,
            true,
        );
        fact(&conn, editor, "todo", "a todo is not why", false, false);

        let blamed = parse_blame(&porcelain());
        let out = explain(&conn, &target(), Some(1), &blamed).unwrap();
        assert!(!out.unlinked);
        assert!(out.uncommitted, "the zero sha is a working-tree change");
        assert_eq!(out.file, "src/auth.rs");
        assert_eq!(out.commits.len(), 2, "the uncommitted line is no commit");
        let a = out.commits.iter().find(|c| c.sha == A).unwrap();
        let ids: Vec<i64> = a.threads.iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![editor, reader, mentioner, bare]);
        let top = &a.threads[0];
        assert_eq!(top.touch.as_deref(), Some("edit"));
        assert_eq!(top.edits, 2, "both spellings of the path, not src/other.rs");
        assert_eq!(top.title.as_deref(), Some("rewrote auth"));
        let facts: Vec<&str> = top.facts.iter().map(|f| f.text.as_str()).collect();
        assert_eq!(
            facts,
            vec!["clock skew breaks expiry", "rotate on every use"]
        );
        assert!(top.facts.iter().all(|f| f.thread_id == editor));
        assert_eq!(a.threads[3].touch, None);
        // Newest first: B (chore: fmt) after A by committer time.
        assert_eq!(out.commits[0].sha, B);
        assert!(out.commits[0].threads.is_empty());
    }

    #[test]
    fn a_prose_mention_ranks_threads_indexed_before_file_edits() {
        let mut conn = temp_db();
        let old = thread(&mut conn, "old", "before 0034", 100);
        let bare = thread(&mut conn, "bare", "named another auth.rs", 100);
        link(&conn, old, A, 1);
        link(&conn, bare, A, 1);
        let mention = |tid: i64, path: &str| {
            conn.execute(
                "INSERT INTO file_mentions (thread_id, path) VALUES (?1, ?2)",
                rusqlite::params![tid, path],
            )
            .unwrap();
        };
        mention(old, "src/Auth.rs");
        // A bare name and another directory's file of the same name are not this file.
        mention(bare, "Auth.rs");
        mention(bare, "lib/auth.rs");
        let out = explain(&conn, &target(), None, &parse_blame(&porcelain())).unwrap();
        let a = out.commits.iter().find(|c| c.sha == A).unwrap();
        let touch = |id| a.threads.iter().find(|t| t.id == id).unwrap().touch.clone();
        assert_eq!(touch(old).as_deref(), Some("mention"));
        assert_eq!(touch(bare), None);

        // Mention extraction stores `.github/...` without its dot.
        let ci = Target {
            repo: "/repo".into(),
            rel: ".github/workflows/ci.yml".into(),
        };
        mention(old, "github/workflows/ci.yml");
        let out = explain(&conn, &ci, None, &parse_blame(&porcelain())).unwrap();
        let a = out.commits.iter().find(|c| c.sha == A).unwrap();
        assert_eq!(a.threads[0].id, old);
        assert_eq!(a.threads[0].touch.as_deref(), Some("mention"));
    }

    /// One created the file with a single write, the other edited it many times: both changed
    /// it, so the edit count orders them, not write over edit.
    #[test]
    fn write_and_edit_are_one_tier() {
        let mut conn = temp_db();
        let writer = thread(&mut conn, "w", "created it", 300);
        let editor = thread(&mut conn, "e", "reworked it", 100);
        link(&conn, writer, A, 1);
        link(&conn, editor, A, 1);
        edit(&conn, writer, "src/auth.rs", "write");
        for _ in 0..20 {
            edit(&conn, editor, "src/auth.rs", "edit");
        }
        let out = explain(&conn, &target(), Some(1), &parse_blame(&porcelain())).unwrap();
        let a = out.commits.iter().find(|c| c.sha == A).unwrap();
        let ids: Vec<i64> = a.threads.iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![editor, writer]);
        assert_eq!(a.threads[0].edits, 20);
    }

    /// Edits stored against another checkout of the project count: a session in the main
    /// repo editing a worktree nested in it (stored relative to the main repo), and a session
    /// elsewhere editing a worktree by absolute path, blamed from the main repo or the
    /// worktree. The worktree is known from the sessions that ran in it, even once removed.
    #[test]
    fn edits_in_another_checkout_of_the_project_count() {
        let mut conn = temp_db();
        let nested = thread(&mut conn, "n", "edited the worktree from the repo", 300);
        let outside = thread(&mut conn, "o", "edited it by absolute path", 200);
        let mentioner = thread(&mut conn, "m", "only talked", 400);
        let in_tree = thread(&mut conn, "t", "ran in the worktree", 100);
        conn.execute(
            "UPDATE threads SET project_path = '/repo/.claude/worktrees/x', project_key = '/repo'
             WHERE id = ?1",
            [in_tree],
        )
        .unwrap();
        for t in [nested, outside, mentioner] {
            link(&conn, t, A, 1);
        }
        edit(&conn, nested, ".claude/worktrees/x/src/auth.rs", "edit");
        edit(
            &conn,
            outside,
            "/repo/.claude/worktrees/x/src/auth.rs",
            "edit",
        );
        edit(&conn, mentioner, "src/auth.rs", "mention");
        for repo in ["/repo", "/repo/.claude/worktrees/x"] {
            let t = Target {
                repo: repo.into(),
                rel: "src/auth.rs".into(),
            };
            let out = explain(&conn, &t, Some(1), &parse_blame(&porcelain())).unwrap();
            let a = out.commits.iter().find(|c| c.sha == A).unwrap();
            let edited: Vec<i64> = a
                .threads
                .iter()
                .filter(|t| t.touch.as_deref() == Some("edit"))
                .map(|t| t.id)
                .collect();
            assert_eq!(edited.len(), 2, "from {repo}: {:?}", a.threads);
            assert!(edited.contains(&nested) && edited.contains(&outside));
            assert_eq!(a.threads[2].id, mentioner, "from {repo}");
        }
    }

    #[test]
    fn path_forms_cover_each_checkout() {
        let t = Target {
            repo: "/repo".into(),
            rel: "src/a.rs".into(),
        };
        let forms = path_forms(&t, &["/repo/.claude/worktrees/x".into(), "/clone".into()]);
        assert_eq!(
            forms,
            vec![
                "src/a.rs",
                "/repo/src/a.rs",
                "/repo/.claude/worktrees/x/src/a.rs",
                "/clone/src/a.rs",
                ".claude/worktrees/x/src/a.rs",
            ]
        );
    }

    #[test]
    fn nothing_linked_says_so() {
        let conn = temp_db();
        let out = explain(&conn, &target(), None, &parse_blame(&porcelain())).unwrap();
        assert!(out.unlinked);
        assert_eq!(out.commits.len(), 2);
        let none = explain(&conn, &target(), Some(4), &[]).unwrap();
        assert!(none.unlinked && none.commits.is_empty() && !none.uncommitted);
    }

    #[test]
    fn linked_commits_are_kept_when_newer_ones_fill_the_cap() {
        let mut conn = temp_db();
        let t = thread(&mut conn, "t", "old work", 1);
        let old = format!("{:040x}", 1);
        link(&conn, t, &old, 1);
        let mut blamed = vec![BlameCommit {
            sha: old.clone(),
            committed_at: 1,
            subject: "the linked one".into(),
            lines: 1,
        }];
        for i in 0..MAX_COMMITS + 5 {
            blamed.push(BlameCommit {
                sha: format!("{:040x}", 1000 + i),
                committed_at: 1000 + i as i64,
                subject: format!("newer {i}"),
                lines: 1,
            });
        }
        let out = explain(&conn, &target(), None, &blamed).unwrap();
        assert_eq!(out.commits.len(), MAX_COMMITS);
        assert_eq!(
            out.commits.last().unwrap().sha,
            old,
            "kept, and still newest first"
        );
        assert!(out
            .commits
            .windows(2)
            .all(|w| w[0].committed_at >= w[1].committed_at));
    }

    fn plan(conn: &Connection, sql: &str) -> String {
        let n = sql.matches('?').count();
        let params: Vec<String> = (0..n).map(|_| "1".to_string()).collect();
        conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map(rusqlite::params_from_iter(params.iter()), |r| {
                r.get::<_, String>(3)
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join(" | ")
    }

    /// Both lookups are point probes: a commit's threads by sha, a thread's ops on the file.
    #[test]
    fn lookups_use_their_indexes() {
        let conn = temp_db();
        let p = plan(&conn, COMMIT_THREADS_SQL);
        assert!(
            p.contains("USING INDEX idx_thread_commits_sha (sha=?)"),
            "{p}"
        );
        let p = plan(&conn, TOUCH_SQL);
        assert!(p.contains("idx_file_edits_"), "{p}");
        assert!(!p.contains("SCAN file_edits"), "{p}");
    }

    #[test]
    fn end_to_end_with_real_git() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let dir = std::env::temp_dir().join(format!(
            "calli_why_repo_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .env("GIT_AUTHOR_DATE", "1700000000 +0000")
                .env("GIT_COMMITTER_DATE", "1700000000 +0000")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(dir.join("src/auth.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "wire auth"]);
        let sha = git(&["rev-parse", "HEAD"]);

        let mut conn = temp_db();
        let tid = thread(&mut conn, "g", "auth work", 1_700_000_000);
        link(&conn, tid, &sha, 1);
        edit(&conn, tid, "src/auth.rs", "write");

        let deadline = Instant::now() + Duration::from_secs(20);
        // From a subdirectory, by a path relative to it, as a shell user would.
        let t = resolve("auth.rs", &dir.join("src"), deadline).unwrap();
        assert_eq!(t.rel, "src/auth.rs");
        // By a repo-relative path from the root, as an agent would.
        assert_eq!(
            resolve("src/auth.rs", &dir, deadline).unwrap().rel,
            "src/auth.rs"
        );
        let out = why(&conn, &t, Some(2), deadline).unwrap();
        assert_eq!(out.commits.len(), 1);
        assert_eq!(out.commits[0].sha, sha);
        assert_eq!(out.commits[0].subject, "wire auth");
        assert_eq!(out.commits[0].committed_at, 1_700_000_000);
        assert_eq!(out.commits[0].threads[0].id, tid);
        assert_eq!(out.commits[0].threads[0].touch.as_deref(), Some("write"));
        let whole = why(&conn, &t, None, deadline).unwrap();
        assert_eq!(whole.commits[0].lines, 2);
        // Past the end: git's own message, not a panic.
        let err = why(&conn, &t, Some(99), deadline).unwrap_err().to_string();
        assert!(err.contains("git blame failed"), "{err}");
        // A file HEAD does not have, untracked or only staged: uncommitted, not an error.
        std::fs::write(dir.join("src/new.rs"), "fn n() {}\n").unwrap();
        let fresh = resolve("src/new.rs", &dir, deadline).unwrap();
        let out = why(&conn, &fresh, Some(1), deadline).unwrap();
        assert!(out.uncommitted && out.commits.is_empty(), "{out:?}");
        git(&["add", "src/new.rs"]);
        let out = why(&conn, &fresh, None, deadline).unwrap();
        assert!(out.uncommitted && out.commits.is_empty(), "{out:?}");
        // A reindent is not the commit that wrote the line (`-w`).
        std::fs::write(dir.join("src/auth.rs"), "fn a() {}\n    fn b() {}\n").unwrap();
        git(&["commit", "-q", "-am", "style: indent"]);
        let out = why(&conn, &t, Some(2), deadline).unwrap();
        assert_eq!(out.commits[0].sha, sha, "{out:?}");
        assert!(resolve("nope.rs", &dir, deadline).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
