//! The Today view: what happened in the user's agent sessions since they last looked. Reads
//! only the index (plus `git log` for the commits), never an LLM. `ui.last_open` is when the
//! app last showed it; the frontend marks it only after the view has loaded, so the window
//! the user is reading is not the one just marked.

use crate::knowledge::{app_config, truth, FactStatus};
use crate::scope::ProjectScope;
use crate::{db, gitlink, issues, snapshot};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// app_config: unix secs of the last time the Today view loaded.
pub const LAST_OPEN_KEY: &str = "ui.last_open";
/// app_config: the project key the Project page and the search scope last used.
pub const LAST_PROJECT_KEY: &str = "ui.last_project";

/// The window when the app has never been opened: the last day.
const DEFAULT_WINDOW_SECS: i64 = 86_400;
/// Threads listed at most: a first open after weeks away should not list thousands
/// (`Today::truncated` says when it cut the list).
const MAX_THREADS: i64 = 200;
const MAX_TODOS: i64 = 50;
const MAX_ISSUES: usize = 10;
const MAX_SNAPSHOTS: usize = 20;
/// How far before `since` an error must have been seen to count as "seen again". 180 days
/// (what the Project page mines) cost about 0.7 s warm and 4.7 s cold on a 21k-thread
/// index; a month takes about 140 ms warm there and still catches the errors that keep
/// coming back.
const ISSUE_LOOKBACK_SECS: i64 = 30 * 86_400;
/// The oldest window start the issue mining honors: a first open months after the last one
/// would otherwise mine months of messages before the view can paint. An older `since`
/// mines the last [`ISSUE_WINDOW_FLOOR_SECS`] plus the lookback, so at most two months.
const ISSUE_WINDOW_FLOOR_SECS: i64 = 30 * 86_400;
/// Total time `git log` may take across every repo in the view; the rest are skipped and
/// the response says the commits are partial.
pub const COMMIT_BUDGET: Duration = Duration::from_secs(2);

/// Everything the Today view shows.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Today {
    /// The start of the window (unix secs), echoed so the header can say "Since <time>".
    pub since: i64,
    /// One group per project key, the most recently active first.
    pub groups: Vec<TodayGroup>,
    pub new_todos: Vec<NewTodo>,
    pub issues_seen_again: Vec<IssueSeenAgain>,
    pub snapshots: Vec<snapshot::AppSnapshot>,
    /// Memory to review: machine flags and disputes still open, and what agents recorded or
    /// replaced since `since` ([`memory_review`]). Empty on an index without 0035's tables.
    pub memory_review: Vec<ReviewItem>,
    /// How many more items there are to review than `memory_review` lists (it stops at
    /// [`MAX_REVIEW`]); 0 when it holds them all.
    pub memory_review_more: usize,
    /// True when the git budget ran out (or the index stayed locked) before every repo was
    /// linked.
    pub commits_partial: bool,
    /// True when more sessions were active since `since` than the view lists (the newest
    /// [`MAX_THREADS`] are kept).
    pub truncated: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TodayGroup {
    /// The canonical project key; "" for sessions with no project (Cursor, in-app chats).
    pub project: String,
    pub threads: Vec<TodayThread>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TodayThread {
    pub id: i64,
    pub title: Option<String>,
    pub source: String,
    pub last_active: i64,
    /// First to last message timestamp.
    pub duration_secs: i64,
    pub messages: i64,
    pub commits: Vec<TodayCommit>,
    /// Where the session ran (a worktree's own path, not the group's main repo): git runs
    /// there, so commits on an unmerged worktree branch are found.
    #[serde(skip)]
    pub project_path: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TodayCommit {
    /// The full sha, which is what a copy should put on the clipboard.
    pub sha: String,
    pub short_sha: String,
    pub subject: Option<String>,
    pub committed_at: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewTodo {
    pub id: i64,
    pub text: String,
    pub project: String,
    pub thread_id: i64,
}

/// Why a fact is on the memory review list ([`memory_review`]).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum ReviewReason {
    /// An agent or a memory pass flagged it stale or wrong: it is out of injected memory
    /// until the user keeps or hides it, unless pinned or edited (`trusted`). A fact memory
    /// never injects (`injectable` false) is not shown to agents either way.
    Flagged,
    /// Another fact contradicts it and nothing settled the pair: both stay injected, marked.
    Disputed,
    /// An agent's fact replaced it since `since`: it left injected memory with no review.
    Replaced,
    /// An agent recorded it since `since`.
    Recorded,
}

/// The fact on the other side of a review item.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReviewOther {
    pub id: i64,
    pub text: String,
}

/// One fact for the user to look at: something an agent (or a memory pass) did to memory that
/// nobody reviewed.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReviewItem {
    pub reason: ReviewReason,
    /// The fact the item is about (for `replaced`, the one that was replaced).
    pub fact_id: i64,
    /// `decision` or `gotcha`.
    pub kind: String,
    pub text: String,
    /// The fact's project key ("" when it has none).
    pub project: String,
    /// When the reason arose (unix secs): the flag, the link, or the fact's recording.
    pub at: i64,
    /// Who did it: `agent` or `distill` (a memory pass) for a flag, the link's source for a
    /// dispute (`distill`, `agent`, `repo`, `review`, `user`), `agent` otherwise.
    pub source: String,
    /// What replaced it, or what disputes it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub other: Option<ReviewOther>,
    /// A flag's kind: `stale` or `wrong`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flag: Option<String>,
    /// A flag's reason, as the agent gave it (redacted when it was stored).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The user pinned or edited it, so a flag leaves it in memory (when `injectable`).
    pub trusted: bool,
    /// Injected memory can carry it: it is pinned, or neither a failure playbook nor a
    /// subagent's fact (those stay out unless pinned, whether edited or not). False: a flag
    /// on it, or keeping one, changes nothing an agent is shown.
    pub injectable: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueSeenAgain {
    /// A representative error line (the cluster's latest raw sighting), what the Project
    /// page's recurring errors list shows.
    pub signature: String,
    pub count: i64,
    pub last_seen: i64,
    /// The project of the latest sighting ("" when that thread has none).
    pub project: String,
}

/// The window start: `since` when given, else the stored last open, else a day ago.
pub fn resolve_since(conn: &Connection, since: Option<i64>, now: i64) -> Result<i64> {
    if let Some(s) = since {
        return Ok(s);
    }
    Ok(app_config::get(conn, LAST_OPEN_KEY)?
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(now - DEFAULT_WINDOW_SECS))
}

/// Store `now` as the last time the Today view loaded.
pub fn mark_opened(conn: &Connection, now: i64) -> Result<()> {
    app_config::put(conn, LAST_OPEN_KEY, &now.to_string())
}

/// The project the app last scoped to, if any.
pub fn last_project(conn: &Connection) -> Result<Option<String>> {
    app_config::get_nonblank(conn, LAST_PROJECT_KEY)
}

/// Remember (or, with None or "", forget) the project the app last scoped to.
pub fn set_last_project(conn: &Connection, project: Option<&str>) -> Result<()> {
    match project.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => app_config::put(conn, LAST_PROJECT_KEY, p),
        None => app_config::delete(conn, LAST_PROJECT_KEY),
    }
}

/// Top-level sessions active since ?1, newest first, straight off the (is_subagent,
/// updated_at DESC) index with no sort. `message_count > 0` drops the synthetic "Recorded
/// memory" notes threads, which only hold facts. ?2 is one more than the view lists, so the
/// extra row says the list was cut.
pub(crate) const TODAY_THREADS_SQL: &str =
    "SELECT t.id, t.title, s.kind, t.updated_at, t.message_count,
            COALESCE(t.project_key, t.project_path, ''), t.project_path
     FROM threads t JOIN sources s ON s.id = t.source_id
     WHERE t.is_subagent = 0 AND t.updated_at >= ?1 AND t.message_count > 0
     ORDER BY t.updated_at DESC LIMIT ?2";

