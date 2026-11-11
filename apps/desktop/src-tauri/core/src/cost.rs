//! Spend layer: turn the captured per-message token usage (migration 0022) into dollars. The
//! index is the only place that sees model spend across every tool, so this is the one view that
//! can answer "what did this actually cost me?". Prices are approximate published list rates
//! ($ per million tokens, `core/prices.json`) and are clearly an estimate, not a billing
//! record: for a subscription they are the list-price equivalent of the usage.
//!
//! [`cost_report`] breaks the same usage down by session, commit, project or local day
//! (`cal cost --by`), with neutral waste signals per session.

use crate::search::ProjectMatch;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, ToSql};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Published list price for a model, in USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPrice {
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub cache_read: f64,
}

/// One entry of `core/prices.json` (its `about` field explains the format).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PriceEntry {
    family: String,
    #[serde(default)]
    equals: Vec<String>,
    #[serde(default)]
    prefix: Vec<String>,
    #[serde(default)]
    contains: Vec<String>,
    /// Words that take an id out of this family even when a pattern matched: a variant the
    /// provider prices differently (`gemini-3.8-flash-lite` is not `gemini-3.8-flash`).
    #[serde(default)]
    except: Vec<String>,
    input: Option<f64>,
    output: Option<f64>,
    cache_write: Option<f64>,
    cache_read: Option<f64>,
    /// Set instead of the prices for a model we deliberately leave unpriced: why.
    unpriced: Option<String>,
}

impl PriceEntry {
    /// Whether `id` (lowercased) is this family. `equals` compares the id's last path
    /// segment too, so a provider-prefixed id (`copilot/auto`) matches its bare form.
    fn matches(&self, id: &str) -> bool {
        let base = id.rsplit('/').next().unwrap_or(id);
        if self.except.iter().any(|x| id.contains(x.as_str())) {
            return false;
        }
        self.equals.iter().any(|e| e == id || e == base)
            || self
                .prefix
                .iter()
                .any(|p| id.starts_with(p.as_str()) || base.starts_with(p.as_str()))
            || self.contains.iter().any(|c| id.contains(c.as_str()))
    }

    fn price(&self) -> Option<ModelPrice> {
        Some(ModelPrice {
            input: self.input?,
            output: self.output?,
            cache_write: self.cache_write?,
            cache_read: self.cache_read?,
        })
    }
}

#[derive(Deserialize)]
struct PriceFile {
    models: Vec<PriceEntry>,
}

/// The price table, `core/prices.json`, embedded at build time so a price change is a
/// reviewed data diff rather than code. A test parses it and checks every entry.
static PRICES: std::sync::LazyLock<Vec<PriceEntry>> = std::sync::LazyLock::new(|| {
    serde_json::from_str::<PriceFile>(include_str!("../prices.json"))
        .map(|f| f.models)
        .unwrap_or_default()
});

/// What the price table says about a model id.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pricing {
    /// A family with list prices.
    Priced {
        family: &'static str,
        price: ModelPrice,
    },
    /// A family we deliberately leave unpriced, and why (shown to the user).
    Unpriced {
        family: &'static str,
        why: &'static str,
    },
    /// No entry matches: counted, but with no price on file.
    Unknown,
}

/// Look a model id up in the price table, matched by family (ids vary by date and suffix).
pub fn pricing(model: &str) -> Pricing {
    let id = model.trim().to_ascii_lowercase();
    let Some(e) = PRICES.iter().find(|e| e.matches(&id)) else {
        return Pricing::Unknown;
    };
    match (e.price(), e.unpriced.as_deref()) {
        (Some(price), None) => Pricing::Priced {
            family: e.family.as_str(),
            price,
        },
        (_, Some(why)) => Pricing::Unpriced {
            family: e.family.as_str(),
            why,
        },
        (None, None) => Pricing::Unknown,
    }
}

/// Approximate $/Mtok for a model id. `None` = we don't have a price, so its tokens are
/// counted as "untracked" rather than guessed.
pub fn price_for(model: &str) -> Option<ModelPrice> {
    match pricing(model) {
        Pricing::Priced { price, .. } => Some(price),
        _ => None,
    }
}

/// Cost in USD for a token breakdown at a given price.
pub fn cost_of(input: i64, output: i64, cache_write: i64, cache_read: i64, p: &ModelPrice) -> f64 {
    (input as f64 * p.input
        + output as f64 * p.output
        + cache_write as f64 * p.cache_write
        + cache_read as f64 * p.cache_read)
        / 1_000_000.0
}

/// Spend for one model.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSpend {
    pub model: String,
    pub cost: f64,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub calls: i64,
    /// False when we have no price for the model (cost is 0 and it's flagged untracked).
    pub priced: bool,
}

/// One of the most expensive threads.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadCost {
    pub thread_id: i64,
    pub title: Option<String>,
    pub project: Option<String>,
    pub cost: f64,
}

/// The cost x-ray: total spend, a per-model breakdown, and the priciest threads.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Spend {
    pub total_cost: f64,
    pub tracked_calls: i64,
    pub untracked_calls: i64,
    pub by_model: Vec<ModelSpend>,
    pub top_threads: Vec<ThreadCost>,
}

/// Cache key: (since, project filter, top_n).
type SpendKey = (i64, Option<String>, usize);

static SPEND_CACHE: std::sync::LazyLock<crate::search::cache::AggCache<SpendKey, Spend>> =
    std::sync::LazyLock::new(crate::search::cache::AggCache::new);

/// [`spend`], reused for up to 10 minutes while the thread set is unchanged (see
/// `search::cache`), with `project` an exact key ([`ProjectMatch::Exact`]). For the app's
/// Stats view and Project page; `cal spend` and tests read live.
pub fn spend_cached(
    conn: &Connection,
    since: i64,
    project: Option<&str>,
    top_n: usize,
) -> Result<Spend> {
    let key = (
        since,
        project.filter(|p| !p.is_empty()).map(str::to_string),
        top_n,
    );
    SPEND_CACHE.get_or_try(crate::search::cache::index_version(conn)?, key, || {
        spend_matching(conn, since, project, ProjectMatch::Exact, top_n)
    })
}

/// Token totals for one (thread, model) pair.
struct Usage {
    thread_id: i64,
    model: String,
    input: i64,
    output: i64,
    cache_write: i64,
    cache_read: i64,
    calls: i64,
}

/// Compute spend across messages with captured usage since `since` (epoch secs; 0 = all
/// time), optionally scoped to a project-path substring (`cal spend -p`).
pub fn spend(conn: &Connection, since: i64, project: Option<&str>, top_n: usize) -> Result<Spend> {
    spend_matching(conn, since, project, ProjectMatch::Substring, top_n)
}

/// [`spend`] with the project matched as `mode` says.
///
/// One aggregate pass per (thread, model), served by the covering usage index (migration
/// 0026), then rolled up per model and per thread in Rust. This used to be two separate
/// GROUP BY scans that each read every message's trailing usage columns.
pub fn spend_matching(
    conn: &Connection,
    since: i64,
    project: Option<&str>,
    mode: ProjectMatch,
    top_n: usize,
) -> Result<Spend> {
    let (sql, args) = usage_sql(since, project, mode);
    let mut rows = usage_by_thread_model(conn, &sql, &args)?;
    copilot_cli_calls(conn, since, project, mode, &mut rows)?;

    let sum = summarize(rows);
    let per_thread: HashMap<i64, f64> = sum
        .per_thread
        .iter()
        .filter(|(_, t)| t.cost > 0.0)
        .map(|(id, t)| (*id, t.cost))
        .collect();
    Ok(Spend {
        total_cost: sum.total_cost,
        tracked_calls: sum.tracked_calls,
        untracked_calls: sum.untracked_calls,
        by_model: sum
            .by_model
            .into_iter()
            .map(|m| ModelSpend {
                model: m.model,
                cost: m.cost,
                input: m.input,
                output: m.output,
                cache_read: m.cache_read,
                calls: m.calls,
                priced: m.priced,
            })
            .collect(),
        top_threads: top_threads(conn, per_thread, top_n)?,
    })
}

/// One agent's (source's) spend over a window.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SourceCost {
    /// The source kind (`claude_code`, `codex`...).
    pub source: String,
    pub cost: f64,
    /// Priced model calls.
    pub calls: i64,
}

/// Spend over a window per model and per agent: what `cal report` shows.
#[derive(Debug, Clone)]
pub struct SpendBySource {
    pub total_cost: f64,
    pub tracked_calls: i64,
    pub untracked_calls: i64,
    /// Priciest first, unpriced models included (cost 0).
    pub by_model: Vec<ModelCost>,
    /// Priciest first; agents with no priced usage are left out.
    pub by_source: Vec<SourceCost>,
}

/// [`SpendBySource`] since `since` (epoch secs; 0 = all time): the same one aggregate pass as
/// [`cost_report`], with each thread's tally added to its source. A subagent transcript is
/// its own thread of the same source, so it counts with its agent.
pub fn spend_by_source(conn: &Connection, since: i64) -> Result<SpendBySource> {
    let (sql, args) = usage_sql(since, None, ProjectMatch::Exact);
    let mut rows = usage_by_thread_model(conn, &sql, &args)?;
    copilot_cli_calls(conn, since, None, ProjectMatch::Exact, &mut rows)?;
    let sum = summarize(rows);
    let mut per_source: HashMap<String, Tally> = HashMap::new();
    if !sum.per_thread.is_empty() {
        let mut stmt = conn.prepare_cached(
            "SELECT s.kind FROM threads t JOIN sources s ON s.id = t.source_id WHERE t.id = ?1",
        )?;
        for (thread_id, tally) in &sum.per_thread {
            if tally.cost <= 0.0 {
                continue;
            }
            let kind: Option<String> = stmt.query_row([thread_id], |r| r.get(0)).optional()?;
            if let Some(kind) = kind {
                per_source.entry(kind).or_default().add(tally);
            }
        }
    }
    let mut by_source: Vec<SourceCost> = per_source
        .into_iter()
        .map(|(source, t)| SourceCost {
            source,
            cost: t.cost,
            calls: t.calls,
        })
        .collect();
    by_source.sort_by(|a, b| {
        b.cost
            .total_cmp(&a.cost)
            .then_with(|| a.source.cmp(&b.source))
    });
    Ok(SpendBySource {
        total_cost: sum.total_cost,
        tracked_calls: sum.tracked_calls,
        untracked_calls: sum.untracked_calls,
        by_model: sum.by_model,
        by_source,
    })
}

