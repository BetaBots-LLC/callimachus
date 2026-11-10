//! "Your agents, so far": one report over the whole index, for `cal report` and the app's
//! Report view. Every section comes from tables and modules that already exist (`cost`,
//! `issues`, facts, `file_edits`, `thread_commits`, `ledger`) and is an `Option`, so a
//! section with no data is left out rather than shown empty. Nothing here calls an LLM or
//! the network, and nothing writes: it runs on a pooled read-only connection.
//!
//! Strings that can carry a secret or a prompt (error lines, prompts, commit subjects,
//! session titles) are redacted as the report is built, because the report is made to be
//! shared: `--markdown`, the app's "Copy as markdown" and the PNG card all start from it.

use crate::cost;
use crate::export::{fact_line, is_harness_prompt, project_label, source_label};
use crate::issues;
use crate::ledger;
use crate::search::ProjectMatch;
use anyhow::Result;
use rusqlite::Connection;
use schemars::JsonSchema;
use serde::Serialize;
use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;

/// Printed small on the share card and at the foot of the markdown.
pub const SITE: &str = "callimachus.app";

/// A session's span counts toward the hours estimate up to this: a session left open
/// overnight, or resumed days later, is not days of work.
const SESSION_CAP_SECS: i64 = 8 * 3600;
/// Rows per section.
const TOP_TOOLS_SPEND: usize = 8;
const TOP_MODELS: usize = 8;
const TOP_COMMITS: usize = 5;
const TOP_ERRORS: usize = 5;
const TOP_PROMPTS: usize = 5;
const TOP_DECISION_PROJECTS: usize = 3;
const TOP_FILES: usize = 10;
/// The recent spend window, in local days with today.
const RECENT_DAYS: u32 = 30;
/// "Solved more than once" needs this many sessions: on a small index two matching prompts
/// are more likely a habit ("continue", a template) than a solved problem coming back.
const PROMPT_MIN_SESSIONS: usize = 50;
/// A prompt shorter than this (after normalizing) says too little to call two sessions the
/// same task.
const PROMPT_MIN_WORDS: usize = 8;
/// Two sessions count as the same task solved again only when they started this far apart;
/// a retry the same day is the same attempt, so sessions closer than this count once.
const PROMPT_MIN_GAP_SECS: i64 = 86_400;
/// Characters of a prompt read and compared. A first prompt can paste a whole log; the
/// opening is what names the task.
const PROMPT_HEAD_CHARS: i64 = 2_000;
/// User turns read per session to find its first typed prompt: Claude Code opens many with
/// a `<local-command-caveat>` or a slash-command expansion first.
const PROMPT_LOOKAHEAD: i64 = 3;
/// Session ids kept per prompt group.
const PROMPT_THREAD_IDS: usize = 10;
/// Recurring error clusters read before the ones with no tool-written example are dropped
/// (see [`tool_example`]): enough that five usually remain.
const ERROR_CANDIDATES: usize = TOP_ERRORS * 3;
/// Newest error-ish tool rows read when looking for a tool-written example of a cluster
/// whose stored one came from a user turn. 0.5 s on a 5 GB index for the broadest words.
const TOOL_EXAMPLE_ROWS: i64 = 200;
/// Characters of an error example kept: one line on the card, one list item in markdown.
const ERROR_EXAMPLE_CHARS: usize = 160;

/// The whole report. `since` bounds every section with a time in it, with two exceptions:
/// recurring errors never reach back past 180 days (the window `issue_clusters` covers), and
/// the ledger's `allTime` counts every event ever recorded.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    /// When it was computed (epoch seconds).
    pub generated_at: i64,
    /// Start of the window (epoch seconds); null for all time.
    pub since: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub totals: Option<Totals>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spend: Option<ReportSpend>,
    /// The most frequent recurring errors across every project.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recurring_errors: Option<Vec<ReportIssue>>,
    /// Sessions that opened with the same prompt on different days.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub solved_again: Option<Vec<PromptGroup>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<ReportMemory>,
    /// The files agents edited or wrote most, inside their project only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<ReportFile>>,
    /// Edited files left out of `files` because they lie outside their session's project
    /// (a plan file, a temp script), whose full path would name the user's folders.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files_omitted: Option<i64>,
    /// What the hooks and the MCP server did (the local value ledger).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ledger: Option<ReportLedger>,
}

/// Sessions and time across every agent. A session is a top-level transcript: subagent
/// transcripts count toward messages, not sessions, and the app's own chats are left out.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    pub sessions: i64,
    /// Messages in those sessions and their subagents.
    pub messages: i64,
    /// When the first session in the window started (epoch seconds).
    pub first_session: i64,
    /// When the last one was last active (epoch seconds).
    pub last_session: i64,
    /// Local calendar days on which a session started or was last active.
    pub active_days: i64,
    /// The sum of each session's span, each capped at 8 hours, over the sessions with a
    /// usable span (`hoursSessions`): an estimate.
    pub estimated_hours: f64,
    /// Sessions whose span is known: the thread's own times, else its messages' first and
    /// last timestamps. Cursor stores most sessions with neither.
    pub hours_sessions: i64,
    /// The agents (source kinds) those sessions ran in, most sessions first.
    pub hours_sources: Vec<String>,
    /// Sessions per agent, most first.
    pub by_tool: Vec<ToolShare>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolShare {
    /// The source kind (`claude_code`, `cursor`...).
    pub source: String,
    /// Its display name.
    pub label: String,
    pub sessions: i64,
    /// This agent's share of the sessions, 0 to 1.
    pub share: f64,
}

/// Estimated spend, in the list-price terms `cal cost` uses.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReportSpend {
    /// What the figures are (list-price equivalents, not a bill).
    pub basis: String,
    /// Over the report's window (all time when `since` is null).
    pub total: SpendWindow,
    /// The last 30 local days, today included (within the window).
    pub last30_days: SpendWindow,
    /// The commits whose sessions cost the most, priciest first.
    pub priciest_commits: Vec<cost::CommitCost>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SpendWindow {
    /// Start of the window (epoch seconds; 0 for all time).
    pub since: i64,
    pub total_cost: f64,
    pub tracked_calls: i64,
    /// Calls on models with no list price, not counted in the cost.
    pub untracked_calls: i64,
    /// Priciest agent first.
    pub by_tool: Vec<ToolSpend>,
    /// Priced models, priciest first (at most 8).
    pub by_model: Vec<cost::ModelCost>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolSpend {
    pub source: String,
    pub label: String,
    pub cost: f64,
    pub calls: i64,
    /// Share of the window's cost, 0 to 1.
    pub share: f64,
}

/// A recurring error and, once a linked commit fixed it, the fix.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReportIssue {
    /// A representative error line from tool output (never a user turn), secrets redacted,
    /// absolute paths shortened to file names, cut to one line.
    pub example: String,
    /// Messages it appeared in.
    pub count: i64,
    /// Sessions it appeared in.
    pub sessions: i64,
    pub first_seen: i64,
    pub last_seen: i64,
    /// The project of the latest sighting.
    pub project: Option<String>,
    pub fix: Option<issues::IssueFix>,
}

/// Sessions whose first prompt was the same task, asked again on another day.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PromptGroup {
    /// The newest session's prompt, secrets redacted and cut to one line.
    pub prompt: String,
    /// Attempts at least a day apart: sessions opened closer together count once.
    pub sessions: i64,
    pub first_seen: i64,
    pub last_seen: i64,
    /// The project most of them ran in.
    pub project: Option<String>,
    /// The sessions, newest first (at most 10), one per day-apart attempt.
    pub thread_ids: Vec<i64>,
}