/// Open heuristic and follow-up TODOs found since ?1 (`created_at` is the message time, see
/// `todo_created_at`, so a reindex does not make old TODOs look new). SQLite picks the kind
/// index over the partial open-TODO one and sorts what it finds: about 1k TODO rows on a
/// 21k-thread index, under a millisecond. In-app chats are skipped: a saved Ask answer that
/// quotes a past "TODO: ..." would otherwise show as a new TODO duplicating the original.
pub(crate) const TODAY_TODOS_SQL: &str =
    "SELECT f.id, f.text, COALESCE(t.project_key, t.project_path, ''), f.thread_id
     FROM facts f JOIN threads t ON t.id = f.thread_id JOIN sources s ON s.id = t.source_id
     WHERE f.kind = 'todo' AND f.status = 'open' AND f.extractor IN ('heuristic', 'followup')
       AND f.hidden = 0 AND f.created_at >= ?1 AND t.is_subagent = 0 AND s.kind != 'in_app'
     ORDER BY f.created_at DESC LIMIT ?2";

/// The Today view since `since` with the commits already linked in the index. Run
/// [`link_commits`] then [`fill_commits`] to add fresh ones.
pub fn build(conn: &Connection, since: i64, now: i64) -> Result<Today> {
    let mut groups: Vec<TodayGroup> = Vec::new();
    let truncated;
    {
        let mut stmt = conn.prepare_cached(TODAY_THREADS_SQL)?;
        let mut window = conn.prepare_cached(gitlink::THREAD_WINDOW_SQL)?;
        let mut rows = stmt
            .query_map(rusqlite::params![since, MAX_THREADS + 1], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    r.get::<_, i64>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, Option<String>>(6)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        truncated = rows.len() as i64 > MAX_THREADS;
        rows.truncate(MAX_THREADS as usize);
        for (id, title, source, last_active, messages, project, project_path) in rows {
            let (first, last): (Option<i64>, Option<i64>) =
                window.query_row([id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            let duration_secs = match (first, last) {
                (Some(a), Some(b)) => (b - a).max(0),
                _ => 0,
            };
            let thread = TodayThread {
                id,
                title,
                source,
                last_active,
                duration_secs,
                messages,
                commits: Vec::new(),
                project_path,
            };
            // Rows arrive newest first, so a group's first thread is its last activity and
            // the groups come out ordered by it.
            match groups.iter_mut().find(|g| g.project == project) {
                Some(g) => g.threads.push(thread),
                None => groups.push(TodayGroup {
                    project,
                    threads: vec![thread],
                }),
            }
        }
    }

    let new_todos = conn
        .prepare_cached(TODAY_TODOS_SQL)?
        .query_map(rusqlite::params![since, MAX_TODOS], |r| {
            Ok(NewTodo {
                id: r.get(0)?,
                text: r.get(1)?,
                project: r.get(2)?,
                thread_id: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    // No further back than a kept item is remembered ([`REVIEW_KEPT_SECS`]): "since the last
    // open" after months away would otherwise list again what was kept that long ago.
    let review = memory_review(conn, &ProjectScope::Any, since.max(now - REVIEW_KEPT_SECS))?;
    let mut today = Today {
        since,
        groups,
        new_todos,
        issues_seen_again: issues_seen_again(conn, since, now)?,
        snapshots: snapshot::list_for_app(conn, None, Some(since), MAX_SNAPSHOTS)?,
        memory_review: review.items,
        memory_review_more: review.more,
        commits_partial: false,
        truncated,
    };
    fill_commits(conn, &mut today)?;
    Ok(today)
}

/// Recurring error clusters seen since `since` that were also seen before it. A `since`
/// older than [`ISSUE_WINDOW_FLOOR_SECS`] counts from that floor instead.
fn issues_seen_again(conn: &Connection, since: i64, now: i64) -> Result<Vec<IssueSeenAgain>> {
    let since = issue_since(since, now);
    let lookback = since - ISSUE_LOOKBACK_SECS;
    // Live, not `recurring_issues_cached`: its key moves only with the thread set, so an
    // error hit again in a session that is still running (messages appended to a thread)
    // stayed hidden for up to 10 minutes. About 150 ms warm on a 21k-thread index, from the
    // top-level sessions Today lists: their subagents' output is several times the text.
    let clusters = issues::recurring_issues_from(
        conn,
        None,
        crate::search::ProjectMatch::Exact,
        lookback,
        500,
        issues::Sightings::TopLevel,
    )?;
    let mut project_of = conn.prepare_cached(
        "SELECT COALESCE(project_key, project_path, '') FROM threads WHERE id = ?1",
    )?;
    let mut out = Vec::new();
    for c in clusters
        .into_iter()
        .filter(|c| c.last_seen >= since && c.first_seen < since)
        .filter(issues::names_the_error)
        .take(MAX_ISSUES)
    {
        let project = project_of
            .query_row([c.last_thread_id], |r| r.get::<_, String>(0))
            .optional()?
            .unwrap_or_default();
        out.push(IssueSeenAgain {
            signature: c.example,
            count: c.count,
            last_seen: c.last_seen,
            project,
        });
    }
    Ok(out)
}

/// Most items [`memory_review`] lists. Agent flags are capped per project
/// (`AGENT_FLAG_CAP`) and agent replacements per day (`AGENT_SUPERSEDE_CAP`), but a memory
/// pass's retirements and disputes are not.
pub const MAX_REVIEW: usize = 50;

/// What [`memory_review`] lists, and how many items its cap left out.
#[derive(Debug, Default)]
pub struct MemoryReview {
    /// At most [`MAX_REVIEW`] items.
    pub items: Vec<ReviewItem>,
    /// How many more items there are than `items` holds (0 when it holds them all).
    pub more: usize,
}

/// What agents and memory passes did to project memory that the user has not reviewed, gated
/// on `truth::has_truth_tables` (empty before 0035): unresolved `stale` / `wrong` flags an
/// agent or a memory pass raised, active `contradicts` links whose two facts are both still in
/// play, and the decisions and gotchas agents replaced (`supersedes` from `record_*`
/// `replaces`) or recorded since `since` and the user has not kept ([`keep_review_item`]).
/// Only visible facts, in `scope`; a fact is listed
/// once, under the first of those reasons that applies (a recorded fact that replaced another
/// is the `other` side of that item, not an item of its own). Listed replaced and recorded
/// first, then disputed, then flagged, each newest first, at most [`MAX_REVIEW`] (`more`
/// counts the rest). The tables are small
/// (links and signals are declared, never inferred): every read is a scan of them or an
/// indexed facts lookup, and a fact's standing comes from `truth::fact_truth`, the same rule
/// the Memory section uses.
pub fn memory_review(conn: &Connection, scope: &ProjectScope, since: i64) -> Result<MemoryReview> {
    if !truth::has_truth_tables(conn) {
        return Ok(MemoryReview::default());
    }
    // (reason, the item's fact uid, the other fact's uid, when, who) in priority order.
    let mut candidates: Vec<(ReviewReason, String, Option<String>, i64, String)> = Vec::new();
    {
        let mut flags = conn.prepare_cached(
            "SELECT fact_uid, created_at, source FROM fact_signals
             WHERE kind IN ('stale', 'wrong') AND resolved_at IS NULL
               AND source IN ('agent', 'distill')
             ORDER BY created_at DESC, id DESC",
        )?;
        for r in flags.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))? {
            let (uid, at, source) = r?;
            candidates.push((ReviewReason::Flagged, uid, None, at, source));
        }
        let mut disputes = conn.prepare_cached(
            "SELECT to_uid, from_uid, created_at, source FROM fact_links
             WHERE kind = 'contradicts' AND dismissed_at IS NULL
             ORDER BY created_at DESC, id DESC",
        )?;
        for r in disputes.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))? {
            let (to, from, at, source) = r?;
            candidates.push((ReviewReason::Disputed, to, Some(from), at, source));
        }
        let mut replaced = conn.prepare_cached(
            "SELECT to_uid, from_uid, created_at FROM fact_links
             WHERE kind = 'supersedes' AND source = 'agent' AND dismissed_at IS NULL
               AND created_at >= ?1
             ORDER BY created_at DESC, id DESC",
        )?;
        for r in replaced.query_map([since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))? {
            let (to, from, at) = r?;
            candidates.push((ReviewReason::Replaced, to, Some(from), at, "agent".into()));
        }
        let mut recorded = conn.prepare_cached(RECORDED_SQL)?;
        for r in recorded.query_map([since], |r| Ok((r.get(0)?, r.get(1)?)))? {
            let (uid, at) = r?;
            candidates.push((ReviewReason::Recorded, uid, None, at, "agent".into()));
        }
    }
    if candidates.is_empty() {
        return Ok(MemoryReview::default());
    }

    let mut uids: Vec<&str> = Vec::new();
    for (_, uid, other, ..) in &candidates {
        uids.push(uid);
        uids.extend(other.as_deref());
    }
    uids.sort_unstable();
    uids.dedup();
    let truths = truth::fact_truth(conn, &uids)?;
    let rows = review_rows(conn, &uids)?;
    let kept = kept_review(conn)?;

    let mut listed: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for (reason, uid, other, at, source) in candidates {
        let (Some(row), Some(t)) = (rows.get(&uid), truths.get(&uid)) else {
            continue;
        };
        if listed.contains(&uid) || !scope.matches(&row.project) {
            continue;
        }
        let other_row = other.as_deref().and_then(|o| rows.get(o));
        let mut item = ReviewItem {
            reason,
            fact_id: row.id,
            kind: row.kind.clone(),
            text: row.text.clone(),
            project: row.project.clone(),
            at,
            source,
            other: None,
            flag: None,
            note: None,
            trusted: row.trusted,
            injectable: row.injectable,
        };
        let superseded = t.status == FactStatus::Superseded;
        match reason {
            ReviewReason::Flagged => {
                // The newest machine flag: one the user raised needs no review.
                let Some(flag) = t
                    .flags
                    .iter()
                    .find(|f| f.source == "agent" || f.source == "distill")
                else {
                    continue;
                };
                if superseded {
                    continue;
                }
                item.flag = Some(flag.kind.clone());
                item.note = flag.reason.clone();
            }
            ReviewReason::Disputed => {
                // Still in play on both sides: `disputed_by` holds only those.
                let Some(o) = other_row.filter(|o| t.disputed_by.iter().any(|d| d.id == o.id))
                else {
                    continue;
                };
                if superseded {
                    continue;
                }
                // One item per pair, whichever way round it was linked.
                listed.insert(o.uid.clone());
                item.other = Some(o.shown());
            }
            ReviewReason::Replaced => {
                // Still replaced by that fact: the link is active and its newer side stands.
                let Some(o) = other_row else { continue };
                let replacing = truths
                    .get(&o.uid)
                    .is_some_and(|ot| ot.replaces.iter().any(|r| r.id == row.id));
                if !superseded || !replacing {
                    continue;
                }
                listed.insert(o.uid.clone());
                item.other = Some(o.shown());
            }
            ReviewReason::Recorded => {
                if superseded {
                    continue;
                }
            }
        }
        // Kept on Today ([`keep_review_item`]): still counted as listed, so the replacer of
        // a kept replacement does not come back as a recorded fact of its own.
        let key = kept_key(reason, &uid, other.as_deref());
        listed.insert(uid);
        if kept.contains_key(&key) {
            continue;
        }
        out.push(item);
    }
    // What entered or left memory with no review comes first: open flags have no window
    // and a memory pass may retire 3 facts per session, so with flags first 50 of them kept
    // every agent replacement and recording off the list. Each reason was picked above in
    // the old order, so a flagged fact an agent also recorded is still settled as a flag.
    // The sort is stable: each group stays newest first.
    out.sort_by_key(|i| match i.reason {
        ReviewReason::Replaced => 0,
        ReviewReason::Recorded => 1,
        ReviewReason::Disputed => 2,
        ReviewReason::Flagged => 3,
    });
    let more = out.len().saturating_sub(MAX_REVIEW);
    out.truncate(MAX_REVIEW);
    Ok(MemoryReview { items: out, more })
}

/// app_config: the recorded and replaced review items the user kept on Today, as a JSON
/// object from the item's [`kept_key`] to when it was kept (unix secs). A flag or a dispute
/// needs no such record (keeping one resolves the flag or dismisses the link), but a recorded
/// or replaced fact has nothing to settle: without it a kept item stayed on `cal memory
/// review` for its 7 days and came back on Today after a restart. An entry is a few dozen
/// bytes and lives [`REVIEW_KEPT_SECS`] ([`keep_review_item`] drops the ones no window can
/// list), so it needs no table.
pub const REVIEW_KEPT_KEY: &str = "memory.review_kept";

/// The longest fixed window that lists recorded and replaced facts: Today's 7d range and
/// `cal memory review`.
pub const REVIEW_RECENT_SECS: i64 = 7 * 86_400;

/// How long a kept entry is remembered, and so the furthest back Today lists recorded and
/// replaced facts ([`build`] clamps "since the last open" to it). Not the stored last open:
/// the app moves that to the current load while the window on screen still starts at the
/// previous one, so pruning by it dropped entries that window listed, and they came back.
/// 90 days is far past [`REVIEW_RECENT_SECS`]; a longer absence lists its last 90 days.
pub const REVIEW_KEPT_SECS: i64 = 90 * 86_400;

/// A review item's identity across reloads: its reason, its fact's uid and the other fact's
/// (uids, not row ids, survive a re-distill that keeps the text).
fn kept_key(reason: ReviewReason, uid: &str, other: Option<&str>) -> String {
    let reason = match reason {
        ReviewReason::Flagged => "flagged",
        ReviewReason::Disputed => "disputed",
        ReviewReason::Replaced => "replaced",
        ReviewReason::Recorded => "recorded",
    };
    format!("{reason}:{uid}:{}", other.unwrap_or(""))
}

/// The kept items by [`kept_key`]; a missing or unreadable value keeps nothing.
fn kept_review(conn: &Connection) -> Result<HashMap<String, i64>> {
    Ok(app_config::get(conn, REVIEW_KEPT_KEY)?
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default())
}

/// Keep a recorded or replaced review item (Today's Keep): it leaves Today and `cal memory
/// review` for good. `other_id` is the fact that replaced a `replaced` item's fact. A flag or
/// a dispute is kept by settling it (`resolve_fact_flags`, `unlink_facts`), so either is an
/// error here. Drops the entries no listing can show any more: an item kept more than
/// [`REVIEW_KEPT_SECS`] ago is older than every window, since its fact or link existed when
/// it was kept.
pub fn keep_review_item(
    conn: &Connection,
    reason: ReviewReason,
    fact_id: i64,
    other_id: Option<i64>,
    now: i64,
) -> Result<()> {
    let uid = |id: i64| -> Result<String> {
        conn.query_row(
            "SELECT uid FROM facts WHERE id = ?1 AND uid IS NOT NULL",
            [id],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| anyhow::anyhow!("fact {id} not found"))
    };
    let other = match (reason, other_id) {
        (ReviewReason::Recorded, _) => None,
        (ReviewReason::Replaced, Some(o)) => Some(uid(o)?),
        (ReviewReason::Replaced, None) => {
            anyhow::bail!("a replaced item names the fact that replaced it")
        }
        (ReviewReason::Flagged | ReviewReason::Disputed, _) => {
            anyhow::bail!("a flag or a dispute is kept by settling it, not by recording a Keep")
        }
    };
    let key = kept_key(reason, &uid(fact_id)?, other.as_deref());
    let floor = now - REVIEW_KEPT_SECS;
    let mut kept = kept_review(conn)?;
    kept.retain(|_, at| *at >= floor);
    kept.insert(key, now);
    app_config::put(conn, REVIEW_KEPT_KEY, &serde_json::to_string(&kept)?)
}

/// The decisions and gotchas agents recorded since ?1, newest first, off idx_facts_kind
/// rather than a scan of every fact (TODOs and summaries are most of the table).
pub(crate) const RECORDED_SQL: &str = "SELECT uid, created_at FROM facts
     WHERE kind IN ('decision', 'gotcha') AND extractor = 'agent' AND created_at >= ?1
       AND hidden = 0 AND uid IS NOT NULL
     ORDER BY created_at DESC, id DESC";

/// One fact as the review list shows it.
struct ReviewRow {
    id: i64,
    uid: String,
    kind: String,
    text: String,
    project: String,
    trusted: bool,
    injectable: bool,
}

impl ReviewRow {
    fn shown(&self) -> ReviewOther {
        ReviewOther {
            id: self.id,
            text: self.text.clone(),
        }
    }
}

/// The visible decisions and gotchas among `uids`, by uid, in one query. Injectable as
/// project memory reads it (`knowledge::MemoryQuery`): an unpinned playbook or subagent fact
/// is never injected, so a flag on one keeps nothing from an agent.
fn review_rows(conn: &Connection, uids: &[&str]) -> Result<HashMap<String, ReviewRow>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT f.id, f.uid, f.kind, f.text, COALESCE(t.project_key, t.project_path, ''),
                f.pinned OR f.edited,
                f.pinned = 1 OR (f.extractor != '{}' AND t.is_subagent = 0)
         FROM json_each(?1) j JOIN facts f ON f.uid = j.value
         JOIN threads t ON t.id = f.thread_id
         WHERE f.hidden = 0 AND f.kind IN ('decision', 'gotcha')",
        crate::knowledge::EXTRACTOR_PLAYBOOK
    ))?;
    let rows = stmt.query_map([serde_json::to_string(uids)?], |r| {
        Ok(ReviewRow {
            id: r.get(0)?,
            uid: r.get(1)?,
            kind: r.get(2)?,
            text: r.get(3)?,
            project: r.get(4)?,
            trusted: r.get(5)?,
            injectable: r.get(6)?,
        })
    })?;
    let mut out = HashMap::new();
    for row in rows {
        let row = row?;
        out.insert(row.uid.clone(), row);
    }
    Ok(out)
}

