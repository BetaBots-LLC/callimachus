//! Spend layer: turn the captured per-message token usage (migration 0022) into dollars. The
//! index is the only place that sees model spend across every tool, so this is the one view that
//! can answer "what did this actually cost me?". Prices are approximate published list rates
//! ($ per million tokens) and are clearly an estimate, not a billing record.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, ToSql};
use serde::Serialize;
use std::collections::HashMap;

/// Published list price for a model, in USD per million tokens.
#[derive(Debug, Clone, Copy)]
pub struct ModelPrice {
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub cache_read: f64,
}

/// Approximate $/Mtok for a model id, matched by family (ids vary by date/suffix). `None` = we
/// don't have a price, so its tokens are counted as "untracked" rather than guessed.
pub fn price_for(model: &str) -> Option<ModelPrice> {
    let m = model.to_ascii_lowercase();
    // Anthropic (Claude): cache write ≈ 1.25× input, cache read ≈ 0.1× input.
    // Fable/Mythos are the top tier and share list pricing.
    if m.contains("fable") || m.contains("mythos") {
        return Some(ModelPrice {
            input: 10.0,
            output: 50.0,
            cache_write: 12.5,
            cache_read: 1.0,
        });
    }
    if m.contains("opus") {
        return Some(ModelPrice {
            input: 5.0,
            output: 25.0,
            cache_write: 6.25,
            cache_read: 0.5,
        });
    }
    if m.contains("sonnet") {
        return Some(ModelPrice {
            input: 3.0,
            output: 15.0,
            cache_write: 3.75,
            cache_read: 0.3,
        });
    }
    if m.contains("haiku") {
        return Some(ModelPrice {
            input: 1.0,
            output: 5.0,
            cache_write: 1.25,
            cache_read: 0.1,
        });
    }
    // OpenAI: no separate cache-write charge; cached input is discounted.
    if m.contains("gpt-5") || m.contains("gpt5") {
        return Some(ModelPrice {
            input: 1.25,
            output: 10.0,
            cache_write: 1.25,
            cache_read: 0.125,
        });
    }
    if m.contains("gpt") || m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4") {
        return Some(ModelPrice {
            input: 2.5,
            output: 10.0,
            cache_write: 2.5,
            cache_read: 1.25,
        });
    }
    None
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
/// `search::cache`). For the app's Stats view; `cal spend` and tests read live.
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
        spend(conn, since, project, top_n)
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
/// time), optionally scoped to a project-path substring.
///
/// One aggregate pass per (thread, model), served by the covering usage index (migration
/// 0026), then rolled up per model and per thread in Rust. This used to be two separate
/// GROUP BY scans that each read every message's trailing usage columns.
pub fn spend(conn: &Connection, since: i64, project: Option<&str>, top_n: usize) -> Result<Spend> {
    let rows = usage_by_thread_model(conn, since, project)?;

    let mut per_model: HashMap<String, ModelSpend> = HashMap::new();
    let mut per_thread: HashMap<i64, f64> = HashMap::new();
    let mut total_cost = 0.0;
    let mut tracked_calls = 0;
    let mut untracked_calls = 0;
    for u in rows {
        let price = price_for(&u.model);
        let cost = price.map_or(0.0, |p| {
            cost_of(u.input, u.output, u.cache_write, u.cache_read, &p)
        });
        if price.is_some() {
            total_cost += cost;
            tracked_calls += u.calls;
            *per_thread.entry(u.thread_id).or_insert(0.0) += cost;
        } else {
            untracked_calls += u.calls;
        }
        let m = per_model
            .entry(u.model.clone())
            .or_insert_with(|| ModelSpend {
                model: u.model.clone(),
                cost: 0.0,
                input: 0,
                output: 0,
                cache_read: 0,
                calls: 0,
                priced: price.is_some(),
            });
        m.cost += cost;
        m.input += u.input;
        m.output += u.output;
        m.cache_read += u.cache_read;
        m.calls += u.calls;
    }
    let mut by_model: Vec<ModelSpend> = per_model.into_values().collect();
    by_model.sort_by(|a, b| {
        b.cost
            .total_cmp(&a.cost)
            .then_with(|| a.model.cmp(&b.model))
    });

    Ok(Spend {
        total_cost,
        tracked_calls,
        untracked_calls,
        by_model,
        top_threads: top_threads(conn, per_thread, top_n)?,
    })
}

/// SUM the usage columns per (thread, model). The `ts` filter is only added for a real
/// window: `ts >= 0` would steer the planner onto the timestamp index and away from the
/// covering usage index.
fn usage_by_thread_model(
    conn: &Connection,
    since: i64,
    project: Option<&str>,
) -> Result<Vec<Usage>> {
    let mut args: Vec<Box<dyn ToSql>> = Vec::new();
    let mut filters = String::new();
    if since > 0 {
        args.push(Box::new(since));
        filters.push_str(&format!(" AND m.ts >= ?{}", args.len()));
    }
    let join = match project.filter(|p| !p.is_empty()) {
        Some(p) => {
            args.push(Box::new(format!("%{p}%")));
            filters.push_str(&format!(
                " AND COALESCE(t.project_key, t.project_path) LIKE ?{}",
                args.len()
            ));
            " JOIN threads t ON t.id = m.thread_id"
        }
        None => "",
    };
    let sql = format!(
        "SELECT m.thread_id, m.model, SUM(m.input_tokens), SUM(m.output_tokens),
                SUM(m.cache_write_tokens), SUM(m.cache_read_tokens), COUNT(*)
         FROM messages m{join}
         WHERE m.model IS NOT NULL{filters}
         GROUP BY m.thread_id, m.model"
    );
    let arg_refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
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
    }
}