/// Project memory across every repo.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReportMemory {
    pub decisions: i64,
    pub gotchas: i64,
    pub open_todos: i64,
    /// Projects with at least one decision or gotcha.
    pub projects_with_memory: i64,
    /// The projects with the most decisions, most first.
    pub top_decision_projects: Vec<ProjectDecisions>,
    /// How often a session started with memory injected, from the value ledger.
    pub injected: Option<Injected>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectDecisions {
    pub project: String,
    pub decisions: i64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Injected {
    /// SessionStart injections of project memory since `sinceDay`.
    pub injections: u64,
    /// Sessions that started since `sinceDay`, on every agent.
    pub sessions: i64,
    /// `injections / sessions`, at most 1. A `/clear` injects again, so this can run high.
    pub share: f64,
    /// The first local day (`YYYY-MM-DD`) the ledger still holds.
    pub since_day: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReportFile {
    /// Relative to the project (a file outside it is counted in `filesOmitted` instead).
    pub path: String,
    pub project: Option<String>,
    /// Edit and write tool calls on it.
    pub edits: i64,
    pub sessions: i64,
}

/// The value ledger's counts over the days it keeps (90), plus the all-time total.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReportLedger {
    pub counts: ledger::Counts,
    /// The first local day (`YYYY-MM-DD`) the counts cover.
    pub since_day: Option<String>,
    /// Every event ever counted, pruned days included.
    pub all_time: u64,
}

type ReportKey = Option<i64>;

static REPORT_CACHE: std::sync::LazyLock<crate::search::cache::AggCache<ReportKey, Report>> =
    std::sync::LazyLock::new(crate::search::cache::AggCache::new);

/// [`build`], reused for up to 10 minutes while the thread set is unchanged (see
/// `search::cache`), with `since` bucketed to the hour so "the last 30 days" asked twice in a
/// minute shares an entry. For the app; `cal report` reads live.
pub fn build_cached(conn: &Connection, since: Option<i64>, now: i64) -> Result<Report> {
    let since = since.map(|s| s - s.rem_euclid(3600));
    REPORT_CACHE.get_or_try(crate::search::cache::index_version(conn)?, since, || {
        build(conn, since, now)
    })
}

/// Build the report over sessions started since `since` (epoch seconds, None for all time).
/// A section that fails is logged and left out: one unreadable table must not cost the whole
/// report.
pub fn build(conn: &Connection, since: Option<i64>, now: i64) -> Result<Report> {
    let since = since.filter(|s| *s > 0);
    let sessions = load_sessions(conn, since)?;
    let (files, files_omitted) = match section("files", files(conn, since)) {
        Some((rows, omitted)) => (Some(rows), (omitted > 0).then_some(omitted)),
        None => (None, None),
    };
    Ok(Report {
        generated_at: now,
        since,
        totals: totals(&sessions),
        spend: section("spend", spend(conn, since)),
        recurring_errors: section("recurring errors", recurring_errors(conn, since, now)),
        solved_again: section("solved again", solved_again(conn, &sessions)),
        memory: section("memory", memory(conn, since, &sessions)),
        files,
        files_omitted,
        ledger: section("ledger", ledger_totals(conn, since)),
    })
}

fn section<T>(name: &str, r: Result<Option<T>>) -> Option<T> {
    match r {
        Ok(v) => v,
        Err(e) => {
            app_log!("report: left out the {name} section: {e:#}");
            None
        }
    }
}

fn red(text: &str) -> String {
    crate::context::redact_secrets(text).into_owned()
}

/// One transcript: a session, or a subagent of one.
struct Session {
    id: i64,
    kind: String,
    start: i64,
    end: i64,
    /// Seconds the session ran, when known (see [`SESSIONS_SQL`]).
    span: Option<i64>,
    messages: i64,
    subagent: bool,
    project: Option<String>,
}

/// Every agent transcript (not the app's own chats) started in the window. A thread with no
/// timestamps at all is left out: it has no place on a timeline.
///
/// The last column is the span of the thread's message timestamps, read only for a session
/// whose own times give none: Cursor writes `updated_at` equal to `created_at` (or earlier,
/// even a 1970 date) for almost every session, so without this the hours figure covered
/// Claude Code alone. Each subquery reads one thread's range of `idx_messages_thread_ts`; the
/// `+` keeps the planner off the partial `idx_messages_ts`, which it picks without stats and
/// which would walk every timed message in the index per thread.
const SESSIONS_SQL: &str = "SELECT t.id, s.kind, t.created_at, t.updated_at, t.message_count,
        t.is_subagent, COALESCE(t.project_key, t.project_path),
        CASE WHEN t.is_subagent = 0 AND NOT COALESCE(t.updated_at > t.created_at, 0)
             THEN (SELECT MAX(m.ts) FROM messages m WHERE m.thread_id = t.id AND +m.ts > 0)
                - (SELECT MIN(m.ts) FROM messages m WHERE m.thread_id = t.id AND +m.ts > 0)
        END
     FROM threads t JOIN sources s ON s.id = t.source_id
     WHERE s.kind <> 'in_app' AND COALESCE(t.created_at, t.updated_at) >= ?1";

fn load_sessions(conn: &Connection, since: Option<i64>) -> Result<Vec<Session>> {
    let mut stmt = conn.prepare(SESSIONS_SQL)?;
    let rows = stmt
        .query_map([since.unwrap_or(0)], |r| {
            let created: Option<i64> = r.get(2)?;
            let updated: Option<i64> = r.get(3)?;
            let from_messages: Option<i64> = r.get(7)?;
            let start = created.or(updated).unwrap_or(0);
            let span = match (created, updated) {
                (Some(c), Some(u)) if u > c => Some(u - c),
                _ => from_messages.filter(|s| *s > 0),
            };
            Ok(Session {
                id: r.get(0)?,
                kind: r.get(1)?,
                start,
                end: updated.unwrap_or(start).max(start),
                span,
                messages: r.get(4)?,
                subagent: r.get::<_, i64>(5)? != 0,
                project: r.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn local_day(ts: i64) -> Option<chrono::NaiveDate> {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|t| t.date_naive())
}

fn totals(all: &[Session]) -> Option<Totals> {
    let sessions: Vec<&Session> = all.iter().filter(|s| !s.subagent).collect();
    if sessions.is_empty() {
        return None;
    }
    let mut days = BTreeSet::new();
    let mut seconds = 0i64;
    let mut timed = 0i64;
    let mut timed_kinds: BTreeSet<&str> = BTreeSet::new();
    let mut per_tool: HashMap<&str, i64> = HashMap::new();
    for s in &sessions {
        days.extend(local_day(s.start));
        days.extend(local_day(s.end));
        // A session with no usable span is left out of the hours, not counted as zero, so
        // the figure can say which sessions it covers.
        if let Some(span) = s.span {
            seconds += span.clamp(0, SESSION_CAP_SECS);
            timed += 1;
            timed_kinds.insert(s.kind.as_str());
        }
        *per_tool.entry(s.kind.as_str()).or_default() += 1;
    }
    let n = sessions.len() as i64;
    let mut by_tool: Vec<ToolShare> = per_tool
        .into_iter()
        .map(|(kind, count)| ToolShare {
            source: kind.to_string(),
            label: source_label(kind).to_string(),
            sessions: count,
            share: count as f64 / n as f64,
        })
        .collect();
    by_tool.sort_by(|a, b| {
        b.sessions
            .cmp(&a.sessions)
            .then_with(|| a.source.cmp(&b.source))
    });
    let hours_sources = by_tool
        .iter()
        .filter(|t| timed_kinds.contains(t.source.as_str()))
        .map(|t| t.source.clone())
        .collect();
    Some(Totals {
        sessions: n,
        messages: all.iter().map(|s| s.messages).sum(),
        first_session: sessions.iter().map(|s| s.start).min().unwrap_or(0),
        last_session: sessions.iter().map(|s| s.end).max().unwrap_or(0),
        active_days: days.len() as i64,
        estimated_hours: seconds as f64 / 3600.0,
        hours_sessions: timed,
        hours_sources,
        by_tool,
    })
}

fn spend_window(s: cost::SpendBySource, since: i64) -> SpendWindow {
    let total = s.total_cost;
    SpendWindow {
        since,
        total_cost: total,
        tracked_calls: s.tracked_calls,
        untracked_calls: s.untracked_calls,
        by_tool: s
            .by_source
            .into_iter()
            .take(TOP_TOOLS_SPEND)
            .map(|c| ToolSpend {
                label: source_label(&c.source).to_string(),
                share: if total > 0.0 { c.cost / total } else { 0.0 },
                source: c.source,
                cost: c.cost,
                calls: c.calls,
            })
            .collect(),
        by_model: s
            .by_model
            .into_iter()
            .filter(|m| m.priced && m.cost > 0.0)
            .take(TOP_MODELS)
            .collect(),
    }
}

fn spend(conn: &Connection, since: Option<i64>) -> Result<Option<ReportSpend>> {
    let from = since.unwrap_or(0);
    let all = cost::spend_by_source(conn, from)?;
    // Only unpriced calls (a Codex or Cursor user on models with no list price) would read
    // "$0.00": leave the section out, as the card already does.
    if all.tracked_calls == 0 || all.total_cost <= 0.0 {
        return Ok(None);
    }
    let recent_from = cost::local_day_start(RECENT_DAYS).max(from);
    let recent = cost::spend_by_source(conn, recent_from)?;
    // All time is the Stats card's breakdown, cached and shared with it; a window asks for
    // the commits made in it.
    let mut commits = match since {
        None => cost::breakdown_cached(conn, None)?.commits,
        Some(from) => cost::cost_report(
            conn,
            &cost::CostQuery {
                by: cost::CostBy::Commit,
                since: from,
                project: None,
                mode: ProjectMatch::Exact,
                limit: TOP_COMMITS,
            },
        )?
        .commits
        .unwrap_or_default(),
    };
    commits.truncate(TOP_COMMITS);
    for c in &mut commits {
        c.subject = c.subject.as_deref().map(red);
        for s in &mut c.sessions {
            s.title = s.title.as_deref().map(red);
        }
    }
    Ok(Some(ReportSpend {
        basis: cost::COST_BASIS.to_string(),
        total: spend_window(all, from),
        last30_days: spend_window(recent, recent_from),
        priciest_commits: commits,
    }))
}

fn recurring_errors(
    conn: &Connection,
    since: Option<i64>,
    now: i64,
) -> Result<Option<Vec<ReportIssue>>> {
    // The standard 180-day window is what the watcher keeps `issue_clusters` fresh for, so
    // an all-time report reads the stored clusters instead of mining again.
    let window = now - issues::CLUSTER_WINDOW_SECS;
    let from = since.map_or(window, |s| s.max(window));
    let found = issues::recurring_issues_with_fixes(
        conn,
        None,
        ProjectMatch::Exact,
        from,
        ERROR_CANDIDATES,
    )?;
    let mut project_of = conn
        .prepare_cached("SELECT COALESCE(project_key, project_path) FROM threads WHERE id = ?1")?;
    let mut out = Vec::with_capacity(TOP_ERRORS);
    for issue in found {
        if out.len() == TOP_ERRORS {
            break;
        }
        let c = &issue.cluster;
        // Issues are mined from user turns too (a pasted log), and the report is made to be
        // shared: a cluster with no tool-written line is left out rather than quote a prompt.
        let Some(example) = tool_example(conn, c, from)? else {
            continue;
        };
        let example = fact_line(&shorten_paths(&example), ERROR_EXAMPLE_CHARS);
        // Two clusters can read alike once their paths are cut to file names; the list (and
        // the app, which keys rows by it) shows each line once.
        if out.iter().any(|e: &ReportIssue| e.example == example) {
            continue;
        }
        let project: Option<String> = project_of
            .query_row([c.last_thread_id], |r| r.get(0))
            .ok()
            .flatten();
        out.push(ReportIssue {
            example,
            count: c.count,
            sessions: c.threads,
            first_seen: c.first_seen,
            last_seen: c.last_seen,
            project,
            fix: issue.fix.map(|mut f| {
                f.subject = f.subject.as_deref().map(red);
                f
            }),
        });
    }
    Ok((!out.is_empty()).then_some(out))
}

/// The cluster's stored example when tool output carried it (the sighting the cluster keeps:
/// its latest, in `last_thread_id` at `last_seen`). `instr` reads only the few rows of that
/// one (thread, ts) pair, through `idx_messages_thread_ts`.
const EXAMPLE_FROM_TOOL_SQL: &str = "SELECT 1 FROM messages m
     WHERE m.thread_id = ?1 AND m.ts = ?2 AND m.role = 'tool' AND instr(m.text, ?3) > 0
     LIMIT 1";

/// Characters of a long tool row's head and tail scanned for an error line, as
/// `issues::candidates_sql` reads them: errors lead a command's output or close it.
const TOOL_TEXT_HEAD: i64 = 6_000;
const TOOL_TEXT_TAIL: i64 = 2_000;

/// An error line from tool output for `c`: its stored example when a tool wrote it, else the
/// newest tool row since `from` with a line of the same signature. None when only user turns
/// carry it.
fn tool_example(conn: &Connection, c: &issues::IssueCluster, from: i64) -> Result<Option<String>> {
    let from_tool = conn
        .prepare_cached(EXAMPLE_FROM_TOOL_SQL)?
        .exists(rusqlite::params![c.last_thread_id, c.last_seen, c.example])?;
    if from_tool {
        return Ok(Some(c.example.clone()));
    }
    let signature = issues::normalize(&c.example);
    let Some(query) = signature_match(&signature) else {
        return Ok(None);
    };
    let sql = format!(
        "SELECT CASE WHEN octet_length(m.text) > {long}
                     THEN substr(m.text, 1, {TOOL_TEXT_HEAD}) || char(10) || substr(m.text, -{TOOL_TEXT_TAIL})
                     ELSE m.text END
         FROM messages_fts
         {join}
         WHERE messages_fts MATCH ?1 AND m.role = 'tool' AND m.ts >= ?2
         ORDER BY m.ts DESC LIMIT ?3",
        long = TOOL_TEXT_HEAD + TOOL_TEXT_TAIL,
        join = crate::search::fts_message_join(conn),
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(rusqlite::params![query, from, TOOL_EXAMPLE_ROWS])?;
    while let Some(row) = rows.next()? {
        let text: String = row.get(0)?;
        if let Some(line) = issues::extract_errors(&text)
            .into_iter()
            .find(|l| issues::normalize(l) == signature)
        {
            return Ok(Some(line));
        }
    }
    Ok(None)
}

/// An FTS query that every row carrying `signature` matches: its longest plain words (the
/// placeholders `issues::normalize` writes for paths, quotes and hashes left out), each
/// quoted. None when it has no such word, so a cluster like `error: <path>` is not looked
/// up by the commonest word in the index.
fn signature_match(signature: &str) -> Option<String> {
    let mut words: Vec<&str> = signature
        .split(|c: char| !c.is_ascii_alphabetic())
        .filter(|w| w.len() >= 3 && *w != "path")
        .collect();
    words.sort_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));
    words.dedup();
    words.truncate(4);
    if words.is_empty() || words.iter().all(|w| *w == "error") {
        return None;
    }
    Some(
        words
            .iter()
            .map(|w| format!("\"{w}\""))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// Where an absolute path starts in `tok`: `/x`, `~/x`, `C:\x`, `\\host` or `file:///x`, at the
/// token's start or after a quote, bracket, `=`, `:` or `,`. A relative `src/a.rs` names no
/// one's folders and stays.
fn abs_path_start(tok: &str) -> Option<usize> {
    let b = tok.as_bytes();
    (0..b.len()).find(|&i| {
        let boundary = i == 0
            || matches!(
                b[i - 1],
                b'\'' | b'"' | b'`' | b'(' | b'[' | b'<' | b'{' | b'=' | b':' | b','
            );
        if !boundary {
            return false;
        }
        let rest = &b[i..];
        match rest {
            // `//` is a URL's authority (`https://host/a`), which names a site, not a folder.
            [b'/', next, ..] => *next != b'/',
            [b'~', b'/' | b'\\', ..] | [b'\\', b'\\', ..] => true,
            [d, b':', b'\\' | b'/', ..] => d.is_ascii_alphabetic(),
            _ => rest.starts_with(b"file:///"),
        }
    })
}

/// `line` with each absolute or home path cut to its last component, so an error line on
/// the share card or in the markdown names `orchard.mjs`, not the user's folders.
fn shorten_paths(line: &str) -> String {
    line.split_whitespace()
        .map(|tok| {
            let mut out = String::new();
            let mut rest = tok;
            while let Some(start) = abs_path_start(rest) {
                out.push_str(&rest[..start]);
                let path = &rest[start..];
                let end = path
                    .find(|c: char| {
                        matches!(c, '\'' | '"' | '`' | ')' | ']' | '>' | '}' | ',' | ';')
                    })
                    .unwrap_or(path.len());
                let trimmed = path[..end].trim_end_matches(['/', '\\']);
                out.push_str(trimmed.rsplit(['/', '\\']).next().unwrap_or(""));
                rest = &path[end..];
            }
            out.push_str(rest);
            out
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The opening user turns of one session, oldest first, as their heads. The (thread_id, seq)
/// unique index serves it, so a session costs one short index range.
const FIRST_PROMPTS_SQL: &str = "SELECT substr(m.text, 1, ?2) FROM messages m
     WHERE m.thread_id = ?1 AND m.role = 'user' ORDER BY m.seq LIMIT ?3";

/// `text` lowercased with punctuation and whitespace collapsed, so the same ask typed again
/// compares equal. A word with a digit becomes `#` (another line number or count is the same
/// ask) and a path or URL its last component, `:line:col` cut (`src/a.rs:12` and
/// `lib/a.rs:40` are one file, but "cannot find module x" and "... module y" are two
/// problems, which compared equal while paths were dropped). Only plain words count toward
/// [`PROMPT_MIN_WORDS`]: None below it.
fn normalize_prompt(text: &str) -> Option<String> {
    let mut words = Vec::new();
    let mut plain = 0;
    for raw in text.split_whitespace() {
        if raw.contains('/') || raw.contains('\\') {
            let last = raw
                .trim_end_matches(|c: char| !c.is_alphanumeric())
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or("");
            let name: String = last
                .split(':')
                .next()
                .unwrap_or("")
                .chars()
                .filter(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_'))
                .flat_map(char::to_lowercase)
                .collect();
            words.push(if name.is_empty() {
                "<path>".into()
            } else {
                name
            });
            continue;
        }
        let word: String = raw
            .chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect();
        if word.is_empty() {
            continue;
        }
        if word.chars().any(|c| c.is_numeric()) {
            words.push("#".into());
            continue;
        }
        words.push(word);
        plain += 1;
    }
    (plain >= PROMPT_MIN_WORDS).then(|| words.join(" "))
}

/// First prompts a tool writes, not the user: Callimachus's own "open in CLI" handoff
/// (`agent::cli_resume`), Cursor's plan-mode Build button and Cursor's quick fix for an
/// editor error ("Fix in Chat"), whose template alone supplies the eight words and wraps
/// whatever error was under the cursor. Each repeats word for word across unrelated tasks,
/// so they would top "solved more than once" on every index (on a real one, two of the three
/// groups were quick fixes for different errors).
const TOOL_PROMPTS: [&str; 3] = [
    "Read the conversation context in @",
    "Implement the plan as specified",
    "For the code present, we get this error:",
];

/// A session's first typed prompt: the first user turn that is not a harness injection.
/// A tool-written opener ([`TOOL_PROMPTS`]) means the session has none.
fn first_prompt(
    stmt: &mut rusqlite::CachedStatement<'_>,
    thread_id: i64,
) -> Result<Option<String>> {
    let mut rows = stmt.query(rusqlite::params![
        thread_id,
        PROMPT_HEAD_CHARS,
        PROMPT_LOOKAHEAD
    ])?;
    while let Some(row) = rows.next()? {
        let text: String = row.get(0)?;
        if text.trim().is_empty() || is_harness_prompt(&text) {
            continue;
        }
        if TOOL_PROMPTS
            .iter()
            .any(|p| text.trim_start().starts_with(p))
        {
            return Ok(None);
        }
        return Ok(Some(text));
    }
    Ok(None)
}

fn solved_again(conn: &Connection, all: &[Session]) -> Result<Option<Vec<PromptGroup>>> {
    let sessions: Vec<&Session> = all.iter().filter(|s| !s.subagent).collect();
    if sessions.len() < PROMPT_MIN_SESSIONS {
        return Ok(None);
    }
    let mut stmt = conn.prepare_cached(FIRST_PROMPTS_SQL)?;
    // normalized prompt -> (session, its raw prompt)
    let mut groups: HashMap<String, Vec<(&Session, String)>> = HashMap::new();
    for s in &sessions {
        let Some(text) = first_prompt(&mut stmt, s.id)? else {
            continue;
        };
        if let Some(key) = normalize_prompt(&text) {
            groups.entry(key).or_default().push((s, text));
        }
    }
    let mut out: Vec<PromptGroup> = groups
        .into_values()
        .filter_map(|mut members| {
            members.sort_by(|a, b| b.0.start.cmp(&a.0.start).then(b.0.id.cmp(&a.0.id)));
            let first = members.last()?.0.start;
            let last = members.first()?.0.start;
            // Newest first, a session counts only when it started a day or more before the
            // last one counted: three tries in one afternoon are one attempt, not three.
            let mut attempts: Vec<i64> = Vec::new();
            let mut kept_start = i64::MAX;
            for (s, _) in &members {
                if kept_start.saturating_sub(s.start) >= PROMPT_MIN_GAP_SECS {
                    attempts.push(s.id);
                    kept_start = s.start;
                }
            }
            if attempts.len() < 2 {
                return None;
            }
            let mut projects: HashMap<&str, usize> = HashMap::new();
            for (s, _) in &members {
                if let Some(p) = s.project.as_deref() {
                    *projects.entry(p).or_default() += 1;
                }
            }
            let project = projects
                .into_iter()
                .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(a.0)))
                .map(|(p, _)| p.to_string());
            Some(PromptGroup {
                prompt: fact_line(&members[0].1, 160),
                sessions: attempts.len() as i64,
                first_seen: first,
                last_seen: last,
                project,
                thread_ids: attempts.into_iter().take(PROMPT_THREAD_IDS).collect(),
            })
        })
        .collect();
    if out.is_empty() {
        return Ok(None);
    }
    out.sort_by(|a, b| {
        b.sessions
            .cmp(&a.sessions)
            .then(b.last_seen.cmp(&a.last_seen))
    });
    out.truncate(TOP_PROMPTS);
    Ok(Some(out))
}

/// Visible decisions, gotchas and open TODOs recorded in the window.
const FACT_KINDS_SQL: &str = "SELECT f.kind, COUNT(*) FROM facts f
     WHERE f.hidden = 0 AND f.created_at >= ?1
       AND (f.kind IN ('decision', 'gotcha') OR (f.kind = 'todo' AND f.status = 'open'))
     GROUP BY f.kind";

/// Decisions and gotchas per project.
const FACT_PROJECTS_SQL: &str = "SELECT COALESCE(t.project_key, t.project_path) AS p,
        SUM(f.kind = 'decision')
     FROM facts f JOIN threads t ON t.id = f.thread_id
     WHERE f.hidden = 0 AND f.created_at >= ?1 AND f.kind IN ('decision', 'gotcha')
       AND p IS NOT NULL
     GROUP BY p";

fn memory(conn: &Connection, since: Option<i64>, all: &[Session]) -> Result<Option<ReportMemory>> {
    let from = since.unwrap_or(0);
    let mut kinds: HashMap<String, i64> = HashMap::new();
    let mut stmt = conn.prepare(FACT_KINDS_SQL)?;
    for row in stmt.query_map([from], |r| Ok((r.get::<_, String>(0)?, r.get(1)?)))? {
        let (kind, n) = row?;
        kinds.insert(kind, n);
    }
    let mut stmt = conn.prepare(FACT_PROJECTS_SQL)?;
    let mut projects: Vec<ProjectDecisions> = stmt
        .query_map([from], |r| {
            Ok(ProjectDecisions {
                project: r.get(0)?,
                decisions: r.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let projects_with_memory = projects.len() as i64;
    projects.retain(|p| p.decisions > 0);
    projects.sort_by(|a, b| {
        b.decisions
            .cmp(&a.decisions)
            .then_with(|| a.project.cmp(&b.project))
    });
    projects.truncate(TOP_DECISION_PROJECTS);

    let injected = injected(conn, since, all)?;
    let count = |k: &str| kinds.get(k).copied().unwrap_or(0);
    let (decisions, gotchas, open_todos) = (count("decision"), count("gotcha"), count("todo"));
    if decisions + gotchas + open_todos == 0 && injected.is_none() {
        return Ok(None);
    }
    Ok(Some(ReportMemory {
        decisions,
        gotchas,
        open_todos,
        projects_with_memory,
        top_decision_projects: projects,
        injected,
    }))
}

/// The local day `since` falls on, as the ledger keys its days.
fn since_day(since: Option<i64>) -> Option<String> {
    since
        .and_then(local_day)
        .map(|d| d.format("%Y-%m-%d").to_string())
}

/// Epoch seconds of local midnight starting `day` (`YYYY-MM-DD`).
fn day_start(day: &str) -> Option<i64> {
    use chrono::TimeZone;
    let date = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").ok()?;
    let midnight = date.and_hms_opt(0, 0, 0)?;
    chrono::Local
        .from_local_datetime(&midnight)
        .earliest()
        .map(|t| t.timestamp())
}

/// SessionStart injections over the sessions started in the days the ledger covers. The
/// ledger counts events, not sessions, so this is a ratio of counts, capped at 1.
fn injected(conn: &Connection, since: Option<i64>, all: &[Session]) -> Result<Option<Injected>> {
    let path = ledger::path_for_conn(conn);
    let kept = ledger::kept(conn, path.as_deref(), since_day(since).as_deref())?;
    let Some(first_day) = kept.first_day else {
        return Ok(None);
    };
    if kept.counts.hook == 0 {
        return Ok(None);
    }
    let Some(from) = day_start(&first_day) else {
        return Ok(None);
    };
    let sessions = all
        .iter()
        .filter(|s| !s.subagent && s.start >= from)
        .count() as i64;
    if sessions == 0 {
        return Ok(None);
    }
    Ok(Some(Injected {
        injections: kept.counts.hook,
        sessions,
        share: (kept.counts.hook as f64 / sessions as f64).min(1.0),
        since_day: first_day,
    }))
}

/// Edit and write calls per (project, path), the path made relative to the project when it
/// lies inside it, so `/repo/src/a.rs` and `src/a.rs` count as one file. `file_edits` has no
/// index on `op`, and the report wants every row of those two ops anyway, so this is one
/// pass over the table with each row's thread looked up by its primary key. CROSS JOIN pins
/// that order: driven from `threads` instead, it would probe `idx_file_edits_thread` once per
/// thread and read every thread row, subagents included.
///
/// A path still absolute after that lies outside the project (a Claude Code plan under
/// `~/.claude/plans`, a script in a temp folder) and would print the user's folders in a
/// report made to be shared: those rows are left out, and the last column counts them.
const FILES_SQL: &str = "SELECT rel, p, n, s, omitted FROM (
       SELECT rel, p, n, s, a, SUM(a) OVER () AS omitted FROM (
         SELECT rel, p, n, s,
                (rel LIKE '/%' OR rel LIKE '~%' OR substr(rel, 1, 1) = char(92)
                 OR substr(rel, 2, 1) = ':') AS a
         FROM (SELECT CASE WHEN p IS NOT NULL
                          AND substr(path, 1, length(p) + 1) IN (p || '/', p || char(92))
                          AND length(path) > length(p) + 1
                     THEN substr(path, length(p) + 2) ELSE path END AS rel,
                 p, COUNT(*) AS n, COUNT(DISTINCT thread_id) AS s
               FROM (SELECT fe.thread_id, fe.path, COALESCE(t.project_key, t.project_path) AS p
                     FROM file_edits fe CROSS JOIN threads t ON t.id = fe.thread_id
                     WHERE fe.op IN ('edit', 'write') AND COALESCE(fe.ts, 0) >= ?1)
               GROUP BY p, rel)))
     WHERE a = 0
     ORDER BY n DESC, rel
     LIMIT ?2";

/// The top files inside their projects, and how many files outside one were left out.
fn files(conn: &Connection, since: Option<i64>) -> Result<Option<(Vec<ReportFile>, i64)>> {
    if !crate::db::has_table(conn, "file_edits") {
        return Ok(None);
    }
    let mut stmt = conn.prepare(FILES_SQL)?;
    let mut omitted = 0i64;
    let rows = stmt
        .query_map(
            rusqlite::params![since.unwrap_or(0), TOP_FILES as i64],
            |r| {
                Ok((
                    ReportFile {
                        path: r.get(0)?,
                        project: r.get(1)?,
                        edits: r.get(2)?,
                        sessions: r.get(3)?,
                    },
                    r.get::<_, i64>(4)?,
                ))
            },
        )?
        .map(|row| {
            row.map(|(file, outside)| {
                omitted = outside;
                file
            })
        })
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok((!rows.is_empty()).then_some((rows, omitted)))
}

fn ledger_totals(conn: &Connection, since: Option<i64>) -> Result<Option<ReportLedger>> {
    let path = ledger::path_for_conn(conn);
    let kept = ledger::kept(conn, path.as_deref(), since_day(since).as_deref())?;
    // Events from before the window alone would read "memory injected 0 times, 0 ...".
    if kept.counts.total() == 0 {
        return Ok(None);
    }
    Ok(Some(ReportLedger {
        counts: kept.counts,
        since_day: kept.first_day,
        all_time: kept.all_time,
    }))
}

// ---------------------------------------------------------------------------------------
// Markdown: what `cal report` prints and the app's "Copy as markdown" copies.
// ---------------------------------------------------------------------------------------

/// `1234567` as `1,234,567`.
fn grouped(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 {
        out.insert(0, '-');
    }
    out
}

/// Dollars: cents under $1,000, whole dollars above.
fn usd(v: f64) -> String {
    if v >= 1000.0 {
        format!("${}", grouped(v.round() as i64))
    } else {
        format!("${v:.2}")
    }
}

fn pct(share: f64) -> String {
    let p = share * 100.0;
    if p > 0.0 && p < 1.0 {
        "<1%".into()
    } else {
        format!("{}%", p.round() as i64)
    }
}

fn date(ts: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|t| t.format("%b %-d, %Y").to_string())
        .unwrap_or_else(|| "an unknown date".into())
}

fn day_label(day: &str) -> String {
    day_start(day).map(date).unwrap_or_else(|| day.to_string())
}

fn plural(n: i64, one: &str, many: &str) -> String {
    format!("{} {}", grouped(n), if n == 1 { one } else { many })
}

/// `a`, `a and b`, `a, b and c`.
fn and_list(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// A table cell: one line, pipes escaped.
fn cell(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "\\|")
}

/// Inline code that survives a backtick in the text.
fn code(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.contains('`') {
        format!("`` {flat} ``")
    } else {
        format!("`{flat}`")
    }
}

fn project_name(project: Option<&str>) -> Option<&str> {
    project.map(project_label).filter(|p| !p.is_empty())
}

/// The report as markdown. Projects show by name, never by full path, since this is the
/// form people paste elsewhere.
pub fn to_markdown(r: &Report) -> String {
    let mut md = String::from("# Your agents, so far\n\n");
    if let Some(since) = r.since {
        let _ = writeln!(md, "_Sessions since {}._\n", date(since));
    }
    let Some(t) = &r.totals else {
        md.push_str("No agent sessions indexed yet.\n");
        footer(&mut md, r);
        return md;
    };
    let _ = write!(
        md,
        "**{}** and **{}** with your coding agents, {} to {}: {}",
        plural(t.sessions, "session", "sessions"),
        plural(t.messages, "message", "messages"),
        date(t.first_session),
        date(t.last_session),
        plural(t.active_days, "active day", "active days"),
    );
    let hours = plural(t.estimated_hours.round() as i64, "hour", "hours");
    if t.hours_sessions >= t.sessions {
        let _ = write!(md, ", about {hours}");
    } else if t.hours_sessions > 0 {
        // Most Cursor sessions carry no usable times: say which sessions the figure covers
        // rather than let it read as every agent's.
        let agents: Vec<&str> = t.hours_sources.iter().map(|k| source_label(k)).collect();
        let _ = write!(
            md,
            ", about {hours} in the {} with usable timestamps ({})",
            plural(t.hours_sessions, "session", "sessions"),
            and_list(&agents),
        );
    }
    md.push_str(".\n\n");
    md.push_str("| Agent | Sessions | Share |\n|---|---:|---:|\n");
    for tool in &t.by_tool {
        let _ = writeln!(
            md,
            "| {} | {} | {} |",
            cell(&tool.label),
            grouped(tool.sessions),
            pct(tool.share)
        );
    }

    if let Some(s) = &r.spend {
        md.push_str("\n## Spend (estimate)\n\n");
        let _ = writeln!(
            md,
            "Estimated spend: {} ({}), {} in the last 30 days.\n",
            usd(s.total.total_cost),
            plural(
                s.total.tracked_calls,
                "priced model call",
                "priced model calls"
            ),
            usd(s.last30_days.total_cost),
        );
        if !s.total.by_tool.is_empty() {
            md.push_str("| Agent | Spend | Last 30 days |\n|---|---:|---:|\n");
            for tool in &s.total.by_tool {
                let recent = s
                    .last30_days
                    .by_tool
                    .iter()
                    .find(|x| x.source == tool.source)
                    .map_or(0.0, |x| x.cost);
                let _ = writeln!(
                    md,
                    "| {} | {} | {} |",
                    cell(&tool.label),
                    usd(tool.cost),
                    usd(recent)
                );
            }
        }
        if !s.total.by_model.is_empty() {
            md.push_str("\nBy model:\n");
            for m in &s.total.by_model {
                let _ = writeln!(
                    md,
                    "- {}: {} ({})",
                    code(&m.model),
                    usd(m.cost),
                    plural(m.calls, "call", "calls")
                );
            }
        }
        if !s.priciest_commits.is_empty() {
            md.push_str("\nPriciest commits:\n");
            for c in &s.priciest_commits {
                let subject = c.subject.as_deref().map(|x| fact_line(x, 80));
                let _ = write!(md, "- {}", code(&c.short_sha));
                if let Some(subject) = subject.filter(|x| !x.is_empty()) {
                    let _ = write!(md, " {subject}");
                }
                if let Some(p) = project_name(c.project.as_deref()) {
                    let _ = write!(md, " ({p})");
                }
                let _ = writeln!(md, ": {}", usd(c.cost));
            }
        }
        let _ = writeln!(md, "\n_{}_", s.basis);
        if s.total.untracked_calls > 0 {
            let _ = writeln!(
                md,
                "_{} on unpriced models not counted._",
                plural(s.total.untracked_calls, "call", "calls")
            );
        }
    }

    if let Some(errors) = &r.recurring_errors {
        // The clusters never reach back past their window, whatever `since` asks for.
        let window = issues::CLUSTER_WINDOW_SECS;
        if r.since.is_none_or(|s| s < r.generated_at - window) {
            let _ = writeln!(
                md,
                "\n## Recurring errors (last {} days)\n",
                window / 86_400
            );
        } else {
            md.push_str("\n## Recurring errors\n\n");
        }
        for (i, e) in errors.iter().enumerate() {
            let _ = write!(
                md,
                "{}. {}: {} in {}, {} to {}",
                i + 1,
                code(&e.example),
                plural(e.count, "time", "times"),
                plural(e.sessions, "session", "sessions"),
                date(e.first_seen),
                date(e.last_seen),
            );
            if let Some(p) = project_name(e.project.as_deref()) {
                let _ = write!(md, " (latest in {p})");
            }
            match &e.fix {
                Some(f) => {
                    let _ = write!(md, ". Fixed in {}", code(&f.short_sha));
                    if let Some(subject) = f.subject.as_deref().filter(|x| !x.trim().is_empty()) {
                        let _ = write!(md, " \"{}\"", fact_line(subject, 80));
                    }
                    let _ = writeln!(md, ", session #{}.", f.thread_id);
                }
                None => md.push_str(". No linked fix yet.\n"),
            }
        }
    }

    if let Some(groups) = &r.solved_again {
        md.push_str("\n## Solved more than once\n\nSessions that opened with the same prompt on different days:\n\n");
        for (i, g) in groups.iter().enumerate() {
            let _ = write!(
                md,
                "{}. \"{}\": {}",
                i + 1,
                g.prompt.replace('"', "'"),
                plural(g.sessions, "session", "sessions"),
            );
            if let Some(p) = project_name(g.project.as_deref()) {
                let _ = write!(md, " in {p}");
            }
            let _ = writeln!(md, ", {} to {}", date(g.first_seen), date(g.last_seen));
        }
    }

    if let Some(m) = &r.memory {
        md.push_str("\n## Memory\n\n");
        // The section can stand on injections alone; "0 decisions, 0 gotchas" says nothing.
        if m.decisions + m.gotchas + m.open_todos > 0 {
            let _ = writeln!(
                md,
                "{}, {} and {} across {}.",
                plural(m.decisions, "decision", "decisions"),
                plural(m.gotchas, "gotcha", "gotchas"),
                plural(m.open_todos, "open TODO", "open TODOs"),
                plural(m.projects_with_memory, "project", "projects"),
            );
        }
        if !m.top_decision_projects.is_empty() {
            let list: Vec<String> = m
                .top_decision_projects
                .iter()
                .map(|p| format!("{} ({})", project_label(&p.project), grouped(p.decisions)))
                .collect();
            let _ = writeln!(md, "Most decisions: {}.", list.join(", "));
        }
        if let Some(inj) = &m.injected {
            let _ = writeln!(
                md,
                "Memory was injected at the start of {} of sessions since {} ({}, {}).",
                pct(inj.share),
                day_label(&inj.since_day),
                plural(inj.injections as i64, "injection", "injections"),
                plural(inj.sessions, "session", "sessions"),
            );
        }
    }

    if let Some(files) = &r.files {
        md.push_str("\n## Files agents changed most\n\n| File | Project | Edits | Sessions |\n|---|---|---:|---:|\n");
        for f in files {
            let _ = writeln!(
                md,
                "| {} | {} | {} | {} |",
                cell(&f.path),
                cell(project_name(f.project.as_deref()).unwrap_or("")),
                grouped(f.edits),
                grouped(f.sessions)
            );
        }
        if let Some(n) = r.files_omitted {
            let _ = writeln!(
                md,
                "\n_{} outside a project folder not shown._",
                plural(n, "file", "files")
            );
        }
    }

    if let Some(l) = &r.ledger {
        md.push_str("\n## What Callimachus did for your agents\n\n");
        let c = &l.counts;
        let since = l
            .since_day
            .as_deref()
            .map(|d| format!("Since {}", day_label(d)))
            .unwrap_or_else(|| "Recently".into());
        let _ = writeln!(
            md,
            "{since}: memory injected {}, {}, {}, {} and {}. {} in all, counted on this machine only.",
            plural(c.hook as i64, "time", "times"),
            plural(c.digest as i64, "session digest", "session digests"),
            plural(c.recall as i64, "recall hit", "recall hits"),
            plural(c.playbook as i64, "error playbook", "error playbooks"),
            plural(c.mcp as i64, "MCP tool call", "MCP tool calls"),
            grouped(l.all_time as i64),
        );
    }
    footer(&mut md, r);
    md
}

fn footer(md: &mut String, r: &Report) {
    let _ = writeln!(
        md,
        "\n---\n_Made with Callimachus ({SITE}) on {}, from the local index._",
        date(r.generated_at)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(tag: &str) -> Connection {
        let dir = std::env::temp_dir().join(format!("calli_report_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        crate::db::open(&dir.join("index.db")).unwrap()
    }

    fn source(conn: &Connection, kind: &str) -> i64 {
        conn.execute("INSERT OR IGNORE INTO sources (kind) VALUES (?1)", [kind])
            .unwrap();
        conn.query_row("SELECT id FROM sources WHERE kind = ?1", [kind], |r| {
            r.get(0)
        })
        .unwrap()
    }

    struct Seed<'a> {
        conn: &'a Connection,
        n: i64,
    }

    impl Seed<'_> {
        /// A session with one user prompt (and an optional harness turn before it).
        fn session(
            &mut self,
            kind: &str,
            project: &str,
            start: i64,
            span: i64,
            prompt: &str,
        ) -> i64 {
            let sid = source(self.conn, kind);
            self.n += 1;
            self.conn
                .execute(
                    "INSERT INTO threads (source_id, external_id, title, project_path, project_key,
                         created_at, updated_at, message_count)
                     VALUES (?1, ?2, 't', ?3, ?3, ?4, ?5, 2)",
                    rusqlite::params![sid, format!("s{}", self.n), project, start, start + span],
                )
                .unwrap();
            let id = self.conn.last_insert_rowid();
            self.conn
                .execute(
                    "INSERT INTO messages (thread_id, seq, role, text, ts) VALUES
                       (?1, 0, 'user', '<local-command-caveat>Caveat: x</local-command-caveat>', ?2),
                       (?1, 1, 'user', ?3, ?2)",
                    rusqlite::params![id, start, prompt],
                )
                .unwrap();
            id
        }
    }

    const NOW: i64 = 1_800_000_000;
    const DAY: i64 = 86_400;

    #[test]
    fn an_empty_index_has_no_sections_and_a_short_markdown() {
        let conn = temp_db("empty");
        let r = build(&conn, None, NOW).unwrap();
        assert!(r.totals.is_none() && r.spend.is_none() && r.recurring_errors.is_none());
        assert!(r.solved_again.is_none() && r.memory.is_none() && r.files.is_none());
        assert!(r.ledger.is_none());
        let json = serde_json::to_value(&r).unwrap();
        // Empty sections are omitted, not null.
        assert_eq!(
            json.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["generatedAt", "since"]
        );
        let md = to_markdown(&r);
        assert!(md.contains("No agent sessions indexed yet."), "{md}");
        assert!(md.contains(SITE));
    }

    #[test]
    fn totals_cap_spans_count_days_and_leave_out_chats_and_subagents() {
        let conn = temp_db("totals");
        let mut s = Seed { conn: &conn, n: 0 };
        let a = s.session(
            "claude_code",
            "/r/app",
            NOW - 10 * DAY,
            3600,
            "fix the build",
        );
        s.session("claude_code", "/r/app", NOW - 9 * DAY, 3 * DAY, "long one");
        s.session("cursor", "/r/web", NOW - 2 * DAY, 1800, "style it");
        s.session("in_app", "/r/app", NOW - DAY, 60, "a chat");
        // A subagent of the first session: its messages count, it is no session.
        conn.execute(
            "INSERT INTO threads (source_id, external_id, created_at, updated_at, message_count, is_subagent)
             SELECT source_id, 'sub', created_at, updated_at, 5, 1 FROM threads WHERE id = ?1",
            [a],
        )
        .unwrap();
        let t = build(&conn, None, NOW).unwrap().totals.unwrap();
        assert_eq!(t.sessions, 3);
        assert_eq!(t.messages, 2 * 3 + 5);
        // 1 h + 8 h (capped from 3 days) + 0.5 h.
        assert!(
            (t.estimated_hours - 9.5).abs() < 1e-9,
            "{}",
            t.estimated_hours
        );
        assert_eq!(t.hours_sessions, 3);
        assert_eq!(t.hours_sources, ["claude_code", "cursor"]);
        assert_eq!(t.first_session, NOW - 10 * DAY);
        assert_eq!(t.last_session, NOW - 2 * DAY + 1800);
        assert!(
            t.active_days >= 4 && t.active_days <= 6,
            "{}",
            t.active_days
        );
        let tools: Vec<(&str, i64)> = t
            .by_tool
            .iter()
            .map(|x| (x.source.as_str(), x.sessions))
            .collect();
        assert_eq!(tools, [("claude_code", 2), ("cursor", 1)]);
        assert!((t.by_tool[0].share - 2.0 / 3.0).abs() < 1e-9);
        // A window keeps the sessions started in it.
        let recent = build(&conn, Some(NOW - 5 * DAY), NOW)
            .unwrap()
            .totals
            .unwrap();
        assert_eq!(recent.sessions, 1);
    }

    /// A Cursor session: `updated_at` equal to `created_at`, as Cursor writes almost every
    /// one, and two messages at `ts`.
    fn cursor_session(conn: &Connection, ext: &str, start: i64, ts: [Option<i64>; 2]) -> i64 {
        let sid = source(conn, "cursor");
        conn.execute(
            "INSERT INTO threads (source_id, external_id, project_path, created_at, updated_at, message_count)
             VALUES (?1, ?2, '/r/web', ?3, ?3, 2)",
            rusqlite::params![sid, ext, start],
        )
        .unwrap();
        let id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO messages (thread_id, seq, role, text, ts) VALUES
               (?1, 0, 'user', 'go', ?2), (?1, 1, 'assistant', 'ok', ?3)",
            rusqlite::params![id, ts[0], ts[1]],
        )
        .unwrap();
        id
    }

    #[test]
    fn hours_fall_back_to_message_times_and_name_the_sessions_they_cover() {
        let conn = temp_db("hours");
        let mut s = Seed { conn: &conn, n: 0 };
        s.session("claude_code", "/r/app", NOW - 3 * DAY, 3600, "x");
        // The thread's own times give no span; its messages span 20 minutes.
        cursor_session(
            &conn,
            "c1",
            NOW - 2 * DAY,
            [Some(NOW - 2 * DAY), Some(NOW - 2 * DAY + 1200)],
        );
        // No usable time at all: left out of the hours, not counted as zero.
        cursor_session(&conn, "c2", NOW - DAY, [None, None]);
        let r = build(&conn, None, NOW).unwrap();
        let t = r.totals.as_ref().unwrap();
        assert_eq!(t.sessions, 3);
        assert_eq!(t.hours_sessions, 2);
        assert_eq!(t.hours_sources, ["cursor", "claude_code"]);
        assert!((t.estimated_hours - (3600.0 + 1200.0) / 3600.0).abs() < 1e-9);
        let md = to_markdown(&r);
        assert!(
            md.contains(
                "about 1 hour in the 2 sessions with usable timestamps (Cursor and Claude Code)."
            ),
            "{md}"
        );

        // With every session timed, the figure needs no label.
        let conn = temp_db("hours_all");
        let mut s = Seed { conn: &conn, n: 0 };
        s.session("claude_code", "/r/app", NOW - 3 * DAY, 7200, "x");
        let md = to_markdown(&build(&conn, None, NOW).unwrap());
        assert!(md.contains(", about 2 hours.\n"), "{md}");
    }

    #[test]
    fn the_same_first_prompt_on_different_days_groups_once_the_index_is_big_enough() {
        let conn = temp_db("prompts");
        let mut s = Seed { conn: &conn, n: 0 };
        let ask =
            "Please fix the flaky login test in the auth suite again today, see src/auth.rs:12";
        let a = s.session("claude_code", "/r/app", NOW - 20 * DAY, 60, ask);
        // Numbers, folders and punctuation differ; the file and the task are the same.
        let b = s.session(
            "codex",
            "/r/app",
            NOW - 3 * DAY,
            60,
            "please fix the FLAKY login test, in the auth suite (again) today: see lib/auth.rs:42",
        );
        // Same prompt within the hour: the same attempt, not solved again.
        s.session(
            "claude_code",
            "/r/web",
            NOW - DAY,
            60,
            "rename every handler in the web routes folder to match the new style",
        );
        s.session(
            "claude_code",
            "/r/web",
            NOW - DAY + 600,
            60,
            "rename every handler in the web routes folder to match the new style",
        );
        // Too short to call the same task.
        s.session("claude_code", "/r/app", NOW - 30 * DAY, 60, "continue");
        s.session("claude_code", "/r/app", NOW - 2 * DAY, 60, "continue");
        assert!(
            build(&conn, None, NOW).unwrap().solved_again.is_none(),
            "under 50 sessions the section is skipped"
        );
        for i in 0..50 {
            s.session(
                "cursor",
                "/r/other",
                NOW - 40 * DAY + i,
                60,
                &format!("unique prompt number {i} about thing {i} here"),
            );
        }
        let groups = build(&conn, None, NOW).unwrap().solved_again.unwrap();
        assert_eq!(groups.len(), 1, "{groups:?}");
        let g = &groups[0];
        assert_eq!(g.sessions, 2);
        assert_eq!(g.thread_ids, [b, a], "newest first");
        assert_eq!(g.project.as_deref(), Some("/r/app"));
        assert!(g.prompt.starts_with("please fix the FLAKY"), "{}", g.prompt);
        assert_eq!((g.first_seen, g.last_seen), (NOW - 20 * DAY, NOW - 3 * DAY));
    }

    #[test]
    fn tool_written_openers_are_not_the_users_prompt() {
        let conn = temp_db("tool_prompts");
        let mut s = Seed { conn: &conn, n: 0 };
        let plan = "Implement the plan as specified, it is attached for your reference. Do NOT edit the plan file itself.";
        s.session("cursor", "/r/app", NOW - 20 * DAY, 60, plan);
        s.session("cursor", "/r/app", NOW - 2 * DAY, 60, plan);
        for i in 0..50 {
            s.session(
                "cursor",
                "/r/o",
                NOW - 40 * DAY + i,
                60,
                &format!("filler {i}"),
            );
        }
        assert!(build(&conn, None, NOW).unwrap().solved_again.is_none());
    }

    #[test]
    fn normalizing_keeps_a_placeholder_for_numbers_and_the_file_for_paths() {
        assert_eq!(
            normalize_prompt("Fix bug #42 in src/a.rs: it's   BROKEN, again and again now!"),
            Some("fix bug # in a.rs its broken again and again now".into())
        );
        assert_eq!(normalize_prompt("too short to count"), None);
        // Paths and numbers do not make up the eight words.
        assert_eq!(
            normalize_prompt("fix src/a.rs src/b.rs 1 2 3 4 5 6 7 8 in it"),
            None
        );
    }

    /// Two pairs that grouped on a real index although each was two different problems: the
    /// template of Cursor's quick fix supplied the words, and the paths that told them apart
    /// were dropped.
    #[test]
    fn quick_fix_templates_and_different_modules_are_not_the_same_task() {
        let quick_fix = |error: &str| {
            format!(
                "For the code present, we get this error:\n```\n{error}\n```\nHow can I resolve this? If you propose a fix, please make it concise."
            )
        };
        let conn = temp_db("quick_fix");
        let mut s = Seed { conn: &conn, n: 0 };
        for (start, error) in [
            (NOW - 30 * DAY, "Cannot find module '@/app/actions/get-pet-details' or its corresponding type declarations."),
            (NOW - 5 * DAY, "Cannot find module '../shelter/_components/shelter-sidebar' or its corresponding type declarations."),
            (NOW - 20 * DAY, "Expected 2 arguments, but got 1."),
            (NOW - 2 * DAY, "Expected 2 arguments, but got 1."),
        ] {
            s.session("cursor", "/r/web", start, 60, &quick_fix(error));
        }
        for i in 0..50 {
            s.session(
                "cursor",
                "/r/o",
                NOW - 40 * DAY + i,
                60,
                &format!("filler {i}"),
            );
        }
        assert!(build(&conn, None, NOW).unwrap().solved_again.is_none());
        // Typed by hand, the module still tells them apart.
        let typed = |m: &str| {
            normalize_prompt(&format!(
                "Cannot find module '{m}' or its corresponding type declarations, please fix"
            ))
        };
        assert_ne!(
            typed("@/app/actions/get-pet-details"),
            typed("../shelter/_components/shelter-sidebar")
        );
        assert!(typed("@/app/actions/get-pet-details").is_some());
    }

    #[test]
    fn sessions_on_the_same_day_count_as_one_attempt() {
        let conn = temp_db("same_day");
        let mut s = Seed { conn: &conn, n: 0 };
        let ask = "rename every handler in the web routes folder to match the new style";
        let week_ago = NOW - 7 * DAY;
        let first = s.session("claude_code", "/r/web", week_ago, 60, ask);
        s.session("claude_code", "/r/web", week_ago + 3600, 60, ask);
        s.session("claude_code", "/r/web", week_ago + 7200, 60, ask);
        let newest = s.session("claude_code", "/r/web", NOW - DAY, 60, ask);
        for i in 0..50 {
            s.session(
                "cursor",
                "/r/o",
                NOW - 40 * DAY + i,
                60,
                &format!("filler {i}"),
            );
        }
        let groups = build(&conn, None, NOW).unwrap().solved_again.unwrap();
        assert_eq!(groups[0].sessions, 2, "{groups:?}");
        assert_eq!(groups[0].thread_ids[0], newest);
        assert_eq!(groups[0].first_seen, week_ago);
        assert!(
            !groups[0].thread_ids.contains(&first),
            "the afternoon's last try stands for it"
        );
    }

    #[test]
    fn a_prompt_with_a_secret_is_redacted() {
        let conn = temp_db("redact");
        let mut s = Seed { conn: &conn, n: 0 };
        let ask = "deploy with key sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA to the staging cluster now";
        s.session("claude_code", "/r/app", NOW - 20 * DAY, 60, ask);
        s.session("claude_code", "/r/app", NOW - 2 * DAY, 60, ask);
        for i in 0..50 {
            s.session(
                "cursor",
                "/r/o",
                NOW - 40 * DAY + i,
                60,
                &format!("filler {i}"),
            );
        }
        let r = build(&conn, None, NOW).unwrap();
        let groups = r.solved_again.as_ref().unwrap();
        assert!(
            !groups[0].prompt.contains("sk-ant-api03"),
            "{}",
            groups[0].prompt
        );
        assert!(!to_markdown(&r).contains("sk-ant-api03"));
    }

    #[test]
    fn memory_counts_visible_facts_and_ranks_projects_by_decisions() {
        let conn = temp_db("memory");
        let mut s = Seed { conn: &conn, n: 0 };
        let app = s.session("claude_code", "/r/app", NOW - DAY, 60, "x");
        let web = s.session("claude_code", "/r/web", NOW - DAY, 60, "y");
        for (thread, kind, status, hidden) in [
            (app, "decision", "open", 0),
            (app, "decision", "open", 0),
            (app, "gotcha", "open", 0),
            (app, "decision", "open", 1),
            (web, "gotcha", "open", 0),
            (web, "todo", "open", 0),
            (web, "todo", "done", 0),
            (web, "summary", "open", 0),
        ] {
            conn.execute(
                "INSERT INTO facts (thread_id, kind, text, status, hidden, created_at)
                 VALUES (?1, ?2, 'f', ?3, ?4, ?5)",
                rusqlite::params![thread, kind, status, hidden, NOW - DAY],
            )
            .unwrap();
        }
        let m = build(&conn, None, NOW).unwrap().memory.unwrap();
        assert_eq!((m.decisions, m.gotchas, m.open_todos), (2, 2, 1));
        assert_eq!(m.projects_with_memory, 2);
        assert_eq!(m.top_decision_projects.len(), 1);
        assert_eq!(m.top_decision_projects[0].project, "/r/app");
        assert!(m.injected.is_none(), "no ledger yet");
    }

    #[test]
    fn files_rank_edits_and_writes_relative_to_the_project() {
        let conn = temp_db("files");
        let mut s = Seed { conn: &conn, n: 0 };
        let a = s.session("claude_code", "/r/app", NOW - DAY, 60, "x");
        let b = s.session("codex", "/r/app", NOW - DAY, 60, "y");
        for (thread, path, op) in [
            (a, "src/lib.rs", "edit"),
            (a, "src/lib.rs", "edit"),
            (b, "/r/app/src/lib.rs", "write"),
            (b, "src/lib.rs", "write"),
            (a, "README.md", "edit"),
            (a, "/elsewhere/x.rs", "edit"),
            (b, "/r/app/", "write"),
            (a, "src/main.rs", "read"),
            (a, "src/main.rs", "mention"),
        ] {
            conn.execute(
                "INSERT INTO file_edits (thread_id, path, op, ts) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![thread, path, op, NOW - DAY],
            )
            .unwrap();
        }
        let files = build(&conn, None, NOW).unwrap().files.unwrap();
        let got: Vec<(&str, i64, i64)> = files
            .iter()
            .map(|f| (f.path.as_str(), f.edits, f.sessions))
            .collect();
        // The absolute spelling counts with the relative one; a path outside the project and
        // the project root itself are left out, and counted.
        assert_eq!(got, [("src/lib.rs", 4, 2), ("README.md", 1, 1)]);
        assert!(files.iter().all(|f| f.project.as_deref() == Some("/r/app")));
        let r = build(&conn, None, NOW).unwrap();
        assert_eq!(r.files_omitted, Some(2));
        let md = to_markdown(&r);
        assert!(
            md.contains("_2 files outside a project folder not shown._"),
            "{md}"
        );
        assert!(!md.contains("/elsewhere"), "{md}");
    }

    #[test]
    fn the_ledger_sets_the_injected_share() {
        let conn = temp_db("ledger");
        let mut s = Seed { conn: &conn, n: 0 };
        let now = chrono::Utc::now().timestamp();
        for i in 0..4 {
            s.session("claude_code", "/r/app", now - i, 60, "x");
        }
        let path = ledger::path_for_conn(&conn).unwrap();
        ledger::append_to(&path, ledger::Kind::Hook, "/r/app", now);
        ledger::append_to(&path, ledger::Kind::Mcp, "search_threads", now);
        let r = build(&conn, None, now).unwrap();
        let l = r.ledger.as_ref().unwrap();
        assert_eq!((l.counts.hook, l.counts.mcp, l.all_time), (1, 1, 2));
        let inj = r.memory.as_ref().unwrap().injected.as_ref().unwrap();
        assert_eq!(inj.injections, 1);
        assert_eq!(inj.sessions, 4, "the sessions since local midnight");
        assert!((inj.share - 0.25).abs() < 1e-9, "{}", inj.share);
        let md = to_markdown(&r);
        assert!(md.contains("Memory was injected at the start of"), "{md}");
        // No facts: the memory section stands on the injections alone, with no zero line.
        assert!(!md.contains("0 decisions"), "{md}");

        // Events only from before the window: the ledger section would read all zero.
        let windowed = build(&conn, Some(now + 2 * DAY), now + 2 * DAY).unwrap();
        assert!(windowed.ledger.is_none());
    }

    /// Plain `cal report --json` is an unversioned shape: hooks that gave up (in the skips
    /// file, or folded into the index by a rollup) leave it byte for byte as it was.
    #[test]
    fn hook_skips_leave_the_report_json_alone() {
        let dir = crate::db::test_support::TestDir::new("report_skips");
        let conn = crate::db::open(&dir).unwrap();
        let mut s = Seed { conn: &conn, n: 0 };
        let now = chrono::Utc::now().timestamp();
        s.session("claude_code", "/r/app", now - 3 * DAY, 60, "x");
        let path = ledger::path_for_conn(&conn).unwrap();
        ledger::append_to(&path, ledger::Kind::Hook, "/r/app", now - 3 * DAY);
        let json = |conn: &Connection| serde_json::to_string(&build(conn, None, now).unwrap());
        let before = json(&conn).unwrap();
        assert!(
            before.contains(r#""counts":{"hook":1,"mcp":0,"recall":0,"digest":0,"playbook":0}"#)
        );

        let skips = ledger::skips_path_for(&dir).unwrap();
        for at in [now - 2 * DAY, now] {
            ledger::append_to(&skips, ledger::Kind::HookSkipped, "schema_behind hook", at);
        }
        assert_eq!(json(&conn).unwrap(), before, "skips in the file");
        ledger::rollup(&conn).unwrap();
        assert_eq!(json(&conn).unwrap(), before, "and folded by a rollup");
    }

    #[test]
    fn unpriced_calls_alone_leave_spend_out() {
        let conn = temp_db("unpriced");
        let mut s = Seed { conn: &conn, n: 0 };
        let a = s.session("codex", "/r/app", NOW - DAY, 60, "x");
        conn.execute(
            "INSERT INTO messages (thread_id, seq, role, text, ts, model, input_tokens, output_tokens)
             VALUES (?1, 5, 'assistant', 'ok', ?2, 'no-such-model-9', 1000, 100)",
            rusqlite::params![a, NOW - DAY],
        )
        .unwrap();
        let r = build(&conn, None, NOW).unwrap();
        assert!(r.spend.is_none(), "{:?}", r.spend);
        assert!(!to_markdown(&r).contains("$0.00"));
    }

    #[test]
    fn error_examples_come_from_tool_output_with_paths_cut_to_file_names() {
        let conn = temp_db("errors");
        let mut s = Seed { conn: &conn, n: 0 };
        let a = s.session("claude_code", "/Users/me/app", NOW - 5 * DAY, 60, "x");
        let b = s.session("claude_code", "/Users/me/app", NOW - 4 * DAY, 60, "y");
        let mut seq = 10;
        let mut add = |thread: i64, role: &str, text: &str, ts: i64| {
            seq += 1;
            conn.execute(
                "INSERT INTO messages (thread_id, seq, role, text, ts) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![thread, seq, role, text, ts],
            )
            .unwrap();
        };
        // Pasted by the user only: never quoted.
        add(
            a,
            "user",
            "TypeError: secretive pasted thing failed badly",
            NOW - 5 * DAY,
        );
        add(
            b,
            "user",
            "TypeError: secretive pasted thing failed badly",
            NOW - 4 * DAY,
        );
        // Tool output first, then the newest sighting pasted by the user.
        let tool_line = "Error: Cannot find module '/Users/me/scratch/orchard.mjs'";
        add(a, "tool", tool_line, NOW - 5 * DAY + 10);
        add(b, "tool", tool_line, NOW - 4 * DAY + 10);
        add(
            b,
            "user",
            "Error: Cannot find module '/Users/me/other/orchard.mjs'",
            NOW - 4 * DAY + 20,
        );
        let r = build(&conn, None, NOW).unwrap();
        let errors = r.recurring_errors.as_ref().unwrap();
        let examples: Vec<&str> = errors.iter().map(|e| e.example.as_str()).collect();
        assert_eq!(examples, ["Error: Cannot find module 'orchard.mjs'"]);
        let md = to_markdown(&r);
        assert!(md.contains("## Recurring errors (last 180 days)"), "{md}");
        assert!(
            !md.contains("/Users/me") && !md.contains("secretive"),
            "{md}"
        );
    }

    #[test]
    fn absolute_paths_shorten_to_their_last_component() {
        for (line, want) in [
            (
                "Error: Cannot find module '/private/tmp/claude-501/-Users-me/x/orchard.mjs'",
                "Error: Cannot find module 'orchard.mjs'",
            ),
            ("at f (/Users/me/app/src/a.js:1:2)", "at f (a.js:1:2)"),
            ("open ~/.claude/plans/x.md failed", "open x.md failed"),
            (
                r"error: C:\Users\me\app\main.rs not found",
                "error: main.rs not found",
            ),
            ("see file:///Users/me/a.html", "see a.html"),
            ("--prefix=/Users/me/lib/", "--prefix=lib"),
            // Relative paths and URLs name no one's folders.
            (
                "error in src/lib.rs and ../x/y.ts",
                "error in src/lib.rs and ../x/y.ts",
            ),
            (
                "fetch https://example.com/a/b failed",
                "fetch https://example.com/a/b failed",
            ),
        ] {
            assert_eq!(shorten_paths(line), want, "{line}");
        }
    }

    #[test]
    fn markdown_carries_every_section_with_project_names_not_paths() {
        let conn = temp_db("markdown");
        let mut s = Seed { conn: &conn, n: 0 };
        let a = s.session("claude_code", "/Users/me/r/app", NOW - 3 * DAY, 3600, "x");
        conn.execute(
            "INSERT INTO file_edits (thread_id, path, op, ts) VALUES (?1, 'src/a|b.rs', 'edit', ?2)",
            rusqlite::params![a, NOW - DAY],
        )
        .unwrap();
        let md = to_markdown(&build(&conn, None, NOW).unwrap());
        assert!(md.starts_with("# Your agents, so far\n"), "{md}");
        assert!(md.contains("**1 session**"), "{md}");
        assert!(md.contains("| Claude Code | 1 | 100% |"), "{md}");
        assert!(md.contains("## Files agents changed most"), "{md}");
        assert!(md.contains("src/a\\|b.rs | app |"), "{md}");
        assert!(!md.contains("/Users/me"), "{md}");
        assert!(!md.contains('\u{2014}'), "no em-dash");
    }

    #[test]
    fn number_and_money_formats() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(1234567), "1,234,567");
        assert_eq!(grouped(-1200), "-1,200");
        assert_eq!(usd(12.345), "$12.35");
        assert_eq!(usd(12345.6), "$12,346");
        assert_eq!(pct(0.004), "<1%");
        assert_eq!(pct(0.666), "67%");
    }

    fn plan(conn: &Connection, sql: &str, args: &[&dyn rusqlite::ToSql]) -> String {
        conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map(args, |r| r.get::<_, String>(3))
            .unwrap()
            .map(|r| r.unwrap())
            .collect::<Vec<_>>()
            .join(" | ")
    }

    #[test]
    fn query_plans_stay_on_their_indexes() {
        let conn = temp_db("plans");
        conn.execute_batch("ANALYZE").unwrap();
        // One short range of a session's messages, never a scan of the table.
        let p = plan(&conn, FIRST_PROMPTS_SQL, &[&1i64, &2000i64, &3i64]);
        assert!(p.contains("SEARCH m USING"), "{p}");
        assert!(!p.contains("SCAN m"), "{p}");
        // A session's message times: index probes, never its message rows.
        let p = plan(&conn, SESSIONS_SQL, &[&0i64]);
        assert!(p.contains("COVERING INDEX idx_messages_thread_ts"), "{p}");
        assert!(!p.contains("SCAN m"), "{p}");
        // The stored example's own rows: one (thread, ts) range.
        let p = plan(&conn, EXAMPLE_FROM_TOOL_SQL, &[&1i64, &2i64, &"x"]);
        assert!(
            p.contains("SEARCH m USING INDEX idx_messages_thread_ts"),
            "{p}"
        );
        // file_edits read once, each row's thread by primary key.
        let p = plan(&conn, FILES_SQL, &[&0i64, &10i64]);
        assert!(p.contains("SCAN fe"), "{p}");
        assert!(p.contains("SEARCH t USING INTEGER PRIMARY KEY"), "{p}");
        assert!(!p.contains("SCAN t"), "{p}");
    }
}