/// The window start [`issues_seen_again`] uses: `since`, but no earlier than
/// [`ISSUE_WINDOW_FLOOR_SECS`] ago, so it mines at most two months (180 days of messages took
/// 4.7 s cold on a 21k-thread index).
fn issue_since(since: i64, now: i64) -> i64 {
    since.min(now).max(now - ISSUE_WINDOW_FLOOR_SECS)
}

/// `path` when it is a git checkout (a `.git` dir, or the file a linked worktree has).
pub(crate) fn git_checkout(path: &str) -> Option<&str> {
    (!path.is_empty() && std::path::Path::new(path).join(".git").exists()).then_some(path)
}

/// The checkouts a group's sessions ran in, each with its threads, in the group's order
/// (the most recently active first). A session's own `project_path` is where git must run:
/// the group key is the main repo, and plain `git log` there never walks a worktree's
/// unmerged branch. A thread with no path of its own falls back to the group key.
fn checkouts(group: &TodayGroup) -> Vec<(&str, Vec<i64>)> {
    let mut out: Vec<(&str, Vec<i64>)> = Vec::new();
    for t in &group.threads {
        let path = t
            .project_path
            .as_deref()
            .filter(|p| !p.is_empty())
            .unwrap_or(group.project.as_str());
        let Some(repo) = git_checkout(path) else {
            continue;
        };
        match out.iter_mut().find(|(p, _)| *p == repo) {
            Some((_, ids)) => ids.push(t.id),
            None => out.push((repo, vec![t.id])),
        }
    }
    out
}

