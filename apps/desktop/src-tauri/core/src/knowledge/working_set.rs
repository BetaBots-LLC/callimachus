//! SessionStart memory ranked by the files the project's latest sessions edited.
//!
//! The hook shows 8 decisions and 8 gotchas ([`crate::export::MemoryBudget::HOOK`]), picked
//! pinned first, then by when they were decided ([`super::get_project_memory_with`]). In a
//! repo with hundreds of facts most never reach a session, the one naming the file the
//! session is about to change included: the read fetches the newest 40 per kind, so an older
//! fact never even becomes a candidate. [`get_project_memory_for_working_set`] also reads the
//! facts that name a file of the project's working set and puts up to
//! [`WORKING_SET_MAX_FACTS`] of each kind right after the pinned ones, never more than half
//! the slots the pinned ones leave ([`promoted_max`]); the rest keep their order.
//!
//! Measured on a real index (2026-11-10, `real_working_set_replay`, its strictest mode, which
//! never credits a fact a working-set session had not been distilled into yet): of 21
//! top-level sessions that went on to edit a file an earlier session's fact names, the date
//! order injected such a fact into 8 and the working set into 13 (19 against 8 of 29 when a
//! fact counts from its session's start; allowing 8 per kind instead of 4 changed nothing).
//! Those counts are an upper bound on what the hook does, not its result. The replay keeps
//! the session bounds ([`NEWEST_SESSIONS`], [`RECENT_THREADS`], [`WORKING_SET_THREADS`]) and
//! [`WORKING_SET_MAX_PATHS`], but credits each session with every repo edit it and its
//! subagents made, with none of the hook's row windows ([`THREAD_ROWS_MAX`],
//! [`OLDER_EDITS_MAX`] for [`OLDER_EDITS_SESSIONS`] sessions, [`SUBAGENT_ROWS_MAX`] of
//! [`SESSION_SUBAGENTS_MAX`] transcripts for [`SUBAGENT_FALLBACK_SESSIONS`] sessions), where the
//! hook reads a session's subagents only when it made no repo edit itself. It also moves up a
//! fixed 4 (or 8) per kind, where the hook moves up fewer beside pinned facts
//! ([`promoted_max`]: 1 beside 5 pinned of 8, where the replay fills 3). The SessionStart
//! hook's median grew 3.1 ms in a debug build (22.1 to 25.2 ms, 20 runs each, interleaved,
//! on four repos: +4.8, +3.5, +1.6 and +1.4 ms).
//!
//! Only the SessionStart hook (startup / clear and the compaction refresher) reads
//! memory this way: MCP `project_memory` and the AGENTS.md block keep the date order, since
//! agents there ask `recall_*` about a topic.
//!
//! The working set ([`working_set`]): the paths edited or written (`file_edits`, ops `edit`
//! and `write`) in the newest [`WORKING_SET_THREADS`] top-level sessions of the project that
//! edited anything, among its [`RECENT_THREADS`] newest within the machine's
//! [`NEWEST_SESSIONS`] newest, sessions on the checked-out branch first, and those sessions'
//! files first too. A session's edits are its own thread's of the checkout's files (among its
//! newest [`THREAD_ROWS_MAX`] rows, else its newest [`OLDER_EDITS_MAX`] before them); one that
//! made none itself is read through its newest subagent transcripts instead
//! ([`SESSION_SUBAGENTS_MAX`]): an orchestrating session hands every edit to them. No git
//! call (the hook has a few hundred milliseconds): the branch is read from the
//! checkout's `HEAD` file ([`head_branch`]). An index without `file_edits` (before 0034) has
//! no working set, and the order is the date order.
//!
//! A fact names a working-set file when one of its keep-tokens ([`super::grounding`]) is
//! that path, or a suffix of it at a directory boundary ([`crate::gitlink::path_match`]):
//! `core/src/export.rs` names `apps/desktop/src-tauri/core/src/export.rs`, while a bare
//! `mod.rs` never names `src/db/mod.rs` (a repo has dozens), nor `src/lib.rs` the
//! `src/lib.rs` of one crate among several ([`names_path`]). A cheap pass over each fact's
//! words ([`file_words`]) narrows the candidates to those holding a working-set file name,
//! so the token check runs on a few facts, and the memory reader's filters (live only) and
//! order on those alone. A token longer than the working-set path names it only as its
//! absolute path in the project's checkout (the key) or in the one the session runs in (a
//! linked worktree keys on its main checkout, but its sessions cite their own paths)
//! ([`checkout_roots`]): a fact about another clone's `/x/old-app/src/lib/retry.ts` names
//! nothing here. That misses, never wrongly matches, a path into another linked worktree of
//! the repo or into the clone a borrowed repo-memory fact came from, and a relative path
//! prefixed with the checkout's own directory name (`ws-app/src/lib/retry.ts`): read from the
//! checkout, a longer relative token is another file (`packages/a/src/lib/retry.ts` is not the
//! root's `src/lib/retry.ts`) or one outside it (`../old-app/...`). A known limit stays: in a
//! monorepo, a path below a package (`src/routes/__root.tsx`)
//! names that file in every package that has one, since the index does not hold the repo's
//! file list that would tell them apart.

use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

use super::grounding::{keep_tokens, KeepToken};
use super::{
    fact_truth, get_project_memory_with, project_facts, repo_memory, MemoryFact, MemoryQuery,
    ProjectMemory, Slice,
};
use crate::scope::ProjectScope;

/// Sessions whose edits make the working set.
pub const WORKING_SET_THREADS: usize = 3;

/// The newest top-level sessions those come from. The branch preference applies within
/// them, so a months-old session on `main` never outranks this week's work elsewhere.
pub const RECENT_THREADS: usize = 30;

/// Top-level sessions of any project, newest first, the working set's come from: a repo with
/// none among them (108 days back on the measured index) has no working set, and its memory
/// goes by date. It bounds the hook's walk whatever the repo.
pub const NEWEST_SESSIONS: usize = 300;

/// Working-set paths at most, the checked-out branch's first, then the most recently edited:
/// one long session can touch hundreds of files, and a fact naming one it touched once an
/// hour ago says little.
pub const WORKING_SET_MAX_PATHS: usize = 20;

/// Working-set facts per kind moved up, at most: half the HOOK budget's 8. A caller that
/// shows fewer (the compaction refresher's 4 decisions, `cal hook -n 3`) or has pinned facts
/// gets fewer ([`promoted_max`]), so the newest facts keep at least half of what the pinned
/// ones leave.
pub const WORKING_SET_MAX_FACTS: usize = 4;

/// [`ProjectScope::Exact`]'s condition on the threads alias `t`, its key bound at `?{ph}`
/// (the one way to filter by project; its OR form selects what `COALESCE(project_key,
/// project_path) = ?` does).
fn project_condition(t: &str, ph: usize) -> String {
    // Exact always has a condition; "0" (no rows) only stands in should that change.
    ProjectScope::Exact(String::new())
        .condition_at(t, ph)
        .unwrap_or_else(|| "0".into())
}

/// The project's newest top-level sessions, `(id, on the checked-out branch)`, the branch's
/// first, then newest first: `?1` the project key, `?2` the branch (NULL when unknown), `?3`
/// [`RECENT_THREADS`], `?4` [`NEWEST_SESSIONS`]. The walk goes down
/// `idx_threads_subagent_updated` newest first over every project and stops at `?4` rows: no
/// index has the project and the date together, so finding a quiet repo's sessions by date
/// used to read every top-level thread (1,788 on the measured index, 3 ms of a debug build's
/// hook), and the project-key index instead sorts every thread of a big repo (134 ms cold for
/// one with 10,456, measured for `export::DIGEST_THREADS_SQL`).
fn recent_sessions_sql() -> String {
    format!(
        "WITH newest AS (
             SELECT t.id, t.git_branch, t.updated_at, t.project_key, t.project_path,
                    t.source_id, t.external_id
             FROM threads t WHERE t.is_subagent = 0
             ORDER BY t.updated_at DESC LIMIT ?4
         ), recent AS (
             SELECT n.id, n.git_branch, n.updated_at, n.source_id, n.external_id FROM newest n
             WHERE {} ORDER BY n.updated_at DESC LIMIT ?3
         )
         SELECT r.id, COALESCE(r.git_branch = ?2, 0) AS on_branch, r.source_id, r.external_id
         FROM recent r
         ORDER BY on_branch DESC, r.updated_at DESC",
        project_condition("n", 1)
    )
}

/// Rows of one thread's `file_edits` read, the newest. The table has no index on the
/// operation (`idx_file_edits_thread` is the thread alone), so finding a thread's edits means
/// fetching its reads and mentions too, 8 of every 9 rows on the measured index: a session
/// with 880 subagent transcripts held 20,403 rows for 808 edits. Read whole, one statement
/// per transcript, the working set took 15.7 ms of a debug build's hook on a repo whose
/// latest sessions ran workflows (the hook went from 21.9 to 44.8 ms). The newest rows hold
/// the latest edits, which are the ones the set keeps, except in a session that edited early
/// and then read and verified for longer ([`OLDER_EDITS_MAX`]).
pub const THREAD_ROWS_MAX: usize = 400;

/// Edits read from before a session's [`THREAD_ROWS_MAX`] window when that window held no
/// repo edit and the thread has more rows, and the sessions per call that get this read: it
/// walks up to the thread's 1,600 older rows (`indexer::file_edits::MAX_ROWS_PER_THREAD`'s
/// 2,000 less the window), and a project whose 30 recent sessions were all long and read-only
/// must not walk 48,000 at every SessionStart.
pub const OLDER_EDITS_MAX: usize = 50;
pub const OLDER_EDITS_SESSIONS: usize = 5;

/// Subagent transcripts consulted for a session that made no edit itself (an orchestrating
/// session hands every edit to them), the newest by id, and the rows read from each.
pub const SESSION_SUBAGENTS_MAX: usize = 20;
pub const SUBAGENT_ROWS_MAX: usize = 100;

