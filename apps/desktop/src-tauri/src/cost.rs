//! Spend layer: turn the captured per-message token usage (migration 0022) into dollars. The
//! index is the only place that sees model spend across every tool, so this is the one view that
//! can answer "what did this actually cost me?". Prices are approximate published list rates
//! ($ per million tokens) and are clearly an estimate, not a billing record.

use anyhow::Result;
use rusqlite::{Connection, ToSql};
use serde::Serialize;

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
    if m.contains("opus") {
        return Some(ModelPrice {
            input: 15.0,
            output: 75.0,
            cache_write: 18.75,
            cache_read: 1.5,
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
            input: 0.8,
            output: 4.0,
            cache_write: 1.0,
            cache_read: 0.08,
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
#[derive(Debug, Serialize)]
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
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadCost {
    pub thread_id: i64,
    pub title: Option<String>,
    pub project: Option<String>,
    pub cost: f64,
}

/// The cost x-ray: total spend, a per-model breakdown, and the priciest threads.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Spend {
    pub total_cost: f64,
    pub tracked_calls: i64,
    pub untracked_calls: i64,
    pub by_model: Vec<ModelSpend>,
    pub top_threads: Vec<ThreadCost>,
}

fn project_clause(project: Option<&str>, args: &mut Vec<Box<dyn ToSql>>) -> String {
    match project.filter(|p| !p.is_empty()) {
        Some(p) => {
            args.push(Box::new(format!("%{p}%")));
            format!(
                " AND COALESCE(t.project_key, t.project_path) LIKE ?{}",
                args.len()
            )
        }
        None => String::new(),
    }
}

/// Compute spend across messages with captured usage since `since` (epoch secs), optionally
/// scoped to a project-path substring.
pub fn spend(conn: &Connection, since: i64, project: Option<&str>, top_n: usize) -> Result<Spend> {
    // Per-model token totals.
    let mut args: Vec<Box<dyn ToSql>> = vec![Box::new(since)];
    let proj = project_clause(project, &mut args);
    let sql = format!(
        "SELECT m.model, SUM(m.input_tokens), SUM(m.output_tokens),
                SUM(m.cache_write_tokens), SUM(m.cache_read_tokens), COUNT(*)
         FROM messages m JOIN threads t ON t.id = m.thread_id
         WHERE m.model IS NOT NULL AND m.ts >= ?1{proj}
         GROUP BY m.model"
    );
    let arg_refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(arg_refs.as_slice(), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1).unwrap_or(0),
            r.get::<_, i64>(2).unwrap_or(0),
            r.get::<_, i64>(3).unwrap_or(0),
            r.get::<_, i64>(4).unwrap_or(0),
            r.get::<_, i64>(5)?,
        ))
    })?;

    let mut by_model = Vec::new();
    let mut total_cost = 0.0;
    let mut tracked_calls = 0;
    let mut untracked_calls = 0;
    for row in rows {
        let (model, input, output, cw, cr, calls) = row?;
        match price_for(&model) {
            Some(p) => {
                let cost = cost_of(input, output, cw, cr, &p);
                total_cost += cost;
                tracked_calls += calls;
                by_model.push(ModelSpend {
                    model,
                    cost,
                    input,
                    output,
                    cache_read: cr,
                    calls,
                    priced: true,
                });
            }
            None => {
                untracked_calls += calls;
                by_model.push(ModelSpend {
                    model,
                    cost: 0.0,
                    input,
                    output,
                    cache_read: cr,
                    calls,
                    priced: false,
                });
            }
        }
    }
    by_model.sort_by(|a, b| b.cost.total_cmp(&a.cost));

    Ok(Spend {
        total_cost,
        tracked_calls,
        untracked_calls,
        by_model,
        top_threads: top_threads(conn, since, project, top_n)?,
    })
}

/// The priciest threads (summing each thread's per-model costs).
fn top_threads(
    conn: &Connection,
    since: i64,
    project: Option<&str>,
    top_n: usize,
) -> Result<Vec<ThreadCost>> {
    let mut args: Vec<Box<dyn ToSql>> = vec![Box::new(since)];
    let proj = project_clause(project, &mut args);
    let sql = format!(
        "SELECT m.thread_id, t.title, COALESCE(t.project_key, t.project_path), m.model,
                SUM(m.input_tokens), SUM(m.output_tokens),
                SUM(m.cache_write_tokens), SUM(m.cache_read_tokens)
         FROM messages m JOIN threads t ON t.id = m.thread_id
         WHERE m.model IS NOT NULL AND m.ts >= ?1{proj}
         GROUP BY m.thread_id, m.model"
    );
    let arg_refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(arg_refs.as_slice(), |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, i64>(4).unwrap_or(0),
            r.get::<_, i64>(5).unwrap_or(0),
            r.get::<_, i64>(6).unwrap_or(0),
            r.get::<_, i64>(7).unwrap_or(0),
        ))
    })?;

    use std::collections::HashMap;
    let mut by_thread: HashMap<i64, ThreadCost> = HashMap::new();
    for row in rows {
        let (tid, title, project, model, input, output, cw, cr) = row?;
        let Some(p) = price_for(&model) else { continue };
        let cost = cost_of(input, output, cw, cr, &p);
        let e = by_thread.entry(tid).or_insert_with(|| ThreadCost {
            thread_id: tid,
            title,
            project,
            cost: 0.0,
        });
        e.cost += cost;
    }
    let mut out: Vec<ThreadCost> = by_thread.into_values().filter(|t| t.cost > 0.0).collect();
    out.sort_by(|a, b| b.cost.total_cmp(&a.cost));
    out.truncate(top_n);
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
    }

    #[test]
    fn cost_math() {
        let p = price_for("claude-opus-4-8").unwrap();
        // 1M input @ $15 + 1M output @ $75 = $90.
        assert!((cost_of(1_000_000, 1_000_000, 0, 0, &p) - 90.0).abs() < 1e-6);
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
        ); // $90
        ins("assistant", Some("mystery-model"), 1_000_000, 0, 1); // untracked
        ins("user", None, 0, 0, 2); // no model

        let s = spend(&conn, 0, None, 5).unwrap();
        assert!((s.total_cost - 90.0).abs() < 1e-6, "total {}", s.total_cost);
        assert_eq!(s.tracked_calls, 1);
        assert_eq!(s.untracked_calls, 1);
        assert_eq!(s.top_threads.len(), 1);
        assert_eq!(s.top_threads[0].thread_id, tid);
    }
}