/// One thread's (or session's, or day's) usage in dollars and tokens. Only priced usage
/// adds to `cost` and `calls`; unpriced usage still adds its tokens.
#[derive(Debug, Clone, Copy, Default)]
struct Tally {
    cost: f64,
    /// The part of `cost` spent re-reading cached context.
    cache_read_cost: f64,
    calls: i64,
    input: i64,
    output: i64,
    cache_write: i64,
    cache_read: i64,
}

impl Tally {
    fn add_usage(&mut self, u: &Usage) {
        if let Some(p) = price_for(&u.model) {
            self.cost += cost_of(u.input, u.output, u.cache_write, u.cache_read, &p);
            self.cache_read_cost += cost_of(0, 0, 0, u.cache_read, &p);
            self.calls += u.calls;
        }
        self.input += u.input;
        self.output += u.output;
        self.cache_write += u.cache_write;
        self.cache_read += u.cache_read;
    }

    fn add(&mut self, o: &Tally) {
        self.cost += o.cost;
        self.cache_read_cost += o.cache_read_cost;
        self.calls += o.calls;
        self.input += o.input;
        self.output += o.output;
        self.cache_write += o.cache_write;
        self.cache_read += o.cache_read;
    }
}

/// The usage rows rolled up: totals, per model (priciest first) and per thread.
struct Summary {
    total_cost: f64,
    tracked_calls: i64,
    untracked_calls: i64,
    by_model: Vec<ModelCost>,
    per_thread: HashMap<i64, Tally>,
}

fn summarize(rows: Vec<Usage>) -> Summary {
    let mut per_model: HashMap<String, ModelCost> = HashMap::new();
    let mut per_thread: HashMap<i64, Tally> = HashMap::new();
    let mut total_cost = 0.0;
    let mut tracked_calls = 0;
    let mut untracked_calls = 0;
    for u in rows {
        let pricing = pricing(&u.model);
        let price = match pricing {
            Pricing::Priced { price, .. } => Some(price),
            _ => None,
        };
        let cost = price.map_or(0.0, |p| {
            cost_of(u.input, u.output, u.cache_write, u.cache_read, &p)
        });
        if price.is_some() {
            total_cost += cost;
            tracked_calls += u.calls;
        } else {
            untracked_calls += u.calls;
        }
        per_thread.entry(u.thread_id).or_default().add_usage(&u);
        let m = per_model
            .entry(u.model.clone())
            .or_insert_with(|| ModelCost::new(&u.model, pricing));
        m.cost += cost;
        m.input += u.input;
        m.output += u.output;
        m.cache_write += u.cache_write;
        m.cache_read += u.cache_read;
        m.calls += u.calls;
    }
    let mut by_model: Vec<ModelCost> = per_model.into_values().collect();
    by_model.sort_by(|a, b| {
        b.cost
            .total_cmp(&a.cost)
            .then_with(|| a.model.cmp(&b.model))
    });
    Summary {
        total_cost,
        tracked_calls,
        untracked_calls,
        by_model,
        per_thread,
    }
}

/// SUM the usage columns per (thread, model). The `ts` filter is only added for a real
/// window: `ts >= 0` would steer the planner onto the timestamp index and away from the
/// covering usage index.
fn usage_sql(
    since: i64,
    project: Option<&str>,
    mode: ProjectMatch,
) -> (String, Vec<Box<dyn ToSql>>) {
    let mut args: Vec<Box<dyn ToSql>> = Vec::new();
    let (join, filters) = usage_filters(since, project, mode, &mut args);
    let sql = format!(
        "SELECT m.thread_id, m.model, SUM(m.input_tokens), SUM(m.output_tokens),
                SUM(m.cache_write_tokens), SUM(m.cache_read_tokens), COUNT(*)
         FROM messages m{join}
         WHERE m.model IS NOT NULL{filters}
         GROUP BY m.thread_id, m.model"
    );
    (sql, args)
}

/// The window and project filters [`usage_sql`] adds: a `JOIN threads t` when a project is
/// given, and the `AND ...` conditions.
fn usage_filters(
    since: i64,
    project: Option<&str>,
    mode: ProjectMatch,
    args: &mut Vec<Box<dyn ToSql>>,
) -> (&'static str, String) {
    let mut filters = String::new();
    if since > 0 {
        // `+` keeps the bound off every ts index: with an exact project the planner would
        // otherwise drive threads -> idx_messages_thread_ts (thread_id, ts), which lacks the
        // usage columns, and read each message row (overflow pages included) for them.
        args.push(Box::new(since));
        filters.push_str(&format!(" AND +m.ts >= ?{}", args.len()));
    }
    let join = match project.filter(|p| !p.is_empty()) {
        Some(p) => {
            let cond = crate::search::project_condition("t", p, mode, args);
            filters.push_str(&format!(" AND {cond}"));
            " JOIN threads t ON t.id = m.thread_id"
        }
        None => "",
    };
    (join, filters)
}