/// Sessions without an edit of their own whose subagents are consulted, at most: a project
/// whose recent sessions only read (reviews, research) would otherwise walk the subagents of
/// all [`RECENT_THREADS`] of them on every SessionStart.
pub const SUBAGENT_FALLBACK_SESSIONS: usize = 5;

/// The edited or written paths among one thread's newest `?2` rows, each with its latest edit
/// time. The inner read walks `idx_file_edits_thread` backwards (newest row first, no sort)
/// and stops at the limit, whatever the thread holds.
const THREAD_EDITS_SQL: &str = "SELECT path, MAX(COALESCE(ts, 0)) FROM (
         SELECT path, op, ts FROM file_edits WHERE thread_id = ?1
         ORDER BY rowid DESC LIMIT ?2)
     WHERE op IN ('edit', 'write') GROUP BY path";

/// The rowid just past one thread's newest `?2` rows (the [`THREAD_EDITS_SQL`] window), none
/// when the window held all of them. Index-only, over the entries the window just read.
const PAST_WINDOW_SQL: &str = "SELECT rowid FROM file_edits WHERE thread_id = ?1
     ORDER BY rowid DESC LIMIT 1 OFFSET ?2";

/// The edited or written paths among one thread's newest `?3` edits from rowid `?2` back,
/// each with its latest edit time: the walk down `idx_file_edits_thread` starts where the
/// window ended and stops at the limit.
const OLDER_EDITS_SQL: &str = "SELECT path, MAX(COALESCE(ts, 0)) FROM (
         SELECT path, ts FROM file_edits
         WHERE thread_id = ?1 AND rowid <= ?2 AND op IN ('edit', 'write')
         ORDER BY rowid DESC LIMIT ?3)
     GROUP BY path";

/// The newest `?4` subagent transcripts of one session: `?1` its source, `?2`..`?3` the
/// `external_id` range (`indexer::subagent_id_range`), a range of the
/// `(source_id, external_id)` index. The range holds only subagent transcripts, so testing
/// `is_subagent` too only cost a table row per transcript (880 for one measured session)
/// where the index alone answers. Newest by id: agent ids are not time-ordered.
const NEWEST_SUBAGENTS_SQL: &str = "SELECT id FROM threads
     WHERE source_id = ?1 AND external_id >= ?2 AND external_id < ?3
     ORDER BY id DESC LIMIT ?4";

/// The project's working set ([module docs](self)), lowercased, at most
/// [`WORKING_SET_MAX_PATHS`]: the files of the sessions on the checked-out branch first (a
/// newer session elsewhere with more edits would otherwise fill every slot), then the most
/// recently edited. Empty on an index without `file_edits`. Paths outside the checkout
/// (`file_edits` stores those absolute: plan files, temp files) are left out: memory is about
/// the repo, and they would take the slots of its files. A session whose only writes were
/// such files made no edit: it is read through its subagents and takes none of the three.
pub fn working_set(conn: &Connection, project: &str, branch: Option<&str>) -> Result<Vec<String>> {
    if !crate::db::has_table(conn, "file_edits") {
        return Ok(Vec::new());
    }
    let sessions: Vec<(i64, bool, i64, String)> = conn
        .prepare_cached(&recent_sessions_sql())?
        .query_map(
            params![
                project,
                branch,
                RECENT_THREADS as i64,
                NEWEST_SESSIONS as i64
            ],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)? != 0,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<_>>()?;
    let mut edits_of = conn.prepare_cached(THREAD_EDITS_SQL)?;
    let mut past_window = conn.prepare_cached(PAST_WINDOW_SQL)?;
    let mut older_edits_of = conn.prepare_cached(OLDER_EDITS_SQL)?;
    let mut subagents_of = conn.prepare_cached(NEWEST_SUBAGENTS_SQL)?;
    // (on the branch, edit time, repo path) over the picked sessions.
    let mut edits: Vec<(bool, i64, String)> = Vec::new();
    // The repo paths among one statement's `(path, ts)` rows, counted: a write outside the
    // checkout is no edit, or a session that only wrote its plan would skip its subagents.
    let mut read_edits = |stmt: &mut rusqlite::CachedStatement<'_>,
                          args: &[&dyn rusqlite::ToSql],
                          on_branch: bool|
     -> Result<usize> {
        let mut n = 0;
        for row in stmt.query_map(args, |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
            let (path, ts) = row?;
            if let Some(p) = repo_path(&path) {
                edits.push((on_branch, ts, p));
                n += 1;
            }
        }
        Ok(n)
    };
    let (mut picked, mut deeper, mut fallbacks) = (0, 0, 0);
    for (session, on_branch, source_id, external_id) in sessions {
        if picked >= WORKING_SET_THREADS {
            break;
        }
        let window = THREAD_ROWS_MAX as i64;
        let mut found = read_edits(&mut edits_of, params![session, window], on_branch)?;
        // Edited early, then read and verified for longer than the window.
        if found == 0 && deeper < OLDER_EDITS_SESSIONS {
            let past: Option<i64> = past_window
                .query_row(params![session, window], |r| r.get(0))
                .optional()?;
            if let Some(rowid) = past {
                deeper += 1;
                let args = params![session, rowid, OLDER_EDITS_MAX as i64];
                found = read_edits(&mut older_edits_of, args, on_branch)?;
            }
        }
        // A session that edited nothing itself may have handed every edit to its subagents.
        if found == 0 && fallbacks < SUBAGENT_FALLBACK_SESSIONS {
            if let Some((lo, hi)) = crate::indexer::subagent_id_range(&external_id) {
                fallbacks += 1;
                let ids: Vec<i64> = subagents_of
                    .query_map(
                        params![source_id, lo, hi, SESSION_SUBAGENTS_MAX as i64],
                        |r| r.get(0),
                    )?
                    .collect::<rusqlite::Result<_>>()?;
                for id in ids {
                    let args = params![id, SUBAGENT_ROWS_MAX as i64];
                    found += read_edits(&mut edits_of, args, on_branch)?;
                }
            }
        }
        // Only sessions that edited something count toward the three.
        if found > 0 {
            picked += 1;
        }
    }
    edits.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    let mut out: Vec<String> = Vec::new();
    for (_, _, path) in edits {
        if !out.contains(&path) {
            out.push(path);
            if out.len() >= WORKING_SET_MAX_PATHS {
                break;
            }
        }
    }
    Ok(out)
}

/// `raw` as a lowercased, `/`-separated path relative to the checkout, or None for an
/// absolute or empty one.
fn repo_path(raw: &str) -> Option<String> {
    let p = raw.trim().replace('\\', "/");
    let p = p.trim_start_matches("./");
    let drive = p.as_bytes().get(1) == Some(&b':');
    if p.is_empty() || p.starts_with('/') || p.starts_with('~') || drive {
        return None;
    }
    Some(p.to_lowercase())
}

/// The branch checked out at `cwd`, from git's `HEAD` file (walking up to the checkout's
/// `.git`, following a linked worktree's `gitdir:` file), or None: detached, not a repo,
/// unreadable. Reads two small files and starts no git process.
pub fn head_branch(cwd: &Path) -> Option<String> {
    fn read_small(p: &Path) -> Option<String> {
        use std::io::Read;
        let mut s = String::new();
        std::fs::File::open(p)
            .ok()?
            .take(4096)
            .read_to_string(&mut s)
            .ok()?;
        Some(s)
    }
    for dir in cwd.ancestors() {
        let dot_git = dir.join(".git");
        let Ok(meta) = std::fs::metadata(&dot_git) else {
            continue;
        };
        let head = if meta.is_dir() {
            dot_git.join("HEAD")
        } else {
            let text = read_small(&dot_git)?;
            let gitdir = text
                .lines()
                .find_map(|l| l.trim().strip_prefix("gitdir:"))?
                .trim();
            dir.join(gitdir).join("HEAD")
        };
        return read_small(&head)?
            .trim()
            .strip_prefix("ref: refs/heads/")
            .filter(|b| !b.is_empty())
            .map(str::to_string);
    }
    None
}

/// Whether `text` names one of `working` (lowercased repo paths): a keep-token of one of its
/// words is that path or a suffix of it at a directory boundary. Only the words whose file
/// name is one of the working set's ([`file_words`]) are tokenized: `keep_tokens` over a
/// whole fact costs about 0.1 ms in a debug build, and this runs on every fact of the repo.
#[cfg(test)]
fn names_working_file(text: &str, project: &str, working: &[String]) -> bool {
    let names = file_names(working);
    names_working_file_in(text, &checkout_roots(project, project), &names, working)
}

/// [`names_working_file`] with `roots` ([`checkout_roots`]) and `names` ([`file_names`])
/// computed once per repo.
fn names_working_file_in(text: &str, roots: &[String], names: &[&str], working: &[String]) -> bool {
    file_words(text, |word, base| {
        has_name(names, base) && names_any(&path_tokens(word), roots, working)
    })
}

/// A checkout's path as an absolute keep-token spells it: lowercased and `/`-separated like
/// the tokens and the working set, and without a trailing slash.
fn checkout_root(project: &str) -> String {
    project
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_lowercase()
}

/// The checkouts whose absolute paths name working-set files ([`checkout_root`] of each): the
/// project key's, and `checkout`, the one the session runs in. They differ in a linked
/// worktree, which keys on its main checkout (`indexer::canonical_project`) while its own
/// `file_edits` paths, and the paths its sessions cite, are relative to the worktree
/// (`indexer::file_edits::checkout_root`).
fn checkout_roots(project: &str, checkout: &str) -> Vec<String> {
    let mut roots = vec![checkout_root(project)];
    let own = checkout_root(checkout);
    if !own.is_empty() && !roots.contains(&own) {
        roots.push(own);
    }
    roots
}