/// Whether any session in `today` ran in a git checkout, i.e. whether [`link_commits`] has
/// work.
pub fn has_git_repos(today: &Today) -> bool {
    today.groups.iter().any(|g| !checkouts(g).is_empty())
}

/// Link fresh commits for the checkouts in `today` (only those with a `.git`, the most
/// recently active first) through `gitlink::link_threads` on `conn`, a writable connection,
/// within `budget` in total. Returns whether any checkout was skipped, for time or because
/// the index stayed locked past the budget.
pub fn link_commits(conn: &Connection, today: &Today, budget: Duration) -> Result<bool> {
    link_commits_with(conn, today, budget, gitlink::run_git_log_until)
}

/// [`link_commits`] with the `git log` runner injected (tests pass a fake log).
pub(crate) fn link_commits_with(
    conn: &Connection,
    today: &Today,
    budget: Duration,
    git_log: impl FnMut(&str, i64, Instant) -> Result<Option<String>>,
) -> Result<bool> {
    let all: Vec<(&str, Vec<i64>)> = today.groups.iter().flat_map(checkouts).collect();
    Ok(link_checkouts_with(conn, &all, budget, git_log)?.1 != LinkStop::Done)
}

/// Why a [`link_checkouts_with`] pass stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkStop {
    /// Every checkout was linked.
    Done,
    /// The budget ran out before the next checkout started; it was not tried.
    OutOfTime,
    /// The next checkout was tried and wrote nothing: `git log` was still running at the
    /// deadline (and was killed), or the index stayed locked past the budget.
    Failed,
}

/// Link each checkout's threads through `gitlink::link_threads`, in order, within `budget`
/// in total. Shared by Today and the watcher's per-batch linking. Returns how many checkouts
/// were linked (the first ones, in order) and why the pass stopped, so a caller that queues
/// work can tell a checkout that was tried and failed from one never reached. Leaves `conn`'s
/// busy timeout at whatever the budget had left: a caller with a long-lived connection
/// restores its own.
pub(crate) fn link_checkouts_with(
    conn: &Connection,
    checkouts: &[(&str, Vec<i64>)],
    budget: Duration,
    mut git_log: impl FnMut(&str, i64, Instant) -> Result<Option<String>>,
) -> Result<(usize, LinkStop)> {
    let deadline = Instant::now() + budget;
    let mut linked = 0;
    for (repo, ids) in checkouts {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok((linked, LinkStop::OutOfTime));
        }
        // The link's write waits for the lock no longer than the budget left: while the
        // watcher or a reindex holds it, the default 15 s wait per checkout hung the view
        // exactly when sessions were active.
        conn.busy_timeout(left)?;
        match gitlink::link_threads_with(conn, repo, ids, deadline, &mut git_log) {
            Ok(Some(_)) => linked += 1,
            Ok(None) => return Ok((linked, LinkStop::Failed)),
            Err(e) if db::is_busy(&e) => return Ok((linked, LinkStop::Failed)),
            // A checkout git can't read (moved, corrupt, git missing) just shows no
            // commits.
            Err(_) => linked += 1,
        }
    }
    Ok((linked, LinkStop::Done))
}