/// Copilot CLI's calls. Its usage arrives once per model at each session shutdown, so one
/// usage row stands for every request of that run, and COUNT(*) would report one call per
/// shutdown. Each row's text carries the run's `requests.count`
/// (`copilot_cli::usage_row_requests`); a row without one counts as one call. Only that
/// source's usage rows are read (a few per session), so the main aggregate keeps to its
/// covering index. Its spend is dated by the shutdown, so a `--since` window that starts
/// mid-run counts the whole run or none of it.
fn copilot_cli_calls(
    conn: &Connection,
    since: i64,
    project: Option<&str>,
    mode: ProjectMatch,
    rows: &mut [Usage],
) -> Result<()> {
    let sid: Option<i64> = conn
        .query_row(
            "SELECT id FROM sources WHERE kind = ?1",
            [crate::indexer::copilot_cli::KIND],
            |r| r.get(0),
        )
        .optional()?;
    let Some(sid) = sid else {
        return Ok(());
    };
    let mut args: Vec<Box<dyn ToSql>> = vec![Box::new(sid)];
    let (join, filters) = usage_filters(since, project, mode, &mut args);
    let join = if join.is_empty() {
        " JOIN threads t ON t.id = m.thread_id"
    } else {
        join
    };
    let sql = format!(
        "SELECT m.thread_id, m.model, m.text FROM messages m{join}
         WHERE t.source_id = ?1 AND m.model IS NOT NULL{filters}"
    );
    let arg_refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let mut calls: HashMap<(i64, String), i64> = HashMap::new();
    let found = stmt.query_map(arg_refs.as_slice(), |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    for row in found {
        let (thread, model, text) = row?;
        let n = crate::indexer::copilot_cli::usage_row_requests(&text).unwrap_or(1);
        *calls.entry((thread, model)).or_insert(0) += n;
    }
    if calls.is_empty() {
        return Ok(());
    }
    for u in rows.iter_mut() {
        if let Some(n) = calls.get(&(u.thread_id, u.model.clone())) {
            u.calls = *n;
        }
    }
    Ok(())
}

/// Run [`usage_sql`]'s statement.
fn usage_by_thread_model(
    conn: &Connection,
    sql: &str,
    args: &[Box<dyn ToSql>],
) -> Result<Vec<Usage>> {
    let arg_refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map(arg_refs.as_slice(), |r| {
            Ok(Usage {
                thread_id: r.get(0)?,
                model: r.get(1)?,
                input: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                output: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                cache_write: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                cache_read: r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                calls: r.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// The priciest threads, from the per-thread totals; titles are looked up only for the
/// threads actually shown.
fn top_threads(
    conn: &Connection,
    per_thread: HashMap<i64, f64>,
    top_n: usize,
) -> Result<Vec<ThreadCost>> {
    let mut ranked: Vec<(i64, f64)> = per_thread.into_iter().filter(|(_, c)| *c > 0.0).collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.truncate(top_n);
    let mut stmt = conn
        .prepare("SELECT title, COALESCE(project_key, project_path) FROM threads WHERE id = ?1")?;
    let mut out = Vec::with_capacity(ranked.len());
    for (thread_id, cost) in ranked {
        let (title, project) = stmt
            .query_row([thread_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?
            .unwrap_or((None, None));
        out.push(ThreadCost {
            thread_id,
            title,
            project,
            cost,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// The breakdowns: `cal cost --by thread|commit|project|day` and the app's spend charts.
// ---------------------------------------------------------------------------------------

/// What every figure is. Subscription plans bill a flat fee, so for them this is what the
/// same usage would cost at API list prices.
pub const COST_BASIS: &str = "List-price equivalent of the recorded token usage: an estimate, not a billing record. A subscription bills differently.";

/// How `cal cost` breaks spend down.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CostBy {
    /// Per model only (the default).
    #[default]
    Model,
    /// Per session: a Claude Code session's subagents count toward it.
    Thread,
    /// Per commit the sessions produced (from `thread_commits`).
    Commit,
    /// Per project.
    Project,
    /// Per local calendar day.
    Day,
}

impl CostBy {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "model" => Self::Model,
            "thread" | "session" => Self::Thread,
            "commit" => Self::Commit,
            "project" => Self::Project,
            "day" => Self::Day,
            _ => return None,
        })
    }
}

/// Spend on one model.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
    pub model: String,
    /// The price-table family it matched (`claude-opus`, `gpt-5`...), null when none did.
    pub family: Option<String>,
    /// False when the model has no price: its cost is 0 and its calls are untracked.
    pub priced: bool,
    /// Why an unpriced model has no price (null for a priced one).
    pub unpriced_reason: Option<String>,
    pub cost: f64,
    pub calls: i64,
    pub input: i64,
    pub output: i64,
    pub cache_write: i64,
    pub cache_read: i64,
}

impl ModelCost {
    fn new(model: &str, pricing: Pricing) -> Self {
        let (family, priced, unpriced_reason) = match pricing {
            Pricing::Priced { family, .. } => (Some(family.to_string()), true, None),
            Pricing::Unpriced { family, why } => {
                (Some(family.to_string()), false, Some(why.to_string()))
            }
            Pricing::Unknown => (None, false, Some("no list price on file".to_string())),
        };
        Self {
            model: model.to_string(),
            family,
            priced,
            unpriced_reason,
            cost: 0.0,
            calls: 0,
            input: 0,
            output: 0,
            cache_write: 0,
            cache_read: 0,
        }
    }
}

/// The kind of a [`CostSignal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum SignalKind {
    /// Re-reading cached context is most of the session's cost.
    CacheReads,
    /// More than 50 turns; `value` is the share of cost after turn 50.
    LongSession,
    /// Subagents spent a large share of the session's cost.
    Subagents,
    /// The same tool failed several times in a row; `value` is the longest run.
    ToolLoop,
}

/// Where a session's spend went, as a short neutral label (never a judgment).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CostSignal {
    pub kind: SignalKind,
    /// Ready to show, e.g. "Cache reads 64% of cost".
    pub label: String,
    /// The share (0 to 1) or count behind the label.
    pub value: f64,
}

/// One session's spend (`--by thread`).
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionCost {
    pub id: i64,
    pub title: Option<String>,
    pub source: String,
    pub project: Option<String>,
    pub last_active: Option<i64>,
    /// The session's own cost plus its subagents'.
    pub cost: f64,
    pub subagent_cost: f64,
    /// Subagent transcripts folded into this session.
    pub subagents: i64,
    /// Priced model calls, subagents included.
    pub calls: i64,
    /// User turns in the session itself.
    pub turns: i64,
    pub signals: Vec<CostSignal>,
}

/// A session behind a commit.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CommitSession {
    pub id: i64,
    pub title: Option<String>,
    /// The session's whole cost, subagents included.
    pub cost: f64,
    /// Files the session and the commit share (the link's confidence).
    pub overlap: i64,
    /// How many of the listed commits this session is linked to. Above 1, its cost counts
    /// toward each of them in full.
    pub commits: i64,
}

/// What the sessions behind one commit cost (`--by commit`).
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CommitCost {
    pub sha: String,
    pub short_sha: String,
    pub subject: Option<String>,
    pub committed_at: i64,
    pub project: Option<String>,
    /// The sum of its sessions' whole costs.
    pub cost: f64,
    /// True when a session behind it also produced another listed commit, so commit costs
    /// overlap and do not add up to the total.
    pub shared: bool,
    pub sessions: Vec<CommitSession>,
}

/// One project's spend (`--by project`).
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectCost {
    /// The project key; null for sessions outside any project.
    pub project: Option<String>,
    pub cost: f64,
    /// Sessions with usage; a Claude Code session's subagent transcripts count with it.
    pub threads: i64,
    pub calls: i64,
}

/// One local calendar day's spend (`--by day`).
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DayCost {
    /// `YYYY-MM-DD` in the machine's local time zone.
    pub day: String,
    pub cost: f64,
    pub input: i64,
    pub output: i64,
    pub cache_write: i64,
    pub cache_read: i64,
}

/// `cal cost`: totals and per-model spend over the window, plus the one breakdown asked for.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CostReport {
    pub by: CostBy,
    /// Start of the window in epoch seconds (0: all time). With `by: commit` it bounds the
    /// commit dates, and each session's whole cost counts.
    pub since: i64,
    /// What the figures are (list-price equivalents, not a bill).
    pub basis: String,
    pub total_cost: f64,
    pub tracked_calls: i64,
    pub untracked_calls: i64,
    pub by_model: Vec<ModelCost>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threads: Option<Vec<SessionCost>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commits: Option<Vec<CommitCost>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projects: Option<Vec<ProjectCost>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub days: Option<Vec<DayCost>>,
}

/// What [`cost_report`] computes.
#[derive(Debug, Clone, Copy)]
pub struct CostQuery<'a> {
    pub by: CostBy,
    /// Epoch seconds; 0 = all time.
    pub since: i64,
    pub project: Option<&'a str>,
    pub mode: ProjectMatch,
    /// Rows for `thread`, `commit` and `project` (days are bounded by the window).
    pub limit: usize,
}

/// Spend over the window broken down as `q.by` asks.
pub fn cost_report(conn: &Connection, q: &CostQuery) -> Result<CostReport> {
    let (sql, args) = usage_sql(q.since, q.project, q.mode);
    let mut rows = usage_by_thread_model(conn, &sql, &args)?;
    copilot_cli_calls(conn, q.since, q.project, q.mode, &mut rows)?;
    let sum = summarize(rows);
    let mut report = CostReport {
        by: q.by,
        since: q.since,
        basis: COST_BASIS.to_string(),
        total_cost: sum.total_cost,
        tracked_calls: sum.tracked_calls,
        untracked_calls: sum.untracked_calls,
        by_model: sum.by_model,
        threads: None,
        commits: None,
        projects: None,
        days: None,
    };
    match q.by {
        CostBy::Model => {}
        CostBy::Thread => {
            report.threads = Some(session_costs(conn, &sum.per_thread, q.since, q.limit)?)
        }
        CostBy::Project => report.projects = Some(project_costs(conn, &sum.per_thread, q.limit)?),
        CostBy::Day => report.days = Some(day_costs(conn, q.since, q.project, q.mode, false)?),
        CostBy::Commit => {
            report.commits = Some(commit_costs(conn, q.since, q.project, q.mode, q.limit)?)
        }
    }
    Ok(report)
}

/// The app's spend charts: the last 30 local days (every day, zero included) and the
/// priciest commits, for the Stats card (`project` None) and a Project page.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpendBreakdown {
    pub days: Vec<DayCost>,
    pub commits: Vec<CommitCost>,
}

/// Cache key: (project, first day of the window).
type BreakdownKey = (Option<String>, i64);

static BREAKDOWN_CACHE: std::sync::LazyLock<
    crate::search::cache::AggCache<BreakdownKey, SpendBreakdown>,
> = std::sync::LazyLock::new(crate::search::cache::AggCache::new);

/// [`SpendBreakdown`], reused while the thread set is unchanged (see `search::cache`), with
/// `project` an exact key.
pub fn breakdown_cached(conn: &Connection, project: Option<&str>) -> Result<SpendBreakdown> {
    let since = local_day_start(30);
    let project = project.filter(|p| !p.is_empty());
    let key = (project.map(str::to_string), since);
    BREAKDOWN_CACHE.get_or_try(crate::search::cache::index_version(conn)?, key, || {
        Ok::<_, anyhow::Error>(SpendBreakdown {
            days: day_costs(conn, since, project, ProjectMatch::Exact, true)?,
            commits: commit_costs(conn, 0, project, ProjectMatch::Exact, 5)?,
        })
    })
}

/// Epoch seconds of local midnight starting the last `days` days (today included).
pub fn local_day_start(days: u32) -> i64 {
    use chrono::{Duration, Local, TimeZone};
    let first = Local::now().date_naive() - Duration::days(i64::from(days.max(1)) - 1);
    let midnight = first.and_hms_opt(0, 0, 0).expect("midnight exists");
    Local
        .from_local_datetime(&midnight)
        .earliest()
        .map(|t| t.timestamp())
        // A zone whose midnight is skipped (a DST jump at 00:00): an hour later exists.
        .unwrap_or_else(|| midnight.and_utc().timestamp())
}

/// Usage per quarter hour, folded into local days. Every UTC offset is a whole
/// number of quarter hours, so a bucket never straddles a local midnight, and the
/// aggregate stays on the covering usage index (ts is its third column) instead of
/// evaluating a time-zone conversion per message. `fill` lists every day from `since` to
/// today, zero ones included, oldest first.
fn day_costs(
    conn: &Connection,
    since: i64,
    project: Option<&str>,
    mode: ProjectMatch,
    fill: bool,
) -> Result<Vec<DayCost>> {
    use chrono::{Local, TimeZone};
    let (sql, args) = day_sql(since, project, mode);
    let arg_refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(arg_refs.as_slice(), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            [
                r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                r.get::<_, Option<i64>>(5)?.unwrap_or(0),
            ],
        ))
    })?;
    let mut days: std::collections::BTreeMap<chrono::NaiveDate, DayCost> = Default::default();
    let blank = |day: chrono::NaiveDate| DayCost {
        day: day.format("%Y-%m-%d").to_string(),
        cost: 0.0,
        input: 0,
        output: 0,
        cache_write: 0,
        cache_read: 0,
    };
    let mut prices: HashMap<String, Option<ModelPrice>> = HashMap::new();
    for row in rows {
        let (model, bucket, [input, output, cache_write, cache_read]) = row?;
        let Some(at) = Local.timestamp_opt(bucket * QUARTER_HOUR, 0).earliest() else {
            continue;
        };
        let date = at.date_naive();
        let d = days.entry(date).or_insert_with(|| blank(date));
        let price = *prices
            .entry(model)
            .or_insert_with_key(|m| price_for(m.as_str()));
        if let Some(p) = price {
            d.cost += cost_of(input, output, cache_write, cache_read, &p);
        }
        d.input += input;
        d.output += output;
        d.cache_write += cache_write;
        d.cache_read += cache_read;
    }
    if fill && since > 0 {
        if let Some(start) = Local.timestamp_opt(since, 0).earliest() {
            let today = Local::now().date_naive();
            let mut day = start.date_naive();
            while day <= today {
                days.entry(day).or_insert_with(|| blank(day));
                day = day.succ_opt().unwrap_or(today + chrono::Duration::days(1));
            }
        }
    }
    Ok(days.into_values().collect())
}