/// The file names (last segments) of `paths`, each once.
fn file_names(paths: &[String]) -> Vec<&str> {
    let mut out: Vec<&str> = paths
        .iter()
        .filter_map(|p| p.rsplit('/').next())
        .filter(|n| !n.is_empty())
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Whether `base` is one of `names` (lowercased), ASCII case aside. A scan, not a hash: at
/// most [`WORKING_SET_MAX_PATHS`] names, and the length check rejects nearly every word.
fn has_name(names: &[&str], base: &str) -> bool {
    names
        .iter()
        .any(|n| n.len() == base.len() && n.eq_ignore_ascii_case(base))
}

/// The keep-tokens of `text` that may name a file (lowercased): a directory or an extension,
/// which a plain identifier lacks.
fn path_tokens(text: &str) -> Vec<String> {
    keep_tokens(text)
        .into_iter()
        .flat_map(|t| match t {
            KeepToken::Exact(s) => vec![s],
            KeepToken::Loose { parts, .. } => parts,
            KeepToken::Near { .. } => Vec::new(),
        })
        .filter(|t| t.contains(['/', '.']))
        .map(|t| t.trim_start_matches("./").to_string())
        .collect()
}

fn names_any(tokens: &[String], roots: &[String], working: &[String]) -> bool {
    tokens
        .iter()
        .any(|t| working.iter().any(|w| names_path(t, roots, w)))
}

/// Whether keep-token `token` names the repo path `path` of the checkouts at `roots`
/// ([`checkout_roots`]): [`crate::gitlink::path_match`], except that a longer token must be
/// one checkout's own absolute path to it, and a shorter suffix ending in a file every package
/// of a monorepo has one of ([`is_package_file`]) must also hold a directory saying which
/// package. `src/lib.rs` is the `src/lib.rs` of every crate (in this repo, a fact about the
/// app crate's would name the core crate's), while `core/src/lib.rs` and `src/db/mod.rs`
/// name one.
fn names_path(token: &str, roots: &[String], path: &str) -> bool {
    if !crate::gitlink::path_match(token, path) {
        return false;
    }
    if token.len() == path.len() {
        return true;
    }
    // Longer: any absolute path ending in the repo path matched, so a fact about
    // `/x/old-app/src/lib/retry.ts` named this repo's `src/lib/retry.ts`. `path_match` saw
    // the `/` before `path`; what precedes it must be a checkout. A longer relative token
    // never is: from the checkout it is another file (`packages/a/src/lib/retry.ts`) or one
    // outside it (`../old-app/src/lib/retry.ts`).
    if token.len() > path.len() {
        return roots
            .iter()
            .any(|root| token.len() == root.len() + 1 + path.len() && token.starts_with(root));
    }
    let mut segments: Vec<&str> = token.split('/').collect();
    let name = segments.pop().unwrap_or_default();
    !is_package_file(name) || segments.iter().any(|d| !LAYOUT_DIRS.contains(d))
}

/// File names a crate, package or module has one of by convention, so a monorepo has one per
/// package: entry points, module files and manifests.
fn is_package_file(name: &str) -> bool {
    const NAMES: [&str; 11] = [
        "lib.rs",
        "main.rs",
        "mod.rs",
        "build.rs",
        "__init__.py",
        "__main__.py",
        "cargo.toml",
        "package.json",
        "tsconfig.json",
        "pyproject.toml",
        "readme.md",
    ];
    NAMES.contains(&name) || name.starts_with("index.") || name.starts_with("main.")
}

/// Directories that say where in a package a file sits, not which package it is.
const LAYOUT_DIRS: [&str; 12] = [
    "", "src", "lib", "app", "source", "sources", "pkg", "internal", "cmd", "bin", "test", "tests",
];

/// [`super::get_project_memory_facts_excluding`] (`exclude` None: every extractor) with the
/// facts that name a file of `working` ([`working_set`]) moved up: pinned facts first, then
/// per kind up to [`promoted_max`] of the `max_decisions` / `max_gotchas` the caller renders
/// that name one (newest first, wherever they sat by date, older than the fetched window
/// too), then the rest by date. Each list holds at most `per_kind` (more only when pinned
/// and working-set facts alone exceed it). `checkout` is the checkout the session runs in
/// (a linked worktree's own, where `project` is its main checkout's key; [`checkout_roots`]).
///
/// The working-set step is best effort: the date-ordered memory is already read, so an
/// error there (a damaged row, an index too old for a column) returns that memory instead of
/// failing the SessionStart hook, which would then print nothing at all.
#[allow(clippy::too_many_arguments)] // scope, checkout, three caps, filter, paths: all distinct
pub fn get_project_memory_for_working_set(
    conn: &Connection,
    project: &str,
    checkout: &str,
    per_kind: usize,
    max_decisions: usize,
    max_gotchas: usize,
    exclude: Option<&str>,
    working: &[String],
) -> Result<ProjectMemory> {
    let mut mem = get_project_memory_with(
        conn,
        project,
        &MemoryQuery {
            per_kind,
            exclude,
            ..Default::default()
        },
    )?;
    let roots = checkout_roots(project, checkout);
    if let Ok((decisions, gotchas)) = naming_facts(conn, project, &roots, exclude, working) {
        promote(&mut mem.decisions, decisions, per_kind, max_decisions);
        promote(&mut mem.gotchas, gotchas, per_kind, max_gotchas);
    }
    Ok(mem)
}

/// The live, unpinned decisions and gotchas of `project` that name a file of `working`,
/// newest first, at most [`WORKING_SET_MAX_FACTS`] each, with their truth fields filled.
fn naming_facts(
    conn: &Connection,
    project: &str,
    roots: &[String],
    exclude: Option<&str>,
    working: &[String],
) -> Result<(Vec<MemoryFact>, Vec<MemoryFact>)> {
    let mut paths: Vec<String> = Vec::new();
    for p in working.iter().filter_map(|p| repo_path(p)) {
        if !paths.contains(&p) && paths.len() < WORKING_SET_MAX_PATHS {
            paths.push(p);
        }
    }
    if paths.is_empty() {
        return Ok(Default::default());
    }
    let borrowed = repo_memory::borrowed_uids(conn, project);
    let borrowed = (!borrowed.is_empty())
        .then(|| serde_json::to_string(&borrowed))
        .transpose()?;
    let ids = candidate_ids(conn, project, roots, exclude, borrowed.as_deref(), &paths)?;
    if ids.is_empty() {
        return Ok(Default::default());
    }
    // The reader's own filters (live only) and order (newest first) on those ids alone.
    let rows = project_facts(
        conn,
        None,
        project,
        false,
        ids.len(),
        exclude,
        borrowed.as_deref(),
        Slice::Live,
        Some(&serde_json::to_string(&ids)?),
        false,
    )?;
    let found: Vec<(MemoryFact, String)> = rows.into_iter().filter(|(f, _)| !f.pinned).collect();
    let uids: Vec<&str> = found.iter().map(|(_, u)| u.as_str()).collect();
    let truths = fact_truth(conn, &uids)?;
    let (mut decisions, mut gotchas) = (Vec::new(), Vec::new());
    for (mut f, uid) in found {
        let list = if f.kind == "decision" {
            &mut decisions
        } else {
            &mut gotchas
        };
        if list.len() < WORKING_SET_MAX_FACTS {
            if let Some(t) = truths.get(&uid) {
                f.apply_truth(t.clone());
            }
            list.push(f);
        }
    }
    Ok((decisions, gotchas))
}

/// The ids of the project's unpinned decisions and gotchas whose text names a file of
/// `paths`, every one: the memory reader then keeps the live ones and orders them by session
/// time. A cut here by id would keep the wrong end, since a backlog is distilled newest
/// session first. Not in SQL: `LIKE '%x.rs%'` per path costs facts x paths x text length (20
/// paths over a repo's 800 facts is 16,000 substring scans), where one pass over each text
/// looks up the file name of each word holding a `.` and tokenizes only the words that match
/// ([`names_working_file_in`]). Never a playbook ([`super::MemoryQuery::playbooks`]): its
/// line quotes the fix commit's subject, which often names the file just edited.
fn candidate_ids(
    conn: &Connection,
    project: &str,
    roots: &[String],
    exclude: Option<&str>,
    borrowed: Option<&str>,
    paths: &[String],
) -> Result<Vec<i64>> {
    let names = file_names(paths);
    // The reader's scope (`project_facts_sql`): the project's threads, or a repo memory fact
    // another clone of the repo holds. Unpinned subagent facts are never live.
    let project_cond = project_condition("t", 1);
    let scope = if borrowed.is_some() {
        format!("({project_cond} OR f.uid IN (SELECT value FROM json_each(?2)))")
    } else {
        project_cond
    };
    let mut sql = format!(
        "SELECT f.id, f.text FROM facts f JOIN threads t ON t.id = f.thread_id
         WHERE f.kind IN ('decision', 'gotcha') AND {scope}
           AND f.hidden = 0 AND f.pinned = 0 AND t.is_subagent = 0
           AND f.extractor != '{}'",
        super::EXTRACTOR_PLAYBOOK
    );
    let mut args: Vec<&dyn rusqlite::ToSql> = vec![&project];
    if let Some(b) = &borrowed {
        args.push(b);
    }
    if let Some(x) = &exclude {
        sql.push_str(&format!(" AND f.extractor != ?{}", args.len() + 1));
        args.push(x);
    }
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(args.as_slice(), |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut ids = Vec::new();
    for row in rows {
        let (id, text) = row?;
        if names_working_file_in(&text, roots, &names, paths) {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// Calls `f(word, file_name)` for each word of `text` holding a `.` (every file name has
/// one), cut at a `#` anchor or a `::` item path and trimmed of the punctuation around it and
/// a `:42`, `:42:7` or `:249-260` line suffix, with its last path segment when that still
/// holds a `.`, until `f` says true. A superset of the words a keep-token naming a file
/// comes from, for ASCII names: a byte past ASCII ends a word, so a curly-quoted `a.rs`
/// still reads as `a.rs`. Splitting every text into words instead cost 24 ms over a repo's
/// 778 facts in a debug build. Slices only, no allocation.
fn file_words(text: &str, mut f: impl FnMut(&str, &str) -> bool) -> bool {
    fn is_line_ref(s: &str) -> bool {
        let digits = |p: &str| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit());
        match s.split_once('-') {
            Some((from, to)) => digits(from) && digits(to),
            None => digits(s),
        }
    }
    fn ends_word(c: u8) -> bool {
        c.is_ascii_whitespace()
            || c >= 0x80
            || matches!(
                c,
                b'`' | b'"'
                    | b'\''
                    | b'('
                    | b')'
                    | b'['
                    | b']'
                    | b'{'
                    | b'}'
                    | b'<'
                    | b'>'
                    | b','
                    | b';'
                    | b'*'
                    | b'|'
                    | b'='
            )
    }
    let b = text.as_bytes();
    let mut from = 0;
    while let Some(dot) = text[from..].find('.').map(|i| from + i) {
        let mut start = dot;
        while start > 0 && !ends_word(b[start - 1]) {
            start -= 1;
        }
        let mut end = dot + 1;
        while end < b.len() && !ends_word(b[end]) {
            end += 1;
        }
        // Only ASCII bytes lie between `start` and `end`, so both are char boundaries.
        let mut w = &text[start..end];
        // Agents cite `src/export.rs#L249` and `export.rs::agent_memory_md_budgeted`: the
        // file is before the anchor or the item path.
        if let Some(i) = w.find('#') {
            w = &w[..i];
        }
        if let Some(i) = w.find("::") {
            w = &w[..i];
        }
        w = w.trim_end_matches(['.', '!', '?', ':']);
        // `lib.rs:42`, `lib.rs:42:7`, `lib.rs:249-260`.
        for _ in 0..2 {
            if let Some((head, tail)) = w.rsplit_once(':') {
                if is_line_ref(tail) {
                    w = head;
                }
            }
        }
        let base = w.rsplit(['/', '\\']).next().unwrap_or(w);
        if base.contains('.') && f(w, base) {
            return true;
        }
        if end >= b.len() {
            break;
        }
        from = end;
    }
    false
}

/// Whether a word of `text` has one of `names` (lowercased) as its file name ([`file_words`]).
#[cfg(test)]
fn mentions_name(text: &str, names: &[&str]) -> bool {
    file_words(text, |_, base| has_name(names, base))
}

/// Working-set facts of one kind moved up when the caller renders `shown` of that kind and
/// `pinned` of them are pinned: at most [`WORKING_SET_MAX_FACTS`], and at most half of what
/// the pinned ones leave, so the newest keep the rest (4 under HOOK's 8, 2 after a
/// compaction's 4 decisions, 2 beside 4 pinned of 8, none with `cal hook -n 1`).
fn promoted_max(shown: usize, pinned: usize) -> usize {
    WORKING_SET_MAX_FACTS.min(shown.saturating_sub(pinned) / 2)
}

/// `found` placed right after `list`'s pinned facts (`list` is pinned first, as
/// `project_facts` orders it), at most [`promoted_max`] of them for `shown`, each fact once
/// (by id, and by text as the read dedupes), the list cut to `cap` unless the pinned and
/// working-set facts alone exceed it.
fn promote(list: &mut Vec<MemoryFact>, found: Vec<MemoryFact>, cap: usize, shown: usize) {
    if found.is_empty() {
        return;
    }
    let key = |f: &MemoryFact| f.text.trim().to_lowercase();
    let pinned = list.iter().take_while(|f| f.pinned).count();
    let pinned_texts: HashSet<String> = list[..pinned].iter().map(key).collect();
    let found: Vec<MemoryFact> = found
        .into_iter()
        .filter(|f| !pinned_texts.contains(&key(f)))
        .take(promoted_max(shown, pinned))
        .collect();
    let ids: HashSet<i64> = found.iter().map(|f| f.id).collect();
    let texts: HashSet<String> = found.iter().map(key).collect();
    list.retain(|f| f.pinned || !(ids.contains(&f.id) || texts.contains(&key(f))));
    let kept = pinned + found.len();
    let rest = list.split_off(pinned);
    list.extend(found);
    list.extend(rest);
    list.truncate(cap.max(kept));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::digest_test_support as dts;
    use crate::export::{agent_memory_md_budgeted, MemoryBudget};

    const P: &str = "/r/ws-app";

    fn edit(conn: &Connection, thread: i64, path: &str, op: &str) {
        edit_at(conn, thread, path, op, chrono::Utc::now().timestamp());
    }

    fn edit_at(conn: &Connection, thread: i64, path: &str, op: &str, ts: i64) {
        conn.execute(
            "INSERT INTO file_edits (thread_id, path, op, ts) VALUES (?1, ?2, ?3, ?4)",
            params![thread, path, op, ts],
        )
        .unwrap();
    }

    fn set_branch(conn: &Connection, thread: i64, branch: &str) {
        conn.execute(
            "UPDATE threads SET git_branch = ?2 WHERE id = ?1",
            params![thread, branch],
        )
        .unwrap();
    }

    /// The HOOK block of `P`, by date or with `working`.
    fn hook(conn: &Connection, working: Option<&[String]>) -> String {
        let b = MemoryBudget::HOOK;
        let (n, d, g) = (b.fetch_limit(), b.max_decisions, b.max_gotchas);
        let mem = match working {
            Some(w) => get_project_memory_for_working_set(conn, P, P, n, d, g, None, w),
            None => super::super::get_project_memory_facts(conn, P, b.fetch_limit()),
        }
        .unwrap();
        agent_memory_md_budgeted(P, &mem, None, &b)
    }

    /// A project with an old decision naming a file, buried under 12 newer ones: by date the
    /// hook leaves it out; once the latest session edits that file it is injected, after the
    /// pinned fact and ahead of the newest ones.
    #[test]
    fn a_fact_naming_a_file_the_branch_edits_reaches_the_hook() {
        let mut conn = dts::temp_db("ws_hook");
        let old = dts::session(&mut conn, "ws-old", P, "old", 90_000, &["go"]);
        dts::fact(
            &conn,
            old,
            "decision",
            "Invoice totals round once, in `src/billing/invoice.rs`, never per line",
            "llm",
        );
        dts::fact(&conn, old, "decision", "Keep `mod.rs` files small", "llm");
        for i in 0..12 {
            let t = dts::session(
                &mut conn,
                &format!("ws-{i}"),
                P,
                "s",
                80_000 - i * 1000,
                &[],
            );
            dts::fact(
                &conn,
                t,
                "decision",
                &format!("Newer decision number {i}"),
                "llm",
            );
        }
        let pinned = dts::session(&mut conn, "ws-pin", P, "pin", 100_000, &[]);
        dts::fact(&conn, pinned, "decision", "Pinned rule stays first", "user");
        conn.execute("UPDATE facts SET pinned = 1 WHERE thread_id = ?1", [pinned])
            .unwrap();
        let latest = dts::session(&mut conn, "ws-now", P, "now", 60, &["fix rounding"]);
        edit(&conn, latest, "apps/api/src/billing/invoice.rs", "edit");
        edit(&conn, latest, "apps/api/src/db/mod.rs", "edit");
        // A read is not an edit, and a file outside the checkout is not the repo's.
        edit(&conn, latest, "apps/api/src/lib.rs", "read");
        edit(&conn, latest, "/Users/me/.claude/plans/x.md", "write");

        let by_date = hook(&conn, None);
        assert!(!by_date.contains("src/billing/invoice.rs"), "{by_date}");

        let working = working_set(&conn, P, Some("main")).unwrap();
        let mut sorted = working.clone();
        sorted.sort();
        assert_eq!(
            sorted,
            vec![
                "apps/api/src/billing/invoice.rs".to_string(),
                "apps/api/src/db/mod.rs".into()
            ]
        );
        let block = hook(&conn, Some(&working));
        let at = |needle: &str| {
            block
                .find(needle)
                .unwrap_or_else(|| panic!("{needle}: {block}"))
        };
        assert!(at("Pinned rule stays first") < at("src/billing/invoice.rs"));
        assert!(at("src/billing/invoice.rs") < at("Newer decision number 11"));
        // A bare `mod.rs` names no file: a repo has dozens.
        assert!(!block.contains("Keep `mod.rs` files small"), "{block}");
        // Still 8 decisions: the working-set fact took the oldest shown one's place.
        assert!(by_date.contains("Newer decision number 5 ("), "{by_date}");
        assert!(!block.contains("Newer decision number 5 ("), "{block}");
        assert_eq!(block.matches("Newer decision number").count(), 6, "{block}");
    }

    /// The branch's sessions come first among the newest, and only sessions that edited
    /// something count toward the three.
    #[test]
    fn the_working_set_prefers_the_branch_and_sessions_with_edits() {
        let mut conn = dts::temp_db("ws_branch");
        let mut thread = |id: &str, age: i64, branch: &str, file: Option<&str>| {
            let t = dts::session(&mut conn, id, P, id, age, &[]);
            set_branch(&conn, t, branch);
            if let Some(f) = file {
                edit(&conn, t, f, "write");
            }
        };
        thread("b1", 5_000, "feat/x", Some("src/feat_one.rs"));
        thread("m1", 4_000, "main", Some("src/main_one.rs"));
        thread("m2", 3_000, "main", Some("src/main_two.rs"));
        thread("m3", 2_000, "main", Some("src/main_three.rs"));
        thread("q", 1_000, "feat/x", None);
        let on_feature = working_set(&conn, P, Some("feat/x")).unwrap();
        assert_eq!(on_feature.len(), 3, "{on_feature:?}");
        assert!(on_feature.contains(&"src/feat_one.rs".to_string()));
        assert!(!on_feature.contains(&"src/main_one.rs".to_string()));
        let unknown = working_set(&conn, P, None).unwrap();
        assert!(
            !unknown.contains(&"src/feat_one.rs".to_string()),
            "{unknown:?}"
        );
        assert!(working_set(&conn, "/r/other", None).unwrap().is_empty());
    }

    /// A session that edited early and then read and verified for longer than its
    /// [`THREAD_ROWS_MAX`] window still edited: its edits are read from before the window,
    /// and it keeps its place among the three.
    #[test]
    fn a_session_that_edited_early_keeps_its_edits() {
        let mut conn = dts::temp_db("ws_early");
        for (id, age, file) in [("s2", 2_000, "a"), ("s3", 3_000, "b"), ("s4", 4_000, "c")] {
            let t = dts::session(&mut conn, id, P, id, age, &[]);
            edit(&conn, t, &format!("src/{file}.rs"), "edit");
        }
        let long = dts::session(&mut conn, "long", P, "long", 600, &[]);
        edit(&conn, long, "/r/home/.claude/plans/p.md", "write");
        edit(&conn, long, "src/billing/invoice.rs", "edit");
        for i in 0..THREAD_ROWS_MAX + 50 {
            edit(&conn, long, &format!("src/read_{}.rs", i % 40), "read");
        }
        let working = working_set(&conn, P, Some("main")).unwrap();
        let mut sorted = working.clone();
        sorted.sort();
        assert_eq!(
            sorted,
            vec![
                "src/a.rs".to_string(),
                "src/b.rs".into(),
                "src/billing/invoice.rs".into()
            ]
        );
    }

    /// The checked-out branch's files come first: newer sessions elsewhere with more than
    /// [`WORKING_SET_MAX_PATHS`] newer edits between them leave every one in the working set.
    #[test]
    fn the_branch_files_survive_busier_sessions_elsewhere() {
        let mut conn = dts::temp_db("ws_branch_paths");
        let now = chrono::Utc::now().timestamp();
        let feat = dts::session(&mut conn, "f1", P, "f1", 86_400, &[]);
        set_branch(&conn, feat, "feat/x");
        for i in 0..5 {
            edit_at(
                &conn,
                feat,
                &format!("src/feat_{i}.rs"),
                "edit",
                now - 86_400,
            );
        }
        for (n, id) in ["m1", "m2"].into_iter().enumerate() {
            let t = dts::session(&mut conn, id, P, id, 600 - n as i64 * 100, &[]);
            for i in 0..13i64 {
                edit_at(&conn, t, &format!("src/{id}_{i}.rs"), "edit", now - 60 + i);
            }
        }
        let on_feature = working_set(&conn, P, Some("feat/x")).unwrap();
        assert_eq!(on_feature.len(), WORKING_SET_MAX_PATHS, "{on_feature:?}");
        for i in 0..5 {
            let want = format!("src/feat_{i}.rs");
            assert!(on_feature.contains(&want), "{want}: {on_feature:?}");
        }
        // With no branch known, the newest edits fill it.
        let unknown = working_set(&conn, P, None).unwrap();
        assert!(
            !unknown.iter().any(|p| p.starts_with("src/feat_")),
            "{unknown:?}"
        );
    }

    /// An orchestrating session's edits are its subagents' (Claude Code writes them beside
    /// it, `<session>/subagents/agent-*.jsonl`): it counts as a session with edits, and their
    /// files are in its working set.
    #[test]
    fn subagent_edits_belong_to_their_session() {
        let mut conn = dts::temp_db("ws_subagents");
        let older = dts::session(&mut conn, "slug/older.jsonl", P, "older", 9_000, &[]);
        edit(&conn, older, "src/older.rs", "edit");
        dts::session(&mut conn, "slug/lead.jsonl", P, "lead", 600, &[]);
        let agent = dts::session(
            &mut conn,
            "slug/lead/subagents/agent-a1.jsonl",
            P,
            "agent",
            500,
            &[],
        );
        conn.execute("UPDATE threads SET is_subagent = 1 WHERE id = ?1", [agent])
            .unwrap();
        edit(&conn, agent, "src/delegated.rs", "write");
        let working = working_set(&conn, P, Some("main")).unwrap();
        assert!(
            working.contains(&"src/delegated.rs".to_string()),
            "{working:?}"
        );
        assert!(working.contains(&"src/older.rs".to_string()), "{working:?}");
    }

    /// A write outside the checkout (a plan, a scratch brief) is no edit of the repo: a
    /// session whose only one it was is still read through its subagents, and sessions that
    /// only planned take none of the three.
    #[test]
    fn writes_outside_the_checkout_are_not_edits() {
        let mut conn = dts::temp_db("ws_plans");
        let older = dts::session(&mut conn, "slug/older.jsonl", P, "older", 9_000, &[]);
        edit(&conn, older, "src/older.rs", "edit");
        for (i, age) in [(1, 800), (2, 700), (3, 650)] {
            let id = format!("slug/plan{i}.jsonl");
            let t = dts::session(&mut conn, &id, P, "plan", age, &[]);
            edit(&conn, t, &format!("/r/home/.claude/plans/p{i}.md"), "write");
        }
        let lead = dts::session(&mut conn, "slug/lead.jsonl", P, "lead", 600, &[]);
        edit(&conn, lead, "/r/scratch/brief.md", "write");
        let agent = "slug/lead/subagents/agent-a1.jsonl";
        let agent = dts::session(&mut conn, agent, P, "agent", 500, &[]);
        conn.execute("UPDATE threads SET is_subagent = 1 WHERE id = ?1", [agent])
            .unwrap();
        edit(&conn, agent, "src/delegated.rs", "write");
        let mut working = working_set(&conn, P, Some("main")).unwrap();
        working.sort();
        assert_eq!(
            working,
            vec!["src/delegated.rs".to_string(), "src/older.rs".into()]
        );
    }

    /// Every fact naming a working-set file is a candidate, so the newest by session time
    /// win even when they were written first (a backlog distills its newest session first)
    /// and a hundred older ones name the same file.
    #[test]
    fn the_newest_facts_naming_a_file_win_whatever_their_ids() {
        let mut conn = dts::temp_db("ws_many");
        let newest = dts::session(&mut conn, "ws-new", P, "new", 5_000, &[]);
        let rule = |who: &str, i: usize| format!("{who} rule {i} for `src/billing/invoice.rs`");
        for i in 0..WORKING_SET_MAX_FACTS {
            dts::fact(&conn, newest, "decision", &rule("Newest", i), "llm");
        }
        let old = dts::session(&mut conn, "ws-old", P, "old", 90_000, &[]);
        for i in 0..100 {
            dts::fact(&conn, old, "decision", &rule("Old", i), "llm");
        }
        for i in 0..12i64 {
            let t = dts::session(&mut conn, &format!("ws-{i}"), P, "s", 1_000 + i * 10, &[]);
            dts::fact(
                &conn,
                t,
                "decision",
                &format!("Unrelated decision {i}"),
                "llm",
            );
        }
        let b = MemoryBudget::HOOK;
        let working = vec!["apps/api/src/billing/invoice.rs".to_string()];
        let (n, d, g) = (b.fetch_limit(), b.max_decisions, b.max_gotchas);
        let mem = get_project_memory_for_working_set(&conn, P, P, n, d, g, None, &working).unwrap();
        let shown: Vec<&str> = mem
            .decisions
            .iter()
            .take(b.max_decisions)
            .map(|f| f.text.as_str())
            .collect();
        for i in 0..WORKING_SET_MAX_FACTS {
            assert!(shown.contains(&rule("Newest", i).as_str()), "{shown:?}");
        }
    }

    /// A playbook's line quotes its fix commit's subject, which often names the file just
    /// edited: it is never a working-set candidate, so it is never promoted into the hook
    /// (`MemoryQuery::playbooks`), while a distilled gotcha naming the file is.
    #[test]
    fn a_playbook_naming_a_working_set_file_is_not_promoted() {
        let mut conn = dts::temp_db("ws_playbook");
        let old = dts::session(&mut conn, "ws-pb-old", P, "old", 90_000, &["go"]);
        let gotcha = "Totals in `src/billing/invoice.rs` round once";
        dts::fact(&conn, old, "gotcha", gotcha, "llm");
        let fixer = dts::session(&mut conn, "ws-pb-fix", P, "fix", 600, &["fix"]);
        let playbook = format!(
            "error[E0308]: mismatched types: seen 2 times; fixed in abc1234 \
             (fix: cast in src/billing/invoice.rs), see thread {fixer}"
        );
        let origin = format!("playbook:e0308@{P}");
        let now = chrono::Utc::now().timestamp();
        super::super::upsert_playbook(&conn, &origin, fixer, &playbook, now).unwrap();
        let id_of = |text: &str| -> i64 {
            conn.query_row("SELECT id FROM facts WHERE text = ?1", [text], |r| r.get(0))
                .unwrap()
        };
        let working = vec!["apps/api/src/billing/invoice.rs".to_string()];
        assert_eq!(
            candidate_ids(&conn, P, &checkout_roots(P, P), None, None, &working).unwrap(),
            vec![id_of(gotcha)]
        );
        let mem =
            get_project_memory_for_working_set(&conn, P, P, 40, 8, 8, None, &working).unwrap();
        let texts: Vec<&str> = mem.gotchas.iter().map(|f| f.text.as_str()).collect();
        assert_eq!(texts, [gotcha]);
        let block = hook(&conn, Some(&working));
        assert!(!block.contains("mismatched types"), "{block}");
    }

    /// The working-set facts take at most half of the slots the pinned facts leave of what
    /// the caller shows, so the newest keep the rest: 4 under HOOK's 8 (above), 2 after a
    /// compaction's 4 decisions, 2 beside 4 pinned of 8, none with `cal hook -n 1`.
    #[test]
    fn the_working_set_leaves_the_newest_half_the_slots() {
        let mut conn = dts::temp_db("ws_caps");
        let old = dts::session(&mut conn, "ws-old", P, "old", 90_000, &[]);
        for i in 0..6 {
            let text = format!("Old rule {i} for `src/billing/invoice.rs`");
            dts::fact(&conn, old, "decision", &text, "llm");
        }
        for i in 0..12i64 {
            let t = dts::session(&mut conn, &format!("ws-{i}"), P, "s", 1_000 + i * 10, &[]);
            dts::fact(&conn, t, "decision", &format!("Newest {i}"), "llm");
        }
        let working = vec!["apps/api/src/billing/invoice.rs".to_string()];
        // (working-set facts, newest facts) among the first `cap` decisions, which is what
        // the renderer shows of a pinned-first list.
        let shown = |conn: &Connection, cap: usize| {
            let mem =
                get_project_memory_for_working_set(conn, P, P, 40, cap, 8, None, &working).unwrap();
            let top = &mem.decisions[..cap.min(mem.decisions.len())];
            let count = |p: &str| top.iter().filter(|f| f.text.starts_with(p)).count();
            (count("Old rule"), count("Newest"))
        };
        assert_eq!(shown(&conn, 8), (4, 4));
        assert_eq!(shown(&conn, 4), (2, 2));
        assert_eq!(shown(&conn, 1), (0, 1));
        let pin = dts::session(&mut conn, "ws-pin", P, "pin", 100_000, &[]);
        for i in 0..4 {
            dts::fact(&conn, pin, "decision", &format!("Pinned {i}"), "user");
        }
        conn.execute("UPDATE facts SET pinned = 1 WHERE thread_id = ?1", [pin])
            .unwrap();
        assert_eq!(shown(&conn, 8), (2, 2));
    }

    /// An index from before 0034 has no `file_edits`: no working set, the date order.
    #[test]
    fn an_index_without_file_edits_keeps_the_date_order() {
        let mut conn = dts::temp_db("ws_old");
        let t = dts::session(&mut conn, "o1", P, "o", 600, &[]);
        dts::fact(&conn, t, "decision", "Use `src/a.rs` for the parser", "llm");
        conn.execute_batch("DROP TABLE file_edits").unwrap();
        assert!(working_set(&conn, P, Some("main")).unwrap().is_empty());
        let mem =
            get_project_memory_for_working_set(&conn, P, P, 40, 8, 8, None, &["src/a.rs".into()])
                .unwrap();
        assert_eq!(mem.decisions.len(), 1);
    }

    #[test]
    fn a_fact_names_a_file_only_at_a_directory_boundary() {
        let w = vec!["apps/desktop/src-tauri/core/src/export.rs".to_string()];
        for (text, names) in [
            ("Budgets live in `core/src/export.rs`", true),
            ("Budgets live in core/src/export.rs:249", true),
            ("See ./src/export.rs for the budgets", true),
            (
                "Budgets live in /r/ws-app/apps/desktop/src-tauri/core/src/export.rs",
                true,
            ),
            // Line ranges, GitHub anchors and Rust item paths cite the file too.
            (
                "Budgets are in core/src/export.rs:249-260; change them there",
                true,
            ),
            ("See `src/export.rs#L249` for the cut", true),
            (
                "`src/export.rs::agent_memory_md_budgeted` renders them",
                true,
            ),
            // Another clone's or repo's file is not this checkout's.
            (
                "Budgets live in /r/old-app/apps/desktop/src-tauri/core/src/export.rs",
                false,
            ),
            (
                "Upstream: https://github.com/acme/other/blob/main/core/src/export.rs#L249",
                false,
            ),
            ("Budgets live in core/src/export.rs:1.2.3", false),
            ("Bumped the parser to 1.2.3", false),
            ("Budgets live in `export.rs`", false),
            ("Budgets live in `src/reexport.rs`", false),
            ("Budgets live in the export module", false),
        ] {
            assert_eq!(names_working_file(text, P, &w), names, "{text}");
            // The cheap pass lets through every fact the token check accepts.
            assert!(!names || mentions_name(text, &["export.rs"]), "{text}");
        }
        let file = ["export.rs"];
        for text in [
            "See (core/src/Export.rs).",
            "In \u{201C}src/export.rs\u{201D}",
            "export.rs:249:7 holds it",
            "a=src\\export.rs",
        ] {
            assert!(mentions_name(text, &file), "{text}");
        }
        assert!(!mentions_name("the exporters in reexport.rs", &file));
        // An absolute path names a file only as this checkout's own path to it, whatever the
        // key's case or trailing slash: a short repo path is the tail of many other files.
        let retry = vec!["src/lib/retry.ts".to_string()];
        for (text, project, names) in [
            ("Ported from /r/old-app/src/lib/retry.ts", P, false),
            ("Ported from /r/ws-app/src/lib/retry.ts", P, true),
            ("Ported from /r/ws-app/src/lib/retry.ts", "/R/WS-App/", true),
            (
                "Ported from C:\\r\\ws-app\\src\\lib\\retry.ts",
                "C:\\r\\ws-app",
                true,
            ),
            ("Ported from ../old-app/src/lib/retry.ts", P, false),
            // Read from the checkout, a longer relative path is another of its files.
            ("Ported from packages/old/src/lib/retry.ts", P, false),
        ] {
            assert_eq!(names_working_file(text, project, &retry), names, "{text}");
        }
        // In a linked worktree (keyed on its main checkout) the worktree's own path names the
        // file too, and a sibling worktree's still does not.
        let (retry_names, roots) = (file_names(&retry), checkout_roots(P, "/r/ws-app-fix/"));
        for (text, names) in [
            ("Ported from /r/ws-app-fix/src/lib/retry.ts", true),
            ("Ported from /r/ws-app/src/lib/retry.ts", true),
            ("Ported from /r/ws-app-old/src/lib/retry.ts", false),
        ] {
            let got = names_working_file_in(text, &roots, &retry_names, &retry);
            assert_eq!(got, names, "{text}");
        }
        // A root file is named by its own name.
        assert!(names_working_file(
            "Pin versions in `Cargo.toml`",
            P,
            &["cargo.toml".into()]
        ));
        // A file every package has one of needs a directory saying which package: in a
        // monorepo `src/lib.rs` is every crate's.
        let lib = vec!["apps/desktop/src-tauri/core/src/lib.rs".to_string()];
        for (text, names) in [
            ("Tauri commands in `src/lib.rs` stay async", false),
            ("Contracts at the top of `core/src/lib.rs`", true),
            ("See src-tauri/core/src/lib.rs:12", true),
            (
                "Contracts in /r/ws-app/apps/desktop/src-tauri/core/src/lib.rs",
                true,
            ),
        ] {
            assert_eq!(names_working_file(text, P, &lib), names, "{text}");
        }
        let index = vec!["apps/web/src/index.ts".to_string()];
        assert!(!names_working_file("Export from `src/index.ts`", P, &index));
        assert!(names_working_file(
            "Export from `web/src/index.ts`",
            P,
            &index
        ));
        assert!(names_working_file(
            "Keep `src/db/mod.rs` small",
            P,
            &["core/src/db/mod.rs".into()]
        ));
        // The repo's own `src/lib.rs`, by its whole path.
        assert!(names_working_file(
            "Tauri commands in `src/lib.rs`",
            P,
            &["src/lib.rs".into()]
        ));
        assert_eq!(repo_path("/abs/x.rs"), None);
        assert_eq!(repo_path("C:\\x\\y.rs"), None);
        assert_eq!(repo_path("~/x.rs"), None);
        assert_eq!(repo_path(".\\Src\\Y.rs").as_deref(), Some("src/y.rs"));
    }

    /// The session's own checkout reaches the token check: in a linked worktree, which keys
    /// on its main checkout, a fact citing the worktree's absolute path is moved up, and with
    /// the main checkout alone it keeps its place by date.
    #[test]
    fn a_linked_worktree_names_its_own_absolute_paths() {
        let mut conn = dts::temp_db("ws_worktree");
        let old = dts::session(&mut conn, "ws-old", P, "old", 90_000, &[]);
        let cited = "Retries back off in /r/ws-app-fix/src/lib/retry.ts";
        dts::fact(&conn, old, "decision", cited, "llm");
        let new = dts::session(&mut conn, "ws-new", P, "new", 1_000, &[]);
        dts::fact(&conn, new, "decision", "Newest decision", "llm");
        let working = vec!["src/lib/retry.ts".to_string()];
        let first = |checkout: &str| {
            let mem =
                get_project_memory_for_working_set(&conn, P, checkout, 40, 8, 8, None, &working)
                    .unwrap();
            mem.decisions[0].text.clone()
        };
        assert_eq!(first("/r/ws-app-fix"), cited);
        assert_eq!(first(P), "Newest decision");
    }

    #[test]
    fn head_branch_reads_the_checkout_and_a_linked_worktree() {
        let dir = crate::db::test_support::TestDir::new("ws_head");
        let root = dir.parent().unwrap();
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src/deep")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/feat/ws\n").unwrap();
        assert_eq!(
            head_branch(&repo.join("src/deep")).as_deref(),
            Some("feat/ws")
        );

        let gitdir = repo.join(".git/worktrees/wt");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/other\n").unwrap();
        let wt = root.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: ../repo/.git/worktrees/wt\n").unwrap();
        assert_eq!(head_branch(&wt).as_deref(), Some("other"));

        std::fs::write(
            repo.join(".git/HEAD"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();
        assert_eq!(head_branch(&repo), None, "detached");
    }

    /// The working set's sessions come from walking `idx_threads_subagent_updated` newest
    /// first (no sort of a big repo's threads), their subagents from a range of the
    /// `(source_id, external_id)` index, and the edits by thread through
    /// `idx_file_edits_thread`, never a scan of `file_edits`.
    #[test]
    fn the_working_set_queries_walk_the_indexes() {
        let conn = dts::temp_db("ws_plan");
        let plan = |sql: &str, args: &[&dyn rusqlite::ToSql]| {
            let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let rows = stmt
                .query_map(args, |r| r.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            rows.join(" | ")
        };
        let p = plan(&recent_sessions_sql(), params![P, "main", 30i64, 300i64]);
        assert!(p.contains("idx_threads_subagent_updated"), "{p}");
        let p = plan(NEWEST_SUBAGENTS_SQL, params![1i64, "a", "b", 20i64]);
        assert!(p.contains("(source_id=? AND external_id>?"), "{p}");
        // No table row per transcript: the index alone answers.
        assert!(p.contains("COVERING INDEX"), "{p}");
        // The newest rows come off the thread index backwards: no scan, and no sort that
        // would read every row of the thread before the limit applies.
        let p = plan(THREAD_EDITS_SQL, params![1i64, 400i64]);
        assert!(p.contains("idx_file_edits_thread"), "{p}");
        assert!(!p.contains("SCAN file_edits"), "{p}");
        assert!(!p.contains("ORDER BY"), "{p}");
        let p = plan(PAST_WINDOW_SQL, params![1i64, 400i64]);
        assert!(p.contains("COVERING INDEX idx_file_edits_thread"), "{p}");
        assert!(!p.contains("ORDER BY"), "{p}");
        // The older edits continue the same walk from where the window ended.
        let p = plan(OLDER_EDITS_SQL, params![1i64, 9i64, 50i64]);
        assert!(
            p.contains("idx_file_edits_thread (thread_id=? AND rowid<?)"),
            "{p}"
        );
        assert!(!p.contains("SCAN file_edits"), "{p}");
        assert!(!p.contains("ORDER BY"), "{p}");
    }

    /// The gate this ranking shipped on: replays SessionStart for every top-level session of
    /// a real index that went on to edit (`file_edits`) a file some prior fact names, and
    /// counts the sessions whose HOOK block would have held such a fact, by date and with the
    /// working set. Everything is as of the session's start: facts created before it and not
    /// yet superseded, edits other sessions made before it, their updates capped at it. A
    /// subagent's edits and facts are its session's. The third "prior" mode is the one to
    /// quote: it never credits the working set with a fact not yet distilled.
    /// Reaffirmations are not replayed (by date means `decided_at`), nor the text dedupe.
    /// Read-only; prints counts and project ranks only, never text or paths.
    ///
    /// CALLIMACHUS_GROUNDING_DB=<an APFS clone of index.db> cargo test -p callimachus-core \
    ///   real_working_set_replay -- --ignored --nocapture
    #[test]
    #[ignore = "needs CALLIMACHUS_GROUNDING_DB, a clone of a real index"]
    fn real_working_set_replay() {
        use std::collections::HashMap;
        let Ok(path) = std::env::var("CALLIMACHUS_GROUNDING_DB") else {
            eprintln!("set CALLIMACHUS_GROUNDING_DB to an index clone");
            return;
        };
        let conn = crate::db::open_readonly(Path::new(&path)).unwrap();
        let budget = MemoryBudget::HOOK.max_decisions;

        // Each Claude Code subagent transcript's session (`indexer::session_thread_ids`
        // read the other way): its edits and facts count as that session's.
        let mut parents: HashMap<i64, i64> = HashMap::new();
        let mut stmt = conn
            .prepare(
                "SELECT c.id, p.id FROM threads c
                 JOIN threads p ON p.source_id = c.source_id
                  AND p.external_id = substr(c.external_id, 1,
                        instr(c.external_id, '/subagents/') - 1) || '.jsonl'
                 WHERE c.is_subagent = 1 AND instr(c.external_id, '/subagents/') > 0",
            )
            .unwrap();
        for row in stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
            .unwrap()
        {
            let (child, parent) = row.unwrap();
            parents.insert(child, parent);
        }

        struct Fact {
            id: i64,
            uid: String,
            /// The session it was distilled from (a subagent's parent).
            thread: i64,
            decision: bool,
            text: String,
            tokens: Vec<String>,
            pinned: bool,
            created: i64,
            decided: i64,
        }
        let mut facts: HashMap<String, Vec<Fact>> = HashMap::new();
        let sql = format!(
            "SELECT f.id, COALESCE(f.uid, ''), f.kind, f.text, f.pinned, f.created_at,
                    {}, COALESCE(t.project_key, t.project_path), f.thread_id
             FROM facts f JOIN threads t ON t.id = f.thread_id
             WHERE f.kind IN ('decision', 'gotcha') AND f.hidden = 0
               AND (t.is_subagent = 0 OR f.pinned = 1) AND f.extractor != ?1
               AND COALESCE(t.project_key, t.project_path) IS NOT NULL",
            super::super::DECIDED_AT_SQL
        );
        let mut stmt = conn.prepare(&sql).unwrap();
        let rows = stmt
            .query_map([super::super::harvest::EXTRACTOR_CLAUDE_MEMORY], |r| {
                let text: String = r.get(3)?;
                let thread: i64 = r.get(8)?;
                Ok((
                    r.get::<_, String>(7)?,
                    Fact {
                        id: r.get(0)?,
                        uid: r.get(1)?,
                        thread: parents.get(&thread).copied().unwrap_or(thread),
                        decision: r.get::<_, String>(2)? == "decision",
                        tokens: path_tokens(&text),
                        text,
                        pinned: r.get::<_, i64>(4)? != 0,
                        created: r.get(5)?,
                        decided: r.get(6)?,
                    },
                ))
            })
            .unwrap();
        for row in rows {
            let (p, f) = row.unwrap();
            facts.entry(p).or_default().push(f);
        }
        // to_uid -> (created_at, dismissed_at) of each supersedes link onto it.
        let mut links: HashMap<String, Vec<(i64, Option<i64>)>> = HashMap::new();
        if crate::db::has_table(&conn, "fact_links") {
            let mut stmt = conn
                .prepare(
                    "SELECT to_uid, created_at, dismissed_at FROM fact_links
                     WHERE kind = 'supersedes'",
                )
                .unwrap();
            for row in stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                    ))
                })
                .unwrap()
            {
                let (to, at, gone) = row.unwrap();
                links.entry(to).or_default().push((at, gone));
            }
        }
        let superseded_at = |uid: &str, at: i64| {
            links
                .get(uid)
                .is_some_and(|l| l.iter().any(|(c, d)| *c < at && d.is_none_or(|d| d > at)))
        };
        // Top-level sessions and their repo edits (path, ts).
        type Sess = (i64, i64, i64, Option<String>);
        let mut sessions: HashMap<String, Vec<Sess>> = HashMap::new();
        let mut everyone: Vec<(i64, i64)> = Vec::new();
        let mut stmt = conn
            .prepare(
                "SELECT id, COALESCE(project_key, project_path), created_at,
                        COALESCE(updated_at, created_at), git_branch
                 FROM threads WHERE is_subagent = 0 AND created_at > 0
                   AND COALESCE(project_key, project_path) IS NOT NULL",
            )
            .unwrap();
        for row in stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(1)?,
                    (
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, Option<String>>(4)?,
                    ),
                ))
            })
            .unwrap()
        {
            let (p, s) = row.unwrap();
            // Every project's, for the NEWEST_SESSIONS bound: (created, updated).
            everyone.push((s.1, s.2));
            if facts.contains_key(&p) {
                sessions.entry(p).or_default().push(s);
            }
        }
        // Each session's repo edits (path, ts), its subagents' included.
        let mut edits: HashMap<i64, Vec<(String, i64)>> = HashMap::new();
        let mut stmt = conn
            .prepare(
                "SELECT e.thread_id, e.path, COALESCE(e.ts, 0), t.is_subagent FROM file_edits e
                 JOIN threads t ON t.id = e.thread_id
                 WHERE e.op IN ('edit', 'write')",
            )
            .unwrap();
        for row in stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)? != 0,
                ))
            })
            .unwrap()
        {
            let (t, p, ts, subagent) = row.unwrap();
            let session = match (subagent, parents.get(&t)) {
                (false, _) => t,
                (true, Some(parent)) => *parent,
                (true, None) => continue,
            };
            if let Some(p) = repo_path(&p) {
                edits.entry(session).or_default().push((p, ts));
            }
        }

        // What a hook would inject for one kind: pinned, then `promoted`, then by date.
        fn injected<'a>(by_date: &[&'a Fact], promoted: &[&'a Fact], cap: usize) -> Vec<&'a Fact> {
            let mut out: Vec<&Fact> = by_date.iter().copied().filter(|f| f.pinned).collect();
            for f in promoted.iter().chain(by_date) {
                if out.len() >= cap {
                    break;
                }
                if !out.iter().any(|o| o.id == f.id) {
                    out.push(*f);
                }
            }
            out.truncate(cap.max(out.iter().filter(|f| f.pinned).count()));
            out
        }
        const KS: [usize; 2] = [WORKING_SET_MAX_FACTS, 8];
        // Per project: eligible sessions, hits by date, hits with the working set per K,
        // sessions whose working set holds a file they went on to edit.
        // "Prior" three ways: the fact existed at the session's start (created before it); its
        // own session started before it (as when the distiller keeps up; a backlog distilled
        // in one go has every fact created after most sessions); or that, except that a fact
        // of one of the session's working-set sessions counts only once it was written. Those
        // sessions just ended (or still run), so at the next SessionStart they were usually not
        // distilled yet: the second way credits the working set with facts it could not have
        // seen, the third does not.
        const MODES: [&str; 3] = [
            "created before",
            "its session started before",
            "its session started before; a working-set session's once written",
        ];
        for (mode, label) in MODES.iter().enumerate() {
            let mut per: Vec<(usize, usize, [usize; 2], usize)> = Vec::new();
            for (p, list) in &sessions {
                let pf = &facts[p];
                let root = checkout_roots(p, p);
                let mut row = (0usize, 0usize, [0usize; 2], 0usize);
                for s in list {
                    let Some(mine) = edits.get(&s.0) else {
                        continue;
                    };
                    let mut edited: Vec<String> = mine.iter().map(|e| e.0.clone()).collect();
                    edited.sort();
                    edited.dedup();
                    // The working set at this session's start: among the NEWEST_SESSIONS
                    // top-level sessions of any project then (updates capped at it).
                    let mut then: Vec<i64> = everyone
                        .iter()
                        .filter(|e| e.0 < s.1)
                        .map(|e| e.1.min(s.1))
                        .collect();
                    let floor = if then.len() > NEWEST_SESSIONS {
                        let at = then.len() - NEWEST_SESSIONS;
                        *then.select_nth_unstable(at).1
                    } else {
                        i64::MIN
                    };
                    let mut others: Vec<(i64, i64, bool)> = list
                        .iter()
                        .filter(|o| o.0 != s.0 && o.1 < s.1 && o.2.min(s.1) >= floor)
                        .map(|o| (o.0, o.2.min(s.1), o.3.is_some() && o.3 == s.3))
                        .collect();
                    others.sort_by_key(|o| std::cmp::Reverse(o.1));
                    others.truncate(RECENT_THREADS);
                    let before = |t: i64| -> Vec<(String, i64)> {
                        edits
                            .get(&t)
                            .map(|v| v.iter().filter(|e| e.1 > 0 && e.1 < s.1).cloned().collect())
                            .unwrap_or_default()
                    };
                    others.retain(|o| !before(o.0).is_empty());
                    others.sort_by(|a, b| b.2.cmp(&a.2).then(b.1.cmp(&a.1)));
                    others.truncate(WORKING_SET_THREADS);
                    // The branch's files first, then the most recently edited, as working_set.
                    let mut recent: Vec<(bool, i64, String)> = others
                        .iter()
                        .flat_map(|o| before(o.0).into_iter().map(move |(p, ts)| (o.2, ts, p)))
                        .collect();
                    recent.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
                    let mut working: Vec<String> = Vec::new();
                    for (_, _, path) in recent {
                        if !working.contains(&path) && working.len() < WORKING_SET_MAX_PATHS {
                            working.push(path);
                        }
                    }
                    let mut prior: Vec<&Fact> = pf
                        .iter()
                        .filter(|f| {
                            let at = if mode == 0 { f.created } else { f.decided };
                            let written = mode < 2
                                || f.created < s.1
                                || !others.iter().any(|o| o.0 == f.thread);
                            at < s.1 && written && !superseded_at(&f.uid, s.1)
                        })
                        .collect();
                    if !prior.iter().any(|f| names_any(&f.tokens, &root, &edited)) {
                        continue;
                    }
                    row.0 += 1;
                    prior.sort_by(|a, b| {
                        b.pinned
                            .cmp(&a.pinned)
                            .then(b.decided.cmp(&a.decided))
                            .then(b.id.cmp(&a.id))
                    });
                    let kind = |d: bool| {
                        prior
                            .iter()
                            .copied()
                            .filter(|f| f.decision == d)
                            .collect::<Vec<&Fact>>()
                    };
                    let (dec, got) = (kind(true), kind(false));
                    let hit = |shown: &[&Fact]| {
                        shown.iter().any(|f| names_any(&f.tokens, &root, &edited))
                    };
                    if hit(&injected(&dec, &[], budget)) || hit(&injected(&got, &[], budget)) {
                        row.1 += 1;
                    }
                    if working.iter().any(|w| edited.contains(w)) {
                        row.3 += 1;
                    }
                    let candidates: Vec<&Fact> = prior
                        .iter()
                        .copied()
                        .filter(|f| !f.pinned && names_working_file(&f.text, p, &working))
                        .collect();
                    for (ki, k) in KS.iter().enumerate() {
                        let up = |d: bool| {
                            candidates
                                .iter()
                                .copied()
                                .filter(|f| f.decision == d)
                                .take(*k)
                                .collect::<Vec<&Fact>>()
                        };
                        if hit(&injected(&dec, &up(true), budget))
                            || hit(&injected(&got, &up(false), budget))
                        {
                            row.2[ki] += 1;
                        }
                    }
                }
                if row.0 > 0 {
                    per.push(row);
                }
            }
            per.sort_by_key(|r| std::cmp::Reverse(r.0));
            let line = |label: &str, r: &(usize, usize, [usize; 2], usize)| {
                eprintln!(
                "{label:<12} eligible {:>4} | by date {:>4} | working set K={} {:>4}, K={} {:>4} \
                 | working set holds an edited file {:>4}",
                r.0, r.1, KS[0], r.2[0], KS[1], r.2[1], r.3
            );
            };
            let mut all = (0usize, 0usize, [0usize; 2], 0usize);
            for r in &per {
                all.0 += r.0;
                all.1 += r.1;
                all.2[0] += r.2[0];
                all.2[1] += r.2[1];
                all.3 += r.3;
            }
            eprintln!(
                "prior = {label}; projects with eligible sessions: {}",
                per.len()
            );
            line("all", &all);
            for (i, r) in per.iter().take(5).enumerate() {
                line(&format!("project #{}", i + 1), r);
            }
        }
    }

    /// What the working set costs the hook, in process on a real index clone: for the 3
    /// projects with the most facts, each step's best of 5 (warm), and how many facts each
    /// filter lets through. Same invocation as [`real_working_set_replay`].
    #[test]
    #[ignore = "needs CALLIMACHUS_GROUNDING_DB, a clone of a real index"]
    fn real_working_set_cost() {
        use std::time::{Duration, Instant};
        let Ok(path) = std::env::var("CALLIMACHUS_GROUNDING_DB") else {
            eprintln!("set CALLIMACHUS_GROUNDING_DB to an index clone");
            return;
        };
        let conn = crate::db::open_readonly(Path::new(&path)).unwrap();
        let claude = super::super::harvest::EXTRACTOR_CLAUDE_MEMORY;
        let exclude = Some(claude);
        let mut stmt = conn
            .prepare(
                "SELECT COALESCE(t.project_key, t.project_path) k FROM facts f
                 JOIN threads t ON t.id = f.thread_id
                 WHERE f.kind IN ('decision', 'gotcha') AND f.hidden = 0 AND f.extractor != ?1
                   AND k IS NOT NULL
                 GROUP BY k ORDER BY COUNT(*) DESC LIMIT 3",
            )
            .unwrap();
        let projects: Vec<String> = stmt
            .query_map([claude], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        fn best<T>(mut f: impl FnMut() -> T) -> (Duration, T) {
            let mut out = None;
            let mut best = Duration::MAX;
            for _ in 0..5 {
                let t = Instant::now();
                let v = f();
                best = best.min(t.elapsed());
                out = Some(v);
            }
            (best, out.unwrap())
        }
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        for (rank, p) in projects.iter().enumerate() {
            let per_kind = MemoryBudget::HOOK.fetch_limit();
            let (d, g) = (
                MemoryBudget::HOOK.max_decisions,
                MemoryBudget::HOOK.max_gotchas,
            );
            let (base, _) = best(|| {
                get_project_memory_with(
                    &conn,
                    p,
                    &MemoryQuery {
                        per_kind,
                        exclude,
                        ..Default::default()
                    },
                )
                .unwrap()
            });
            let (ws_t, working) = best(|| working_set(&conn, p, Some("main")).unwrap());
            // A repo with no session among the machine's newest NEWEST_SESSIONS has no working
            // set, so its scan never ran in the hook's timing (the largest repo's, when the
            // gate was measured). Its next session start will run it: time that case with its
            // latest session's edited files.
            let synthetic = working.is_empty();
            let working = if synthetic {
                let mut stmt = conn
                    .prepare(&format!(
                        "SELECT e.path FROM threads t JOIN file_edits e ON e.thread_id = t.id
                         WHERE {} AND t.is_subagent = 0 AND e.op IN ('edit', 'write')
                         ORDER BY t.updated_at DESC, e.ts DESC LIMIT 500",
                        project_condition("t", 1)
                    ))
                    .unwrap();
                let mut out: Vec<String> = Vec::new();
                for path in stmt.query_map([p], |r| r.get::<_, String>(0)).unwrap() {
                    if let Some(rp) = repo_path(&path.unwrap()) {
                        if !out.contains(&rp) && out.len() < WORKING_SET_MAX_PATHS {
                            out.push(rp);
                        }
                    }
                }
                out
            } else {
                working
            };
            let (scan_t, rows) = best(|| {
                let mut stmt = conn
                    .prepare(&format!(
                        "SELECT f.text FROM facts f JOIN threads t ON t.id = f.thread_id
                         WHERE f.kind IN ('decision', 'gotcha') AND {}
                           AND f.hidden = 0 AND f.pinned = 0 AND t.is_subagent = 0
                           AND f.extractor != ?2",
                        project_condition("t", 1)
                    ))
                    .unwrap();
                stmt.query_map(params![p, claude], |r| r.get::<_, String>(0))
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<String>>>()
                    .unwrap()
            });
            let names = file_names(&working);
            let (name_t, named) = best(|| {
                rows.iter()
                    .filter(|t| mentions_name(t, &names))
                    .collect::<Vec<_>>()
            });
            let (tok_t, confirmed) = best(|| {
                named
                    .iter()
                    .filter(|t| names_working_file(t, p, &working))
                    .count()
            });
            let (all_t, _) = best(|| {
                get_project_memory_for_working_set(&conn, p, p, per_kind, d, g, exclude, &working)
                    .unwrap()
            });
            // As the hook meets it: a fresh connection, so SQLite's page cache is cold (the
            // OS's is warm), with the branch read and the working set included.
            let fresh = || crate::db::open_readonly(Path::new(&path)).unwrap();
            let (cold_base, _) = best(|| {
                let c = fresh();
                get_project_memory_with(
                    &c,
                    p,
                    &MemoryQuery {
                        per_kind,
                        exclude,
                        ..Default::default()
                    },
                )
                .unwrap()
            });
            let (cold_ws, _) = best(|| {
                let c = fresh();
                let w = working_set(&c, p, head_branch(Path::new(p)).as_deref()).unwrap();
                let w = if synthetic { working.clone() } else { w };
                get_project_memory_for_working_set(&c, p, p, per_kind, d, g, exclude, &w).unwrap()
            });
            eprintln!(
                "project #{}: fresh connection: date order {:.2} ms, with the working set {:.2} ms (+{:.2})",
                rank + 1,
                ms(cold_base),
                ms(cold_ws),
                ms(cold_ws) - ms(cold_base)
            );
            eprintln!(
                "project #{}: facts {} | working set{} {} paths in {:.2} ms | base read {:.2} ms | \
                 scan {:.2} ms, name pass {:.2} ms ({} through), token check {:.2} ms ({} name a \
                 file) | whole read with the working set {:.2} ms (+{:.2})",
                rank + 1,
                rows.len(),
                if synthetic { " (synthetic)" } else { "" },
                working.len(),
                ms(ws_t),
                ms(base),
                ms(scan_t),
                ms(name_t),
                named.len(),
                ms(tok_t),
                confirmed,
                ms(all_t),
                ms(all_t) - ms(base)
            );
        }
    }
}