/// Fill each thread's commits from the stored links, those committed since the window
/// started, newest first.
pub fn fill_commits(conn: &Connection, today: &mut Today) -> Result<()> {
    let mut stmt = conn.prepare_cached(
        "SELECT sha, short_sha, subject, committed_at FROM thread_commits
         WHERE thread_id = ?1 AND committed_at >= ?2
         ORDER BY committed_at DESC",
    )?;
    let since = today.since;
    for thread in today.groups.iter_mut().flat_map(|g| g.threads.iter_mut()) {
        thread.commits = stmt
            .query_map(rusqlite::params![thread.id, since], |r| {
                Ok(TodayCommit {
                    sha: r.get(0)?,
                    short_sha: r.get(1)?,
                    subject: r.get(2)?,
                    committed_at: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::digest_test_support as dts;

    fn plan(conn: &Connection, sql: &str, args: impl rusqlite::Params) -> String {
        conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map(args, |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join(" | ")
    }

    #[test]
    fn today_queries_use_their_indexes() {
        let conn = dts::temp_db("today_plan");
        let p = plan(&conn, TODAY_THREADS_SQL, rusqlite::params![0, 200]);
        assert!(
            p.contains("SEARCH t USING INDEX idx_threads_subagent_updated"),
            "{p}"
        );
        assert!(!p.contains("TEMP B-TREE"), "{p}");
        // Off an index on facts, never a scan of every fact.
        let p = plan(&conn, TODAY_TODOS_SQL, rusqlite::params![0, 50]);
        assert!(p.contains("SEARCH f USING INDEX idx_facts_"), "{p}");
        let p = plan(&conn, RECORDED_SQL, [0]);
        assert!(p.contains("SEARCH facts USING INDEX idx_facts_kind"), "{p}");
        let p = plan(&conn, gitlink::THREAD_WINDOW_SQL, [1]);
        assert!(p.contains("COVERING INDEX idx_messages_thread_ts"), "{p}");
    }

    /// A decision or gotcha with a uid on `tid`, created at `at`.
    fn review_fact(conn: &Connection, tid: i64, text: &str, at: i64) -> i64 {
        conn.execute(
            "INSERT INTO facts (thread_id, kind, text, extractor, created_at, uid)
             VALUES (?1, 'decision', ?2, 'llm', ?3, ?4)",
            rusqlite::params![tid, text, at, crate::knowledge::new_fact_uid()],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    /// What needs the user's eye: machine flags and open disputes whenever they were raised,
    /// agent replacements and recordings since the window started; never what the user did
    /// themself, settled, hid or undid, nor another project's facts when scoped.
    #[test]
    fn memory_review_lists_what_agents_did_and_nobody_reviewed() {
        use crate::knowledge::{
            flag_fact, link_facts, record_fact_with_origin, resolve_fact_flags, restore_fact,
            set_fact_hidden, FactOrigin, FlagKind, LinkKind, LinkSource, SignalSource,
        };
        let mut conn = dts::temp_db("today_review");
        let now = chrono::Utc::now().timestamp();
        let since = now - 3_600;
        let old = now - 30 * 86_400;
        let tid = dts::session(&mut conn, "r.jsonl", "/r/app", "s", 90_000, &["hi"]);
        let web = dts::session(&mut conn, "w.jsonl", "/r/web", "w", 90_000, &["hi"]);
        let f = |text: &str| review_fact(&conn, tid, text, old);
        let (agent_flag, distill_flag, user_flag, kept, hidden) = (
            f("agent flagged"),
            f("memory pass flagged"),
            f("user flagged"),
            f("flag resolved"),
            f("flagged then hidden"),
        );
        let flag = |id, source| {
            flag_fact(
                &conn,
                id,
                FlagKind::Stale,
                source,
                Some("no longer holds"),
                None,
                old,
            )
            .unwrap()
        };
        flag(agent_flag, SignalSource::Agent);
        flag(distill_flag, SignalSource::Distill);
        flag(user_flag, SignalSource::User);
        flag(kept, SignalSource::Agent);
        resolve_fact_flags(&conn, kept, now).unwrap();
        flag(hidden, SignalSource::Agent);
        set_fact_hidden(&conn, hidden, true).unwrap();
        // A dispute in play, and one whose other side the user hid.
        let (older, newer, lone, gone) = (
            f("disputed older"),
            f("disputed newer"),
            f("disputed lone"),
            f("disputed gone"),
        );
        let dispute = |from, to| {
            link_facts(
                &conn,
                from,
                to,
                LinkKind::Contradicts,
                LinkSource::Distill,
                None,
                old,
            )
            .unwrap()
        };
        dispute(newer, older);
        dispute(gone, lone);
        set_fact_hidden(&conn, gone, true).unwrap();
        // Agents replace and record: in the window, before it, and undone.
        let (replaced, undone, before) = (
            f("agent replaced"),
            f("agent replaced, undone"),
            f("replaced long ago"),
        );
        let record = |text: &str, replaces: Option<i64>, at: i64| {
            record_fact_with_origin(
                &conn,
                "/r/app",
                "decision",
                text,
                None,
                FactOrigin::Agent,
                replaces,
                at,
            )
            .unwrap()
            .id
        };
        let replacer = record("the replacement", Some(replaced), now - 60);
        let undone_by = record("the undone replacement", Some(undone), now - 50);
        restore_fact(&conn, undone, now - 40).unwrap();
        record("an old replacement", Some(before), old + 60);
        let recorded = record("agent recorded", None, now - 30);
        record("agent recorded long ago", None, old);
        let other_project = review_fact(&conn, web, "web flagged", old);
        flag(other_project, SignalSource::Agent);

        let review = memory_review(&conn, &ProjectScope::Any, since).unwrap();
        assert_eq!(review.more, 0);
        let items = review.items;
        let got: Vec<(ReviewReason, i64)> = items.iter().map(|i| (i.reason, i.fact_id)).collect();
        assert_eq!(
            got,
            vec![
                (ReviewReason::Replaced, replaced),
                // Recorded since: the fact whose replacement the user undid stands as recorded.
                (ReviewReason::Recorded, recorded),
                (ReviewReason::Recorded, undone_by),
                (ReviewReason::Disputed, older),
                // Raised together: the newest flag first.
                (ReviewReason::Flagged, other_project),
                (ReviewReason::Flagged, distill_flag),
                (ReviewReason::Flagged, agent_flag),
            ],
            "{items:#?}"
        );
        assert!(items.iter().all(|i| i.injectable));
        let by = |id| items.iter().find(|i| i.fact_id == id).unwrap();
        assert_eq!(by(agent_flag).source, "agent");
        assert_eq!(by(agent_flag).flag.as_deref(), Some("stale"));
        assert_eq!(by(agent_flag).note.as_deref(), Some("no longer holds"));
        assert_eq!(by(distill_flag).source, "distill");
        assert_eq!(by(older).other.as_ref().map(|o| o.id), Some(newer));
        assert_eq!(by(replaced).other.as_ref().map(|o| o.id), Some(replacer));
        assert_eq!(by(replaced).project, "/r/app");

        // Scoped to one project, and settled as the user acts on it.
        let app = memory_review(&conn, &ProjectScope::Exact("/r/app".into()), since)
            .unwrap()
            .items;
        assert!(app.iter().all(|i| i.project == "/r/app"));
        assert_eq!(app.len(), 6);
        resolve_fact_flags(&conn, agent_flag, now).unwrap();
        restore_fact(&conn, replaced, now).unwrap();
        set_fact_hidden(&conn, older, true).unwrap();
        let left: Vec<i64> = memory_review(&conn, &ProjectScope::Exact("/r/app".into()), since)
            .unwrap()
            .items
            .iter()
            .map(|i| i.fact_id)
            .collect();
        // With its replacement undone, the replacer is listed as what an agent recorded.
        assert_eq!(left, vec![recorded, undone_by, replacer, distill_flag]);
        // And Today carries it, for every project.
        let t = build(&conn, since, now).unwrap();
        assert_eq!(t.memory_review.len(), 5);
    }

    /// However many flags are open, what agents replaced and recorded is listed first, and
    /// the items past the cap are counted, for Today and `cal memory review` alike.
    #[test]
    fn a_full_review_list_still_lists_what_agents_replaced_and_recorded() {
        use crate::knowledge::{
            flag_fact, record_fact_with_origin, FactOrigin, FlagKind, SignalSource,
        };
        let mut conn = dts::temp_db("today_review_cap");
        let now = chrono::Utc::now().timestamp();
        let since = now - 3_600;
        let old = now - 30 * 86_400;
        let tid = dts::session(&mut conn, "c.jsonl", "/r/app", "s", 90_000, &["hi"]);
        // Memory passes retire up to 3 facts each, and a flag stays open until settled.
        let flags = MAX_REVIEW + 10;
        for n in 0..flags {
            let id = review_fact(&conn, tid, &format!("retired fact {n}"), old);
            let at = old + n as i64;
            flag_fact(
                &conn,
                id,
                FlagKind::Stale,
                SignalSource::Distill,
                None,
                None,
                at,
            )
            .unwrap();
        }
        let replaced = review_fact(&conn, tid, "cache with bincode", old);
        let record = |text: &str, replaces: Option<i64>, at: i64| {
            record_fact_with_origin(
                &conn,
                "/r/app",
                "decision",
                text,
                None,
                FactOrigin::Agent,
                replaces,
                at,
            )
            .unwrap()
            .id
        };
        record("cache with JSON", Some(replaced), now - 60);
        let recorded = record("sign every webhook", None, now - 30);

        let review = memory_review(&conn, &ProjectScope::Any, since).unwrap();
        let head: Vec<(ReviewReason, i64)> = review.items[..2]
            .iter()
            .map(|i| (i.reason, i.fact_id))
            .collect();
        assert_eq!(
            head,
            [
                (ReviewReason::Replaced, replaced),
                (ReviewReason::Recorded, recorded)
            ]
        );
        assert_eq!(review.items.len(), MAX_REVIEW);
        assert!(review.items[2..]
            .iter()
            .all(|i| i.reason == ReviewReason::Flagged));
        // Every flag and the two agent items, less the 50 listed.
        assert_eq!(review.more, flags + 2 - MAX_REVIEW);
        let t = build(&conn, since, now).unwrap();
        assert_eq!(
            (t.memory_review.len(), t.memory_review_more),
            (MAX_REVIEW, review.more)
        );
    }

    /// A flag on a fact injected memory never carries (an unpinned playbook, a subagent's
    /// fact) says so, since keeping it brings nothing back; a pinned playbook is injected.
    #[test]
    fn a_flag_on_a_fact_memory_never_injects_is_marked() {
        use crate::knowledge::{
            flag_fact, set_fact_pinned, upsert_playbook, FlagKind, SignalSource,
        };
        let mut conn = dts::temp_db("today_review_injectable");
        let now = chrono::Utc::now().timestamp();
        let tid = dts::session(&mut conn, "i.jsonl", "/r/app", "s", 90_000, &["hi"]);
        let sub = dts::session(&mut conn, "i-sub.jsonl", "/r/app", "sub", 90_000, &["hi"]);
        conn.execute("UPDATE threads SET is_subagent = 1 WHERE id = ?1", [sub])
            .unwrap();
        let text = "error[E0308]: mismatched types: seen 2 times; fixed in abc1234";
        upsert_playbook(&conn, "playbook:a@/r/app", tid, text, now).unwrap();
        let playbook: i64 = conn
            .query_row(
                "SELECT id FROM facts WHERE extractor = 'playbook'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let plain = review_fact(&conn, tid, "plain decision", now);
        let of_subagent = review_fact(&conn, sub, "a subagent's decision", now);
        for id in [playbook, plain, of_subagent] {
            flag_fact(
                &conn,
                id,
                FlagKind::Stale,
                SignalSource::Distill,
                None,
                None,
                now,
            )
            .unwrap();
        }
        let injectable = |conn: &Connection| -> Vec<(i64, bool)> {
            let mut got: Vec<(i64, bool)> = memory_review(conn, &ProjectScope::Any, now)
                .unwrap()
                .items
                .iter()
                .map(|i| (i.fact_id, i.injectable))
                .collect();
            got.sort_unstable();
            got
        };
        let mut want = vec![(playbook, false), (plain, true), (of_subagent, false)];
        want.sort_unstable();
        assert_eq!(injectable(&conn), want);
        set_fact_pinned(&conn, playbook, true).unwrap();
        assert!(injectable(&conn).contains(&(playbook, true)));
    }

    /// Keep on a recorded or replaced item is stored, so the item stays off the list for
    /// Today and `cal memory review` alike, and the replacer a kept replacement names is not
    /// listed in its place. A flag or a dispute is kept by settling it, not here. An entry no
    /// window can list any more is dropped on the next Keep.
    #[test]
    fn a_kept_review_item_stays_kept() {
        use crate::knowledge::{record_fact_with_origin, FactOrigin};
        let mut conn = dts::temp_db("today_review_kept");
        let now = chrono::Utc::now().timestamp();
        let since = now - 3_600;
        let tid = dts::session(&mut conn, "k.jsonl", "/r/app", "s", 90_000, &["hi"]);
        let old = review_fact(&conn, tid, "cache with bincode", now - 30 * 86_400);
        let record = |text: &str, replaces: Option<i64>, at: i64| {
            record_fact_with_origin(
                &conn,
                "/r/app",
                "decision",
                text,
                None,
                FactOrigin::Agent,
                replaces,
                at,
            )
            .unwrap()
            .id
        };
        let replacer = record("cache with JSON", Some(old), now - 60);
        let recorded = record("sign every webhook", None, now - 30);
        let listed = |conn: &Connection| -> Vec<(ReviewReason, i64)> {
            memory_review(conn, &ProjectScope::Any, since)
                .unwrap()
                .items
                .iter()
                .map(|i| (i.reason, i.fact_id))
                .collect()
        };
        assert_eq!(
            listed(&conn),
            vec![
                (ReviewReason::Replaced, old),
                (ReviewReason::Recorded, recorded)
            ]
        );

        keep_review_item(&conn, ReviewReason::Recorded, recorded, None, now).unwrap();
        assert_eq!(listed(&conn), vec![(ReviewReason::Replaced, old)]);
        keep_review_item(&conn, ReviewReason::Replaced, old, Some(replacer), now).unwrap();
        assert!(
            listed(&conn).is_empty(),
            "the replacer is not listed instead"
        );
        // As `cal memory review` asks: a week back.
        assert!(
            memory_review(&conn, &ProjectScope::Any, now - REVIEW_RECENT_SECS)
                .unwrap()
                .items
                .is_empty()
        );

        for reason in [ReviewReason::Flagged, ReviewReason::Disputed] {
            assert!(keep_review_item(&conn, reason, recorded, None, now).is_err());
        }
        assert!(keep_review_item(&conn, ReviewReason::Replaced, old, None, now).is_err());
        assert!(keep_review_item(&conn, ReviewReason::Recorded, -1, None, now).is_err());

        let stored = |conn: &Connection| -> HashMap<String, i64> {
            serde_json::from_str(&app_config::get(conn, REVIEW_KEPT_KEY).unwrap().unwrap()).unwrap()
        };
        // Ten days on the app has marked this load opened, but the window on screen still
        // starts at the previous open (`since`): a Keep drops nothing that window lists.
        let later = now + 10 * 86_400;
        mark_opened(&conn, later - 30).unwrap();
        let next = record("rotate the signing key", None, later - 10);
        keep_review_item(&conn, ReviewReason::Recorded, next, None, later).unwrap();
        assert_eq!(stored(&conn).len(), 3, "{:?}", stored(&conn));
        assert!(listed(&conn).is_empty());
        // Past REVIEW_KEPT_SECS no window lists them, Today's from a stale open included: the
        // next Keep drops the entries kept that long ago.
        let much_later = now + REVIEW_KEPT_SECS + 60;
        let last = record("pin the retry budget", None, much_later - 10);
        keep_review_item(&conn, ReviewReason::Recorded, last, None, much_later).unwrap();
        let mut kept_at: Vec<i64> = stored(&conn).into_values().collect();
        kept_at.sort_unstable();
        assert_eq!(kept_at, [later, much_later]);
        assert!(build(&conn, since, much_later)
            .unwrap()
            .memory_review
            .is_empty());
    }

    /// An index the app has not upgraded to 0035 has nothing to review, and says so quietly.
    #[test]
    fn memory_review_is_empty_without_the_truth_tables() {
        let path = db::test_support::TestPath::new("today_review_old");
        db::test_support::index_at_version(&path, 34, |_| {});
        let conn = db::open_readonly(&path).unwrap();
        assert!(memory_review(&conn, &ProjectScope::Any, 0)
            .unwrap()
            .items
            .is_empty());
    }

    #[test]
    fn groups_by_project_newest_first_and_skips_old_and_subagent_threads() {
        let mut conn = dts::temp_db("today_groups");
        let now = chrono::Utc::now().timestamp();
        let a1 = dts::session(
            &mut conn,
            "a1.jsonl",
            "/r/app",
            "app old-ish",
            3_000,
            &["one"],
        );
        let b1 = dts::session(&mut conn, "b1.jsonl", "/r/web", "web", 2_000, &["two"]);
        let a2 = dts::session(
            &mut conn,
            "a2.jsonl",
            "/r/app",
            "app newest",
            100,
            &["x", "y"],
        );
        dts::session(
            &mut conn,
            "old.jsonl",
            "/r/app",
            "yesterday",
            90_000,
            &["z"],
        );
        let sub = dts::session(&mut conn, "sub.jsonl", "/r/app", "subagent", 50, &["s"]);
        conn.execute("UPDATE threads SET is_subagent = 1 WHERE id = ?1", [sub])
            .unwrap();
        // A notes thread (facts only, no messages) is not a session.
        crate::knowledge::record_fact(&conn, "/r/app", "decision", "use x", None, now).unwrap();

        let t = build(&conn, now - 86_000, now).unwrap();
        let projects: Vec<&str> = t.groups.iter().map(|g| g.project.as_str()).collect();
        assert_eq!(projects, ["/r/app", "/r/web"]);
        let app: Vec<i64> = t.groups[0].threads.iter().map(|t| t.id).collect();
        assert_eq!(app, [a2, a1]);
        assert_eq!(t.groups[1].threads[0].id, b1);
        let newest = &t.groups[0].threads[0];
        assert_eq!(newest.source, "claude_code");
        // Prompts at at-100 and at-90, the tool row at `at`: 100 s from first to last.
        assert_eq!(newest.duration_secs, 100);
        assert_eq!(newest.messages, 5);
        assert!(!t.commits_partial);
    }

    #[test]
    fn since_defaults_to_the_last_open_then_a_day() {
        let conn = dts::temp_db("today_since");
        assert_eq!(
            resolve_since(&conn, None, 1_000_000).unwrap(),
            1_000_000 - 86_400
        );
        mark_opened(&conn, 999_000).unwrap();
        assert_eq!(resolve_since(&conn, None, 1_000_000).unwrap(), 999_000);
        assert_eq!(resolve_since(&conn, Some(5), 1_000_000).unwrap(), 5);
    }

    #[test]
    fn last_project_round_trips_and_clears() {
        let conn = dts::temp_db("today_lastproj");
        assert_eq!(last_project(&conn).unwrap(), None);
        set_last_project(&conn, Some("/r/app")).unwrap();
        assert_eq!(last_project(&conn).unwrap().as_deref(), Some("/r/app"));
        set_last_project(&conn, Some("  ")).unwrap();
        assert_eq!(last_project(&conn).unwrap(), None);
        set_last_project(&conn, Some("/r/web")).unwrap();
        set_last_project(&conn, None).unwrap();
        assert_eq!(last_project(&conn).unwrap(), None);
    }

    #[test]
    fn todos_issues_and_snapshots_since() {
        let mut conn = dts::temp_db("today_strip");
        let now = chrono::Utc::now().timestamp();
        let since = now - 3_600;
        let tid = dts::session(&mut conn, "s.jsonl", "/r/app", "s", 60, &["hi"]);
        let ins_todo = |text: &str, at: i64, extractor: &str| {
            conn.execute(
                "INSERT INTO facts (thread_id, kind, text, status, extractor, created_at)
                 VALUES (?1, 'todo', ?2, 'open', ?3, ?4)",
                rusqlite::params![tid, text, extractor, at],
            )
            .unwrap();
        };
        ins_todo("fresh todo", now - 60, "heuristic");
        ins_todo("old todo", now - 7_200, "heuristic");
        ins_todo("llm todo", now - 60, "llm");

        // The same error before and after `since` is seen again; one only after is not.
        let mut seq = 100;
        let mut tool = |text: &str, ts: i64| {
            seq += 1;
            conn.execute(
                "INSERT INTO messages (thread_id, seq, role, text, ts) VALUES (?1, ?2, 'tool', ?3, ?4)",
                rusqlite::params![tid, seq, text, ts],
            )
            .unwrap();
        };
        tool(
            "error[E0425]: cannot find value `x` in this scope",
            now - 86_400,
        );
        tool(
            "error[E0425]: cannot find value `y` in this scope",
            now - 30,
        );
        tool("TypeError: Cannot read properties of undefined", now - 40);
        tool("TypeError: Cannot read properties of undefined", now - 20);
        let snap = snapshot::create(&conn, tid, Some("before deploy")).unwrap();
        let t = build(&conn, since, now).unwrap();
        let texts: Vec<&str> = t.new_todos.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, ["fresh todo"]);
        assert_eq!(t.new_todos[0].project, "/r/app");
        assert_eq!(t.new_todos[0].thread_id, tid);
        assert_eq!(t.issues_seen_again.len(), 1, "{:?}", t.issues_seen_again);
        let seen = &t.issues_seen_again[0];
        assert!(seen.signature.starts_with("error[E0425]"));
        assert_eq!(seen.count, 2);
        assert_eq!(seen.project, "/r/app");
        assert_eq!(t.snapshots.len(), 1);
        assert_eq!(t.snapshots[0].id, snap.id);
        assert!(build(&conn, now + 10, now).unwrap().snapshots.is_empty());
    }

    #[test]
    fn link_commits_skips_repos_without_git_and_reports_the_budget() {
        let mut conn = dts::temp_db("today_git");
        let now = chrono::Utc::now().timestamp();
        dts::session(
            &mut conn,
            "n.jsonl",
            "/nonexistent/repo",
            "n",
            60,
            &["edit src/a.rs"],
        );
        let mut t = build(&conn, now - 3_600, now).unwrap();
        // No .git: nothing to link, so nothing is partial even with no budget at all.
        assert!(!link_commits(&conn, &t, Duration::ZERO).unwrap());
        fill_commits(&conn, &mut t).unwrap();
        assert!(t.groups[0].threads[0].commits.is_empty());
    }

    /// A temp dir standing in for a checkout: `.git` is a dir for the main repo and a file
    /// for a linked worktree, as git lays them out.
    fn fake_checkout(tag: &str, worktree: bool) -> String {
        let dir = std::env::temp_dir().join(format!(
            "calli_today_{tag}_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        if worktree {
            std::fs::write(dir.join(".git"), "gitdir: /elsewhere/.git/worktrees/x\n").unwrap();
        } else {
            std::fs::create_dir_all(dir.join(".git")).unwrap();
        }
        dir.to_str().unwrap().to_string()
    }

    fn log_record(sha: &str, at: i64, subject: &str, files: &[&str]) -> String {
        format!(
            "\u{1e}COMMIT\u{1f}{sha}\u{1f}{at}\u{1f}{subject}\n{}\n",
            files.join("\n")
        )
    }

    fn links(conn: &Connection, thread_id: i64) -> Vec<String> {
        let mut shas: Vec<String> = conn
            .prepare("SELECT sha FROM thread_commits WHERE thread_id = ?1")
            .unwrap()
            .query_map([thread_id], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        shas.sort();
        shas
    }

    #[test]
    fn link_commits_runs_git_in_each_sessions_own_checkout_and_keeps_other_links() {
        let main = fake_checkout("main", false);
        let wt = fake_checkout("wt", true);
        let mut conn = dts::temp_db("today_worktree");
        let now = chrono::Utc::now().timestamp();
        let in_main = dts::session(&mut conn, "m.jsonl", &main, "main work", 300, &["edit"]);
        let in_wt = dts::session(&mut conn, "w.jsonl", &wt, "worktree work", 60, &["edit"]);
        // Both group under the main repo, as `canonical_project` resolves a worktree.
        conn.execute(
            "UPDATE threads SET project_key = ?1 WHERE id IN (?2, ?3)",
            rusqlite::params![main, in_main, in_wt],
        )
        .unwrap();
        for (tid, path) in [(in_main, "src/main.rs"), (in_wt, "src/branch.rs")] {
            conn.execute(
                "INSERT INTO file_mentions (thread_id, path) VALUES (?1, ?2)",
                rusqlite::params![tid, path],
            )
            .unwrap();
        }
        let ins_link = |tid: i64, sha: &str| {
            conn.execute(
                "INSERT INTO thread_commits (thread_id, sha, short_sha, subject, committed_at, overlap, created_at)
                 VALUES (?1, ?2, ?3, 'old', ?4, 1, 0)",
                rusqlite::params![tid, sha, &sha[..7], now - 500],
            )
            .unwrap();
        };
        // Found earlier by ThreadView in the worktree; no log read here holds it.
        ins_link(in_wt, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        // In main's log but no longer touching anything the session mentioned: stale.
        ins_link(in_main, "dddddddddddddddddddddddddddddddddddddddd");

        let t = build(&conn, now - 3_600, now).unwrap();
        assert_eq!(t.groups.len(), 1, "{:?}", t.groups);
        let mut ran = Vec::new();
        let partial = link_commits_with(&conn, &t, COMMIT_BUDGET, |repo, _since, _deadline| {
            ran.push(repo.to_string());
            let log = if repo == wt {
                log_record(
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    now - 100,
                    "on the branch",
                    &["src/branch.rs"],
                )
            } else {
                log_record(
                    "cccccccccccccccccccccccccccccccccccccccc",
                    now - 340,
                    "on main",
                    &["src/main.rs"],
                ) + &log_record(
                    "dddddddddddddddddddddddddddddddddddddddd",
                    now - 340,
                    "unrelated",
                    &["docs/other.md"],
                )
            };
            Ok(Some(log))
        })
        .unwrap();
        assert!(!partial);
        // git ran once per checkout, the newest session's first, never the group key twice.
        assert_eq!(ran, [wt.clone(), main.clone()]);
        assert_eq!(
            links(&conn, in_wt),
            [
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            ]
        );
        assert_eq!(
            links(&conn, in_main),
            ["cccccccccccccccccccccccccccccccccccccccc"]
        );

        // A deadline hit inside git writes nothing and says the commits are partial.
        let partial = link_commits_with(&conn, &t, COMMIT_BUDGET, |_, _, _| Ok(None)).unwrap();
        assert!(partial);
        assert_eq!(links(&conn, in_main).len(), 1);
        let _ = std::fs::remove_dir_all(&main);
        let _ = std::fs::remove_dir_all(&wt);
    }

    #[test]
    fn a_long_absence_is_truncated_and_mines_issues_from_a_floor() {
        let mut conn = dts::temp_db("today_trunc");
        let now = chrono::Utc::now().timestamp();
        for i in 0..(MAX_THREADS + 2) {
            dts::session(
                &mut conn,
                &format!("t{i}.jsonl"),
                "/r/app",
                "t",
                60 + i,
                &["x"],
            );
        }
        let t = build(&conn, now - 86_000, now).unwrap();
        let listed: usize = t.groups.iter().map(|g| g.threads.len()).sum();
        assert_eq!(listed as i64, MAX_THREADS);
        assert!(t.truncated);
        assert!(!build(&conn, now - 100, now).unwrap().truncated);
        // A window from a year ago mines from 30 days ago (minus the lookback), not a year.
        assert_eq!(issue_since(now - 365 * 86_400, now), now - 30 * 86_400);
        assert_eq!(issue_since(now - 3_600, now), now - 3_600);
        assert_eq!(issue_since(now + 10, now), now);
    }

    #[test]
    fn todos_from_in_app_chats_are_not_new_todos() {
        let mut conn = dts::temp_db("today_inapp");
        let now = chrono::Utc::now().timestamp();
        let tid = dts::session(&mut conn, "s.jsonl", "/r/app", "s", 60, &["hi"]);
        let chat_src = crate::indexer::source_id(&conn, "in_app").unwrap();
        conn.execute(
            "INSERT INTO threads (source_id, external_id, title, message_count, updated_at)
             VALUES (?1, 'ask-1', 'ask', 2, ?2)",
            rusqlite::params![chat_src, now],
        )
        .unwrap();
        let chat = conn.last_insert_rowid();
        for (thread, text) in [(tid, "in the session"), (chat, "quoted in an answer")] {
            conn.execute(
                "INSERT INTO facts (thread_id, kind, text, status, extractor, created_at)
                 VALUES (?1, 'todo', ?2, 'open', 'heuristic', ?3)",
                rusqlite::params![thread, text, now - 30],
            )
            .unwrap();
        }
        let t = build(&conn, now - 3_600, now).unwrap();
        let texts: Vec<&str> = t.new_todos.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, ["in the session"]);
    }

    /// Times the Today view's reads on the index `CALLIMACHUS_DB` names (read-only; skipped
    /// without it, so it never opens the real index by accident). On a 21k-thread clone the
    /// live issue mining took 147 ms warm (912 ms cold) for a day's window, and 227 ms warm
    /// (672 ms cold) for a year away, which the floor cuts to two months of messages.
    #[test]
    #[ignore]
    fn time_today_on_a_real_index() {
        let Some(path) = std::env::var_os("CALLIMACHUS_DB") else {
            eprintln!("set CALLIMACHUS_DB to a copy of an index");
            return;
        };
        let conn = db::open_readonly(std::path::Path::new(&path)).unwrap();
        let newest: i64 = conn
            .query_row("SELECT MAX(ts) FROM messages", [], |r| r.get(0))
            .unwrap();
        for (label, since) in [
            ("a day", newest - 86_400),
            ("a year", newest - 365 * 86_400),
        ] {
            for pass in ["cold", "warm"] {
                let t = Instant::now();
                let issues = issues_seen_again(&conn, since, newest).unwrap();
                let issue_ms = t.elapsed().as_millis();
                let t = Instant::now();
                let view = build(&conn, since, newest).unwrap();
                eprintln!(
                    "since {label} ({pass}): issues {issue_ms} ms ({} seen again), build {} ms, \
                     {} groups, truncated {}",
                    issues.len(),
                    t.elapsed().as_millis(),
                    view.groups.len(),
                    view.truncated
                );
            }
        }
    }

    #[test]
    fn link_commits_links_a_real_repo_and_fills_the_thread() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let dir = std::env::temp_dir().join(format!(
            "calli_today_git_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let repo = dir.to_str().unwrap().to_string();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(dir.join("src/auth.rs"), "fn a() {}").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "wire auth"]);

        let mut conn = dts::temp_db("today_realgit");
        let now = chrono::Utc::now().timestamp();
        let key = crate::knowledge::project_key(&repo);
        let tid = dts::session(&mut conn, "g.jsonl", &key, "auth", 10, &["fix src/auth.rs"]);
        conn.execute(
            "INSERT OR IGNORE INTO file_mentions (thread_id, path) VALUES (?1, 'src/auth.rs')",
            [tid],
        )
        .unwrap();
        // The commit landed seconds ago, inside the thread's window.
        let mut t = build(&conn, now - 3_600, now).unwrap();
        assert!(t.groups[0].threads[0].commits.is_empty());
        assert!(!link_commits(&conn, &t, COMMIT_BUDGET).unwrap());
        fill_commits(&conn, &mut t).unwrap();
        let commits = &t.groups[0].threads[0].commits;
        assert_eq!(commits.len(), 1, "{commits:?}");
        assert_eq!(commits[0].subject.as_deref(), Some("wire auth"));
        assert_eq!(commits[0].sha.len(), 40);
        // An exhausted budget skips the repo and says so.
        assert!(link_commits(&conn, &t, Duration::ZERO).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