/// 15 minutes: the day aggregate's bucket (see [`day_costs`]).
const QUARTER_HOUR: i64 = 900;

/// [`usage_sql`] grouped by (thread, model, quarter hour). The thread leads the GROUP BY,
/// though the fold ignores it, to keep the planner on the covering usage index: grouped
/// by (model, bucket) it took idx_messages_model and read every message row for the
/// token columns, 33 s cold on a 5 GB index against 0.6 s for this form.
fn day_sql(since: i64, project: Option<&str>, mode: ProjectMatch) -> (String, Vec<Box<dyn ToSql>>) {
    let mut args: Vec<Box<dyn ToSql>> = Vec::new();
    let (join, filters) = usage_filters(since, project, mode, &mut args);
    let sql = format!(
        "SELECT m.model, m.ts / {QUARTER_HOUR}, SUM(m.input_tokens), SUM(m.output_tokens),
                SUM(m.cache_write_tokens), SUM(m.cache_read_tokens)
         FROM messages m{join}
         WHERE m.model IS NOT NULL AND m.ts IS NOT NULL{filters}
         GROUP BY m.thread_id, m.model, m.ts / {QUARTER_HOUR}"
    );
    (sql, args)
}

/// The priciest sessions, each with its subagents folded in and its waste signals.
fn session_costs(
    conn: &Connection,
    per_thread: &HashMap<i64, Tally>,
    since: i64,
    limit: usize,
) -> Result<Vec<SessionCost>> {
    let parents = crate::indexer::subagent_parents(conn)?;
    // (own, subagents' total, subagent count) per session.
    let mut sessions: HashMap<i64, (Tally, Tally, i64)> = HashMap::new();
    for (id, t) in per_thread {
        match parents.get(id) {
            Some(parent) => {
                let s = sessions.entry(*parent).or_default();
                s.1.add(t);
                s.2 += 1;
            }
            None => sessions.entry(*id).or_default().0.add(t),
        }
    }
    let mut ranked: Vec<(i64, (Tally, Tally, i64))> = sessions
        .into_iter()
        .filter(|(_, (own, sub, _))| own.cost + sub.cost > 0.0)
        .collect();
    ranked.sort_by(|a, b| {
        let cost = |s: &(Tally, Tally, i64)| s.0.cost + s.1.cost;
        cost(&b.1)
            .total_cmp(&cost(&a.1))
            .then_with(|| a.0.cmp(&b.0))
    });
    ranked.truncate(limit);
    let mut meta = conn.prepare_cached(
        "SELECT t.title, s.kind, COALESCE(t.project_key, t.project_path), t.updated_at
         FROM threads t JOIN sources s ON s.id = t.source_id WHERE t.id = ?1",
    )?;
    let mut out = Vec::with_capacity(ranked.len());
    for (id, (own, sub, subagents)) in ranked {
        let (title, source, project, last_active) = meta
            .query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .optional()?
            .unwrap_or((None, String::new(), None, None));
        let cost = own.cost + sub.cost;
        let turns = user_turns(conn, id)?;
        let mut signals = Vec::new();
        if own.cost > 0.0 && own.cache_read_cost / own.cost >= CACHE_READ_SHARE {
            let share = own.cache_read_cost / own.cost;
            signals.push(CostSignal {
                kind: SignalKind::CacheReads,
                label: format!("Cache reads {} of cost", percent(share)),
                value: share,
            });
        }
        if turns > LONG_SESSION_TURNS && own.cost > 0.0 {
            let after = cost_after_turn(conn, id, since, LONG_SESSION_TURNS)?;
            let share = (after / own.cost).clamp(0.0, 1.0);
            signals.push(CostSignal {
                kind: SignalKind::LongSession,
                label: format!(
                    "{turns} turns; {} of cost after turn {LONG_SESSION_TURNS}",
                    percent(share)
                ),
                value: share,
            });
        }
        if cost > 0.0 && sub.cost / cost >= SUBAGENT_SHARE {
            let share = sub.cost / cost;
            signals.push(CostSignal {
                kind: SignalKind::Subagents,
                label: format!("Subagents {} of cost", percent(share)),
                value: share,
            });
        }
        if let Some((tool, run)) = failing_tool_run(conn, id)? {
            signals.push(CostSignal {
                kind: SignalKind::ToolLoop,
                label: format!("{tool} failed {run} times in a row"),
                value: run as f64,
            });
        }
        out.push(SessionCost {
            id,
            title,
            source,
            project,
            last_active,
            cost,
            subagent_cost: sub.cost,
            subagents,
            calls: own.calls + sub.calls,
            turns,
            signals,
        });
    }
    Ok(out)
}

/// A session past this many user turns gets a long-session signal.
const LONG_SESSION_TURNS: i64 = 50;
/// Cache reads at or above this share of a session's cost get a signal. Long agent sessions
/// re-read their whole context every call; below half it is unremarkable.
const CACHE_READ_SHARE: f64 = 0.5;
/// Subagents at or above this share of a session's cost get a signal.
const SUBAGENT_SHARE: f64 = 0.25;
/// The same tool failing this many times in a row gets a signal.
const TOOL_LOOP_RUN: usize = 3;

fn percent(share: f64) -> String {
    format!("{:.0}%", share * 100.0)
}

/// User turns in a thread, read from the (thread_id, ts, role) index alone.
fn user_turns(conn: &Connection, thread_id: i64) -> Result<i64> {
    Ok(conn
        .prepare_cached(USER_TURNS_SQL)?
        .query_row([thread_id], |r| r.get(0))?)
}

const USER_TURNS_SQL: &str = "SELECT COUNT(*) FROM messages WHERE thread_id = ?1 AND role = 'user'";

/// What a thread spent from its turn `after + 1` on (within the window): the turn's
/// timestamp from the (thread_id, ts, role) index, then the usage from the covering usage
/// index, so neither reads a message row.
fn cost_after_turn(conn: &Connection, thread_id: i64, since: i64, after: i64) -> Result<f64> {
    let from: Option<i64> = conn
        .prepare_cached(TURN_TS_SQL)?
        .query_row(rusqlite::params![thread_id, after], |r| r.get(0))
        .optional()?;
    let Some(from) = from else {
        return Ok(0.0);
    };
    let mut stmt = conn.prepare_cached(USAGE_FROM_SQL)?;
    let rows = stmt.query_map(rusqlite::params![thread_id, from.max(since)], |r| {
        Ok(Usage {
            thread_id,
            model: r.get(0)?,
            input: r.get::<_, Option<i64>>(1)?.unwrap_or(0),
            output: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
            cache_write: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
            cache_read: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
            calls: 0,
        })
    })?;
    let mut t = Tally::default();
    for u in rows {
        t.add_usage(&u?);
    }
    Ok(t.cost)
}

const TURN_TS_SQL: &str = "SELECT ts FROM messages
     WHERE thread_id = ?1 AND role = 'user' AND ts IS NOT NULL
     ORDER BY ts LIMIT 1 OFFSET ?2";

const USAGE_FROM_SQL: &str = "SELECT model, SUM(input_tokens), SUM(output_tokens),
            SUM(cache_write_tokens), SUM(cache_read_tokens)
     FROM messages WHERE thread_id = ?1 AND model IS NOT NULL AND ts >= ?2
     GROUP BY model";

/// The longest run of one tool failing call after call in a thread, when it reaches
/// [`TOOL_LOOP_RUN`]: tool results that failed back to back while that tool was the only
/// one being called. Results are not paired to calls one by one, since an empty result
/// (a quiet shell command) is never stored and would shift every pairing after it; a call
/// to another tool or a result that succeeded ends the run.
fn failing_tool_run(conn: &Connection, thread_id: i64) -> Result<Option<(String, usize)>> {
    let mut stmt = conn.prepare_cached(TOOL_ROWS_SQL)?;
    let rows = stmt.query_map([thread_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })?;
    let mut calling: Option<String> = None;
    let mut fails = 0usize;
    let mut best: Option<(String, usize)> = None;
    for row in rows {
        let (role, tool, head) = row?;
        if role == "tool" {
            let Some(name) = &calling else {
                continue;
            };
            if !tool_failed(head.as_deref().unwrap_or("")) {
                fails = 0;
                continue;
            }
            fails += 1;
            if best.as_ref().is_none_or(|(_, b)| fails > *b) {
                best = Some((name.clone(), fails));
            }
        } else if let Some(name) = tool {
            if calling.as_deref() != Some(name.as_str()) {
                calling = Some(name);
                fails = 0;
            }
        }
    }
    Ok(best.filter(|(_, k)| *k >= TOOL_LOOP_RUN))
}

/// Tool calls and results in order; the result's head is enough to tell a failure.
const TOOL_ROWS_SQL: &str = "SELECT role, tool_name,
            CASE WHEN role = 'tool' THEN substr(text, 1, 300) END
     FROM messages
     WHERE thread_id = ?1 AND (tool_name IS NOT NULL OR role = 'tool')
     ORDER BY seq";

/// Whether a tool result reads as a failure, from how each agent reports one: Claude Code's
/// `<tool_use_error>` and `Exit code N`, Codex's `Exit code: N` / `Process exited with code
/// N` / `"exit_code": N`, or an error line or Python's traceback header leading the output.
pub(crate) fn tool_failed(text: &str) -> bool {
    let head = text.trim_start();
    if head.starts_with("<tool_use_error>") {
        return true;
    }
    let first = head.lines().next().unwrap_or("").trim();
    let lower = first.to_ascii_lowercase();
    let nonzero = |rest: &str| {
        let code: String = rest
            .trim_start_matches([' ', ':'])
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '-')
            .collect();
        !code.is_empty() && code != "0"
    };
    for marker in ["exit code", "process exited with code"] {
        if let Some(rest) = lower.strip_prefix(marker) {
            return nonzero(rest);
        }
    }
    if let Some(i) = head
        .get(..300.min(head.len()))
        .and_then(|h| h.find("\"exit_code\":"))
    {
        return nonzero(&head[i + "\"exit_code\":".len()..]);
    }
    // Python's traceback header leads a failed run, and the extractor no longer returns it
    // (it takes the block's exception line, which this first line alone never reaches).
    lower.starts_with("error")
        || lower.starts_with("fatal")
        || lower.starts_with("traceback (most recent call last)")
        || !crate::issues::extract_errors(first).is_empty()
}

/// Spend per project, priciest first.
fn project_costs(
    conn: &Connection,
    per_thread: &HashMap<i64, Tally>,
    limit: usize,
) -> Result<Vec<ProjectCost>> {
    let mut stmt = conn
        .prepare_cached("SELECT COALESCE(project_key, project_path) FROM threads WHERE id = ?1")?;
    // A subagent transcript counts toward its session, not as one: on the clone a project
    // with 182 sessions read "10471 sessions" when every transcript counted.
    let parents = crate::indexer::subagent_parents(conn)?;
    let mut by: HashMap<Option<String>, (ProjectCost, HashSet<i64>)> = HashMap::new();
    for (id, t) in per_thread {
        let project: Option<String> = stmt.query_row([id], |r| r.get(0)).optional()?.flatten();
        let (p, sessions) = by.entry(project.clone()).or_insert_with(|| {
            (
                ProjectCost {
                    project,
                    cost: 0.0,
                    threads: 0,
                    calls: 0,
                },
                HashSet::new(),
            )
        });
        p.cost += t.cost;
        p.calls += t.calls;
        sessions.insert(parents.get(id).copied().unwrap_or(*id));
    }
    let mut out: Vec<ProjectCost> = by
        .into_values()
        .map(|(mut p, sessions)| {
            p.threads = sessions.len() as i64;
            p
        })
        .filter(|p| p.cost > 0.0)
        .collect();
    out.sort_by(|a, b| {
        b.cost
            .total_cmp(&a.cost)
            .then_with(|| a.project.cmp(&b.project))
    });
    out.truncate(limit);
    Ok(out)
}

/// The commits the sessions produced, priciest first: each commit costs what its sessions
/// cost in full (subagents included, all time), so a session behind several commits counts
/// toward each and the commit is marked `shared`. `since` bounds the commit dates.
fn commit_costs(
    conn: &Connection,
    since: i64,
    project: Option<&str>,
    mode: ProjectMatch,
    limit: usize,
) -> Result<Vec<CommitCost>> {
    if !crate::db::has_table(conn, "thread_commits") {
        return Ok(Vec::new());
    }
    let mut args: Vec<Box<dyn ToSql>> = Vec::new();
    let mut filters = String::new();
    if since > 0 {
        args.push(Box::new(since));
        filters.push_str(&format!(" AND tc.committed_at >= ?{}", args.len()));
    }
    if let Some(p) = project.filter(|p| !p.is_empty()) {
        let cond = crate::search::project_condition("t", p, mode, &mut args);
        filters.push_str(&format!(" AND {cond}"));
    }
    let sql = format!(
        "SELECT tc.thread_id, tc.sha, tc.short_sha, tc.subject, tc.committed_at, tc.overlap,
                COALESCE(t.project_key, t.project_path), t.title
         FROM thread_commits tc JOIN threads t ON t.id = tc.thread_id
         WHERE 1{filters}
         ORDER BY tc.committed_at DESC, tc.sha"
    );
    let arg_refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    type Link = (
        i64,
        String,
        String,
        Option<String>,
        i64,
        i64,
        Option<String>,
        Option<String>,
    );
    let links: Vec<Link> = stmt
        .query_map(arg_refs.as_slice(), |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    if links.is_empty() {
        return Ok(Vec::new());
    }
    let parents = crate::indexer::subagent_parents(conn)?;
    let root = |id: i64| parents.get(&id).copied().unwrap_or(id);
    let mut children: HashMap<i64, Vec<i64>> = HashMap::new();
    for (child, parent) in &parents {
        children.entry(*parent).or_default().push(*child);
    }
    let mut session_cost: HashMap<i64, f64> = HashMap::new();
    let mut titles: HashMap<i64, Option<String>> = HashMap::new();
    let mut title_stmt = conn.prepare_cached("SELECT title FROM threads WHERE id = ?1")?;
    // Per commit: its fields, then (session, best overlap) in first-seen order.
    let mut commits: Vec<(CommitCost, Vec<(i64, i64)>)> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut commits_per_session: HashMap<i64, HashSet<String>> = HashMap::new();
    for (thread, sha, short_sha, subject, committed_at, overlap, project, title) in links {
        let session = root(thread);
        if session == thread {
            titles.entry(session).or_insert(title);
        }
        commits_per_session
            .entry(session)
            .or_default()
            .insert(sha.clone());
        let i = *index.entry(sha.clone()).or_insert_with(|| {
            commits.push((
                CommitCost {
                    sha,
                    short_sha,
                    subject,
                    committed_at,
                    project,
                    cost: 0.0,
                    shared: false,
                    sessions: Vec::new(),
                },
                Vec::new(),
            ));
            commits.len() - 1
        });
        let sessions = &mut commits[i].1;
        match sessions.iter_mut().find(|(s, _)| *s == session) {
            Some(s) => s.1 = s.1.max(overlap),
            None => sessions.push((session, overlap)),
        }
    }
    let mut out = Vec::with_capacity(commits.len());
    for (mut c, sessions) in commits {
        for (session, overlap) in sessions {
            let cost = match session_cost.get(&session) {
                Some(c) => *c,
                None => {
                    let mut total = thread_cost(conn, session)?;
                    for child in children.get(&session).into_iter().flatten() {
                        total += thread_cost(conn, *child)?;
                    }
                    session_cost.insert(session, total);
                    total
                }
            };
            let title = match titles.get(&session) {
                Some(t) => t.clone(),
                None => {
                    let t: Option<String> = title_stmt
                        .query_row([session], |r| r.get(0))
                        .optional()?
                        .flatten();
                    titles.insert(session, t.clone());
                    t
                }
            };
            let n = commits_per_session.get(&session).map_or(1, |s| s.len()) as i64;
            c.cost += cost;
            c.shared |= n > 1;
            c.sessions.push(CommitSession {
                id: session,
                title,
                cost,
                overlap,
                commits: n,
            });
        }
        c.sessions
            .sort_by(|a, b| b.cost.total_cmp(&a.cost).then_with(|| a.id.cmp(&b.id)));
        out.push(c);
    }
    out.retain(|c| c.cost > 0.0);
    out.sort_by(|a, b| {
        b.cost
            .total_cmp(&a.cost)
            .then_with(|| b.committed_at.cmp(&a.committed_at))
    });
    out.truncate(limit);
    Ok(out)
}

/// One thread's whole priced cost, from the covering usage index.
fn thread_cost(conn: &Connection, thread_id: i64) -> Result<f64> {
    let mut stmt = conn.prepare_cached(THREAD_USAGE_SQL)?;
    let rows = stmt.query_map([thread_id], |r| {
        Ok(Usage {
            thread_id,
            model: r.get(0)?,
            input: r.get::<_, Option<i64>>(1)?.unwrap_or(0),
            output: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
            cache_write: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
            cache_read: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
            calls: 0,
        })
    })?;
    let mut t = Tally::default();
    for u in rows {
        t.add_usage(&u?);
    }
    Ok(t.cost)
}

const THREAD_USAGE_SQL: &str = "SELECT model, SUM(input_tokens), SUM(output_tokens),
            SUM(cache_write_tokens), SUM(cache_read_tokens)
     FROM messages WHERE thread_id = ?1 AND model IS NOT NULL
     GROUP BY model";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pricing_matches_by_family() {
        assert!(
            price_for("claude-opus-4-8").unwrap().output
                > price_for("claude-sonnet-4-6").unwrap().output
        );
        assert!(price_for("claude-haiku-4-5").is_some());
        assert!(price_for("gpt-5").is_some());
        assert!(price_for("some-unknown-local-model").is_none());
        // Fable/Mythos are priced, not left untracked; guard the cache ratios (1.25×/0.1×).
        let fable = price_for("claude-fable-5").unwrap();
        assert_eq!(fable.input, 10.0);
        assert_eq!(fable.output, 50.0);
        assert_eq!(fable.cache_write, 12.5);
        assert_eq!(fable.cache_read, 1.0);
        assert!(price_for("claude-mythos-5").is_some());
    }

    /// Copilot CLI's one usage row per run reports the run's requests as its calls.
    #[test]
    fn copilot_cli_calls_come_from_the_request_count() {
        let p = std::env::temp_dir().join(format!("calli_cost_copcli_{}.db", std::process::id()));
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", p.display()));
        }
        let conn = crate::db::open(&p).unwrap();
        let sid: i64 = conn
            .query_row(
                "SELECT id FROM sources WHERE kind = 'copilot_cli'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO threads (source_id, external_id, title, project_path, created_at, updated_at)
             VALUES (?1, 's1', 'cli', '/p', 1, 1)",
            [sid],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        for (seq, text) in [
            "Copilot CLI usage: gpt-5, 10 input, 0 cache read, 0 cache write, 1 output tokens, 4 requests",
            "Copilot CLI usage: gpt-5, 10 input, 0 cache read, 0 cache write, 1 output tokens",
        ]
        .iter()
        .enumerate()
        {
            conn.execute(
                "INSERT INTO messages (thread_id, seq, role, text, ts, model, input_tokens, output_tokens, cache_write_tokens, cache_read_tokens)
                 VALUES (?1, ?2, 'system', ?3, 100, 'gpt-5', 10, 1, 0, 0)",
                rusqlite::params![tid, seq as i64, text],
            )
            .unwrap();
        }
        let s = spend(&conn, 0, None, 5).unwrap();
        assert_eq!(
            s.by_model[0].calls, 5,
            "4 requests, plus 1 for the row without a count"
        );
        assert_eq!(spend(&conn, 0, Some("/p"), 5).unwrap().by_model[0].calls, 5);
        assert!(spend(&conn, 200, None, 5).unwrap().by_model.is_empty());
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", p.display()));
        }
    }

    #[test]
    fn cost_math() {
        let p = price_for("claude-opus-4-8").unwrap();
        // 1M input @ $5 + 1M output @ $25 = $30.
        assert!((cost_of(1_000_000, 1_000_000, 0, 0, &p) - 30.0).abs() < 1e-6);
        // cache read is far cheaper than fresh input.
        assert!(cost_of(0, 0, 0, 1_000_000, &p) < cost_of(1_000_000, 0, 0, 0, &p));
    }

    #[test]
    fn spend_aggregates_and_flags_untracked() {
        let p = std::env::temp_dir().join(format!("calli_cost_{}.db", std::process::id()));
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        let conn = crate::db::open(&p).unwrap();
        let sid: i64 = conn
            .query_row(
                "SELECT id FROM sources WHERE kind = 'claude_code'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO threads (source_id, external_id, title, project_path, created_at, updated_at)
             VALUES (?1, 't', 'pricey thread', '/p', 1, 1)",
            [sid],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        let ins = |role: &str, model: Option<&str>, inp: i64, out: i64, seq: i64| {
            conn.execute(
                "INSERT INTO messages (thread_id, seq, role, text, ts, model, input_tokens, output_tokens, cache_write_tokens, cache_read_tokens)
                 VALUES (?1, ?2, ?3, 'x', 100, ?4, ?5, ?6, 0, 0)",
                rusqlite::params![tid, seq, role, model, inp, out],
            )
            .unwrap();
        };
        ins(
            "assistant",
            Some("claude-opus-4-8"),
            1_000_000,
            1_000_000,
            0,
        ); // $30
        ins("assistant", Some("mystery-model"), 1_000_000, 0, 1); // untracked
        ins("user", None, 0, 0, 2); // no model

        let s = spend(&conn, 0, None, 5).unwrap();
        assert!((s.total_cost - 30.0).abs() < 1e-6, "total {}", s.total_cost);
        assert_eq!(s.tracked_calls, 1);
        assert_eq!(s.untracked_calls, 1);
        assert_eq!(s.top_threads.len(), 1);
        assert_eq!(s.top_threads[0].thread_id, tid);
        assert_eq!(s.top_threads[0].title.as_deref(), Some("pricey thread"));

        // Project scoping and a time window give the same totals here (all rows match).
        let scoped = spend(&conn, 50, Some("/p"), 5).unwrap();
        assert!((scoped.total_cost - 30.0).abs() < 1e-6);
        assert!(spend(&conn, 0, Some("/elsewhere"), 5)
            .unwrap()
            .by_model
            .is_empty());
        let cached = spend_cached(&conn, 0, None, 5).unwrap();
        assert!((cached.total_cost - s.total_cost).abs() < 1e-9);

        // The all-time aggregate reads only the covering usage index.
        let plan: String = conn
            .prepare(
                "EXPLAIN QUERY PLAN SELECT m.thread_id, m.model, SUM(m.input_tokens),
                        SUM(m.output_tokens), SUM(m.cache_write_tokens), SUM(m.cache_read_tokens),
                        COUNT(*)
                 FROM messages m WHERE m.model IS NOT NULL GROUP BY m.thread_id, m.model",
            )
            .unwrap()
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .map(|r| r.unwrap())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(plan.contains("COVERING INDEX idx_messages_usage"), "{plan}");

        // A sibling repo whose path starts with this one: `cal spend -p /p` still counts it,
        // the app's exact key does not.
        conn.execute(
            "INSERT INTO threads (source_id, external_id, title, project_path, created_at, updated_at)
             VALUES (?1, 't2', 'web thread', '/p-web', 1, 1)",
            [sid],
        )
        .unwrap();
        let web = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO messages (thread_id, seq, role, text, ts, model, input_tokens, output_tokens, cache_write_tokens, cache_read_tokens)
             VALUES (?1, 0, 'assistant', 'x', 100, 'claude-opus-4-8', 1000000, 0, 0, 0)",
            [web],
        )
        .unwrap();
        let sub = spend(&conn, 0, Some("/p"), 5).unwrap();
        assert!((sub.total_cost - 35.0).abs() < 1e-6, "{}", sub.total_cost);
        let exact = spend_matching(&conn, 0, Some("/p"), ProjectMatch::Exact, 5).unwrap();
        assert!(
            (exact.total_cost - 30.0).abs() < 1e-6,
            "{}",
            exact.total_cost
        );
        assert_eq!(exact.top_threads.len(), 1);
        // Both scopes stay on the covering usage index, all time and windowed.
        for (since, project, mode) in [
            (0, Some("/p"), ProjectMatch::Exact),
            (50, Some("/p"), ProjectMatch::Exact),
            (0, Some("/p"), ProjectMatch::Substring),
            (50, Some("/p"), ProjectMatch::Substring),
            (50, None, ProjectMatch::Exact),
        ] {
            let (sql, args) = usage_sql(since, project, mode);
            let refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
            let plan = conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap()
                .query_map(refs.as_slice(), |r| r.get::<_, String>(3))
                .unwrap()
                .map(|r| r.unwrap())
                .collect::<Vec<_>>()
                .join(" | ");
            assert!(plan.contains("COVERING INDEX idx_messages_usage"), "{plan}");
        }
    }

    /// The embedded table parses, and every entry is either fully priced or says why not.
    #[test]
    fn the_price_table_is_well_formed() {
        let file: PriceFile = serde_json::from_str(include_str!("../prices.json")).unwrap();
        assert!(!file.models.is_empty());
        assert_eq!(file.models.len(), PRICES.len());
        let mut families = HashSet::new();
        for e in &file.models {
            assert!(families.insert(e.family.as_str()), "duplicate {}", e.family);
            assert!(
                !(e.equals.is_empty() && e.prefix.is_empty() && e.contains.is_empty()),
                "{} matches nothing",
                e.family
            );
            match (e.price(), &e.unpriced) {
                (Some(p), None) => {
                    assert!(p.input > 0.0 && p.output > 0.0, "{}", e.family);
                    assert!(
                        p.cache_read <= p.input,
                        "{}: cache reads cost less",
                        e.family
                    );
                }
                (None, Some(why)) => assert!(!why.trim().is_empty(), "{}", e.family),
                _ => panic!("{} needs all four prices or an unpriced reason", e.family),
            }
            let text = format!("{:?}", e.unpriced);
            assert!(
                !text.contains('\u{2014}'),
                "no em-dashes in user-facing text"
            );
        }
    }

    /// Every model family an indexer writes maps to a price or to a deliberate "unpriced",
    /// never silently to "no price on file". The ids are real ones from each source's logs.
    #[test]
    fn every_model_family_the_indexers_emit_is_priced_or_unpriced_on_purpose() {
        let emitted: &[(&str, &[&str])] = &[
            (
                "claude_code",
                &[
                    "claude-opus-5-5",
                    "claude-opus-4-8",
                    "claude-fable-5-1",
                    "claude-sonnet-5",
                    "claude-sonnet-4-6",
                    "claude-haiku-4-5-20251001",
                    "claude-opus-4-1-20250805",
                    "claude-3-5-haiku-20241022",
                    "<synthetic>",
                ],
            ),
            (
                "codex",
                &[
                    "gpt-5.5",
                    "gpt-5.4",
                    "gpt-5.4-mini",
                    "gpt-5.3-codex",
                    "gpt-5.2-codex",
                    "gpt-5.1-codex",
                    "gpt-5.7",
                    "gpt-5-codex",
                    "gpt-5.1-codex-mini",
                    "gpt-5-mini",
                    "o3",
                    "o4-mini",
                    "codex-mini-latest",
                    "codex",
                ],
            ),
            (
                "copilot",
                &[
                    "gpt-4.1",
                    "gpt-4o",
                    "claude-sonnet-4",
                    "gemini-2.5-pro",
                    "auto",
                ],
            ),
            ("copilot_cli", &["gpt-5.3-codex", "claude-sonnet-4.5"]),
            (
                "gemini",
                &[
                    "gemini-2.5-pro",
                    "gemini-2.5-flash",
                    "gemini-2.5-flash-lite",
                    "gemini-3-pro-preview",
                    "gemini-3-flash-preview",
                    "gemini-3.1-pro-preview",
                    "gemini-3.1-flash-lite",
                    "gemini-3.5-flash-lite",
                    "gemini-3.6-flash",
                    "gemini-3.8-flash",
                    "gemini-3.9-ultra",
                ],
            ),
            (
                "qwen",
                &[
                    "qwen3-coder-plus",
                    "qwen3-coder-flash",
                    "coder-model",
                    "qwen3-max",
                ],
            ),
            (
                "opencode",
                &[
                    "claude-sonnet-4-5",
                    "gpt-5",
                    "qwen3-coder",
                    "kimi-k2",
                    "glm-4.6",
                    "grok-code",
                    "big-pickle",
                    "gpt-oss-120b",
                    "deepseek-chat",
                ],
            ),
        ];
        for (source, ids) in emitted {
            for id in *ids {
                assert_ne!(pricing(id), Pricing::Unknown, "{source}: {id}");
            }
        }
        // The specific id wins over its family, and open weights are never priced as GPT.
        assert_eq!(price_for("gpt-5-mini").unwrap().input, 0.25);
        assert_eq!(price_for("claude-opus-4-1").unwrap().input, 15.0);
        assert_eq!(price_for("claude-opus-4-8").unwrap().input, 5.0);
        assert!(matches!(pricing("gpt-oss-20b"), Pricing::Unpriced { .. }));
        assert!(matches!(pricing("<synthetic>"), Pricing::Unpriced { .. }));
        assert!(matches!(pricing("copilot/auto"), Pricing::Unpriced { .. }));
        assert_eq!(pricing("a-brand-new-lab-model"), Pricing::Unknown);
    }

    /// Each point release gets its own published price or an honest "unpriced", never its
    /// sibling's: gpt-5.5 costs four times gpt-5, and Gemini 3.6 Flash is not 3 Flash.
    #[test]
    fn point_releases_never_borrow_a_siblings_price() {
        let input = |id: &str| price_for(id).map(|p| p.input);
        assert_eq!(input("gpt-5"), Some(1.25));
        assert_eq!(input("gpt-5.1-codex"), Some(1.25));
        assert_eq!(input("gpt-5.2"), Some(1.75));
        assert_eq!(input("gpt-5.3-codex"), Some(1.75));
        assert_eq!(input("gpt-5.4"), Some(2.5));
        assert_eq!(input("gpt-5.4-mini"), Some(0.75));
        assert_eq!(input("gpt-5.5"), Some(5.0));
        assert_eq!(input("gpt-5.5-pro"), Some(30.0));
        assert_eq!(input("gpt-5.1-codex-mini"), Some(0.25));
        assert!(matches!(pricing("gpt-5.7-codex"), Pricing::Unpriced { .. }));
        assert_eq!(input("gemini-3.1-pro-preview"), Some(2.0));
        assert_eq!(input("gemini-3-flash-preview"), Some(0.5));
        assert_eq!(input("gemini-3.6-flash"), Some(0.75));
        assert_eq!(input("gemini-3.5-flash-lite"), Some(0.3));
        assert_eq!(input("gemini-2.0-flash"), Some(0.1));
        for unlisted in [
            "gemini-3.8-flash-lite",
            "gemini-2.0-flash-lite",
            "gemini-3.5-flash",
        ] {
            assert!(
                matches!(pricing(unlisted), Pricing::Unpriced { .. }),
                "{unlisted}"
            );
        }
        // Qwen's commercial models bill by prompt-length tier: no flat guess.
        for id in ["qwen3-coder-plus", "qwen3-max", "coder-model"] {
            assert!(matches!(pricing(id), Pricing::Unpriced { .. }), "{id}");
        }
    }

    #[test]
    fn tool_failures_read_from_each_agents_result_shape() {
        for failed in [
            "<tool_use_error>String to replace not found in file.</tool_use_error>",
            "Exit code 1\nerror[E0425]: cannot find value `x`",
            "Exit code: 2\nWall time: 0.1 seconds",
            "Process exited with code 101",
            r#"{"output":"boom","metadata":{"exit_code":1,"duration_seconds":0.2}}"#,
            "Error: ENOENT: no such file or directory",
            "TypeError: x is not a function",
            "Traceback (most recent call last):\n  File \"a.py\", line 1, in <module>",
        ] {
            assert!(tool_failed(failed), "{failed}");
        }
        for ok in [
            "Exit code 0",
            r#"{"output":"done","metadata":{"exit_code":0}}"#,
            "Compiling foo v0.1.0\nFinished",
            "src/errors.rs:12: fn is_error_line",
            "",
        ] {
            assert!(!tool_failed(ok), "{ok}");
        }
    }

    fn temp_db(tag: &str) -> Connection {
        let p = std::env::temp_dir().join(format!("calli_cost_{tag}_{}.db", std::process::id()));
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", p.display()));
        }
        crate::db::open(&p).unwrap()
    }

    fn plan(conn: &Connection, sql: &str, args: &[&dyn ToSql]) -> String {
        conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map(args, |r| r.get::<_, String>(3))
            .unwrap()
            .map(|r| r.unwrap())
            .collect::<Vec<_>>()
            .join(" | ")
    }

    /// A Claude Code session with a subagent, a long tail of turns and a failing tool loop;
    /// a second session in another repo; two commits sharing the first session.
    struct Fixture {
        conn: Connection,
        parent: i64,
        subagent: i64,
        other: i64,
    }

    fn fixture(tag: &str) -> Fixture {
        let conn = temp_db(tag);
        let sid: i64 = conn
            .query_row(
                "SELECT id FROM sources WHERE kind = 'claude_code'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let thread = |ext: &str, title: &str, project: &str, sub: bool| {
            conn.execute(
                "INSERT INTO threads (source_id, external_id, title, project_path, project_key,
                     created_at, updated_at, is_subagent)
                 VALUES (?1, ?2, ?3, ?4, ?4, 1, 2000000, ?5)",
                rusqlite::params![sid, ext, title, project, sub],
            )
            .unwrap();
            conn.last_insert_rowid()
        };
        let parent = thread("slug/sess.jsonl", "long session", "/r/app", false);
        let subagent = thread("slug/sess/subagents/agent-a.jsonl", "agent", "/r/app", true);
        let other = thread("slug/other.jsonl", "short session", "/r/web", false);
        let mut seq = std::collections::HashMap::<i64, i64>::new();
        let mut msg = |tid: i64,
                       role: &str,
                       text: &str,
                       tool: Option<&str>,
                       ts: i64,
                       usage: Option<(&str, i64, i64, i64, i64)>| {
            let s = seq.entry(tid).or_insert(0);
            conn.execute(
                "INSERT INTO messages (thread_id, seq, role, text, tool_name, ts, model,
                     input_tokens, output_tokens, cache_write_tokens, cache_read_tokens)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                rusqlite::params![
                    tid,
                    *s,
                    role,
                    text,
                    tool,
                    ts,
                    usage.map(|u| u.0),
                    usage.map(|u| u.1),
                    usage.map(|u| u.2),
                    usage.map(|u| u.3),
                    usage.map(|u| u.4),
                ],
            )
            .unwrap();
            *s += 1;
        };
        // 60 turns: each $0.10 of fresh input and $0.40 of cache reads on Opus ($5 / $0.5).
        for turn in 0..60 {
            let ts = 1_000_000 + turn * 1000;
            msg(parent, "user", "next step", None, ts, None);
            msg(
                parent,
                "assistant",
                "working",
                None,
                ts + 1,
                Some(("claude-opus-4-8", 20_000, 0, 0, 800_000)),
            );
        }
        // Bash fails three times running, then succeeds.
        for result in [
            "Exit code 1\nboom",
            "Exit code 1\nboom",
            "Exit code 2\nboom",
            "ok",
        ] {
            msg(
                parent,
                "assistant",
                "Bash: {}",
                Some("Bash"),
                1_100_000,
                None,
            );
            msg(parent, "tool", result, None, 1_100_001, None);
        }
        // The subagent: $3 of output on Opus.
        msg(
            subagent,
            "assistant",
            "sub",
            None,
            1_050_000,
            Some(("claude-opus-4-8", 0, 120_000, 0, 0)),
        );
        // Another repo, another day: $1 of Haiku input, plus an unpriced placeholder turn.
        msg(other, "user", "hi", None, 1_500_000, None);
        msg(
            other,
            "assistant",
            "hello",
            None,
            1_500_001,
            Some(("claude-haiku-4-5", 1_000_000, 0, 0, 0)),
        );
        msg(
            other,
            "assistant",
            "",
            None,
            1_500_002,
            Some(("<synthetic>", 0, 0, 0, 0)),
        );
        for (thread, sha, at) in [
            (parent, "aaaa1111", 1_200_000),
            (subagent, "aaaa1111", 1_200_000),
            (parent, "bbbb2222", 1_300_000),
            (other, "cccc3333", 1_600_000),
        ] {
            conn.execute(
                "INSERT INTO thread_commits (thread_id, sha, short_sha, subject, committed_at, overlap, created_at)
                 VALUES (?1, ?2, substr(?2, 1, 7), 'subject ' || ?2, ?3, 2, 0)",
                rusqlite::params![thread, sha, at],
            )
            .unwrap();
        }
        Fixture {
            conn,
            parent,
            subagent,
            other,
        }
    }

    fn query(by: CostBy) -> CostQuery<'static> {
        CostQuery {
            by,
            since: 0,
            project: None,
            mode: ProjectMatch::Substring,
            limit: 10,
        }
    }

    #[test]
    fn sessions_fold_in_subagents_and_carry_neutral_signals() {
        let f = fixture("sessions");
        let r = cost_report(&f.conn, &query(CostBy::Thread)).unwrap();
        // $30 (parent) + $3 (subagent) + $1 (other).
        assert!((r.total_cost - 34.0).abs() < 1e-6, "{}", r.total_cost);
        assert_eq!(r.untracked_calls, 1, "the <synthetic> turn");
        let rows = r.threads.unwrap();
        assert_eq!(rows.len(), 2, "the subagent is folded into its session");
        let s = &rows[0];
        assert_eq!(s.id, f.parent);
        assert!((s.cost - 33.0).abs() < 1e-6 && (s.subagent_cost - 3.0).abs() < 1e-6);
        assert_eq!((s.subagents, s.turns, s.calls), (1, 60, 61));
        let kinds: Vec<SignalKind> = s.signals.iter().map(|g| g.kind).collect();
        assert_eq!(
            kinds,
            vec![
                SignalKind::CacheReads,
                SignalKind::LongSession,
                SignalKind::ToolLoop
            ],
            "{:?}",
            s.signals
        );
        // Cache reads are $24 of the session's own $30; 10 of 60 turns come after turn 50.
        assert_eq!(s.signals[0].label, "Cache reads 80% of cost");
        assert_eq!(s.signals[1].label, "60 turns; 17% of cost after turn 50");
        assert_eq!(s.signals[2].label, "Bash failed 3 times in a row");
        assert_eq!(rows[1].id, f.other);
        assert!(rows[1].signals.is_empty());
        let synthetic = r
            .by_model
            .iter()
            .find(|m| m.model == "<synthetic>")
            .unwrap();
        assert!(!synthetic.priced && synthetic.unpriced_reason.is_some());
        let _ = f.subagent;
    }

    #[test]
    fn spend_by_source_adds_each_thread_to_its_agent() {
        let f = fixture("by_source");
        let codex: i64 = f
            .conn
            .query_row("SELECT id FROM sources WHERE kind = 'codex'", [], |r| {
                r.get(0)
            })
            .unwrap();
        f.conn
            .execute(
                "UPDATE threads SET source_id = ?1 WHERE id = ?2",
                [codex, f.other],
            )
            .unwrap();
        let s = spend_by_source(&f.conn, 0).unwrap();
        assert!((s.total_cost - 34.0).abs() < 1e-6, "{}", s.total_cost);
        let got: Vec<(&str, f64)> = s
            .by_source
            .iter()
            .map(|c| (c.source.as_str(), (c.cost * 100.0).round() / 100.0))
            .collect();
        // The subagent's $3 counts with Claude Code; the unpriced turn adds nothing.
        assert_eq!(got, vec![("claude_code", 33.0), ("codex", 1.0)]);
        assert_eq!(s.untracked_calls, 1);
        // A window after the Opus turns leaves only the other repo's usage.
        let late = spend_by_source(&f.conn, 1_400_000).unwrap();
        assert_eq!(late.by_source.len(), 1);
        assert_eq!(late.by_source[0].source, "codex");
    }

    #[test]
    fn a_subagent_heavy_session_says_so() {
        let f = fixture("subshare");
        f.conn
            .execute(
                "UPDATE messages SET output_tokens = 2000000 WHERE thread_id = ?1",
                [f.subagent],
            )
            .unwrap();
        let r = cost_report(&f.conn, &query(CostBy::Thread)).unwrap();
        let s = &r.threads.unwrap()[0];
        assert!(
            s.signals
                .iter()
                .any(|g| g.kind == SignalKind::Subagents && g.label == "Subagents 62% of cost"),
            "{:?}",
            s.signals
        );
    }

    /// A commit costs what its sessions cost in full; a session behind two commits counts
    /// toward both and marks them shared; a subagent linked to the same commit as its
    /// session is not counted twice.
    #[test]
    fn commits_carry_their_sessions_whole_cost() {
        let f = fixture("commits");
        let r = cost_report(&f.conn, &query(CostBy::Commit)).unwrap();
        let commits = r.commits.unwrap();
        let shas: Vec<&str> = commits.iter().map(|c| c.sha.as_str()).collect();
        assert_eq!(shas, vec!["bbbb2222", "aaaa1111", "cccc3333"]);
        let a = commits.iter().find(|c| c.sha == "aaaa1111").unwrap();
        assert_eq!(a.sessions.len(), 1);
        assert!((a.cost - 33.0).abs() < 1e-6, "{}", a.cost);
        assert!(a.shared && a.sessions[0].commits == 2);
        let c = commits.iter().find(|c| c.sha == "cccc3333").unwrap();
        assert!(!c.shared && (c.cost - 1.0).abs() < 1e-6);
        assert_eq!(c.project.as_deref(), Some("/r/web"));
        // The window bounds commit dates; the project scope narrows to the repo.
        let recent = CostQuery {
            since: 1_250_000,
            project: Some("/r/app"),
            ..query(CostBy::Commit)
        };
        let commits = cost_report(&f.conn, &recent).unwrap().commits.unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].sha, "bbbb2222");
        assert!(!commits[0].shared, "the other commit is outside the window");
        let gui = breakdown_cached(&f.conn, Some("/r/web")).unwrap();
        assert_eq!(gui.commits.len(), 1);
        assert_eq!(
            gui.days.len(),
            30,
            "every day of the window, zero ones included"
        );
    }

    #[test]
    fn projects_and_local_days_split_the_same_total() {
        let f = fixture("days");
        let r = cost_report(&f.conn, &query(CostBy::Project)).unwrap();
        let projects = r.projects.unwrap();
        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0].project.as_deref(), Some("/r/app"));
        assert!((projects[0].cost - 33.0).abs() < 1e-6);
        // The session and its subagent transcript are one session.
        assert_eq!(projects[0].threads, 1);
        let days = cost_report(&f.conn, &query(CostBy::Day))
            .unwrap()
            .days
            .unwrap();
        let total: f64 = days.iter().map(|d| d.cost).sum();
        assert!((total - 34.0).abs() < 1e-6, "{total}");
        assert!(days.windows(2).all(|w| w[0].day < w[1].day), "oldest first");
        use chrono::TimeZone;
        let last = chrono::Local.timestamp_opt(1_500_001, 0).unwrap();
        assert_eq!(
            days.last().unwrap().day,
            last.format("%Y-%m-%d").to_string()
        );
    }

    /// A quiet call (no stored result) does not shift the run; another tool ends it.
    #[test]
    fn a_failing_run_is_one_tool_back_to_back() {
        let f = fixture("loops");
        let rows = |items: &[(&str, Option<&str>, &str)]| {
            f.conn
                .execute("DELETE FROM messages WHERE thread_id = ?1", [f.other])
                .unwrap();
            for (i, (role, tool, text)) in items.iter().enumerate() {
                f.conn
                    .execute(
                        "INSERT INTO messages (thread_id, seq, role, text, tool_name, ts)
                         VALUES (?1, ?2, ?3, ?4, ?5, 1)",
                        rusqlite::params![f.other, i as i64, role, text, tool],
                    )
                    .unwrap();
            }
            failing_tool_run(&f.conn, f.other).unwrap()
        };
        let fail = ("tool", None, "Exit code 1");
        let bash = ("assistant", Some("Bash"), "Bash: {}");
        let edit = ("assistant", Some("Edit"), "Edit: {}");
        assert_eq!(
            rows(&[bash, fail, bash, bash, fail, bash, fail]),
            Some(("Bash".to_string(), 3))
        );
        assert_eq!(rows(&[bash, fail, bash, fail, edit, fail]), None);
        assert_eq!(
            rows(&[
                bash,
                fail,
                bash,
                ("tool", None, "ok"),
                bash,
                fail,
                bash,
                fail
            ]),
            None
        );
    }

    /// The breakdown queries read indexes only: no message row (and its overflow pages).
    #[test]
    fn the_breakdown_queries_stay_on_covering_indexes() {
        let f = fixture("plans");
        for (since, project) in [(0, None), (50, None), (50, Some("/r/app"))] {
            let (sql, args) = day_sql(since, project, ProjectMatch::Exact);
            let refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
            let p = plan(&f.conn, &sql, &refs);
            assert!(p.contains("COVERING INDEX idx_messages_usage"), "{p}");
        }
        for sql in [USAGE_FROM_SQL, THREAD_USAGE_SQL] {
            let args: [&dyn ToSql; 2] = [&1i64, &0i64];
            let p = plan(&f.conn, sql, &args[..sql.matches('?').count()]);
            assert!(p.contains("COVERING INDEX idx_messages_usage"), "{p}");
        }
        for sql in [USER_TURNS_SQL, TURN_TS_SQL] {
            let args: [&dyn ToSql; 2] = [&1i64, &50i64];
            let p = plan(&f.conn, sql, &args[..sql.matches('?').count()]);
            assert!(p.contains("COVERING INDEX idx_messages_thread_ts"), "{p}");
        }
        let p = plan(&f.conn, TOOL_ROWS_SQL, &[&1i64]);
        assert!(p.contains("sqlite_autoindex_messages_1"), "{p}");
        let p = plan(&f.conn, crate::indexer::SUBAGENT_PARENTS_SQL, &[]);
        assert!(p.contains("SEARCH p USING"), "{p}");
    }
}
