//! Recall and the contradiction guard when there is nothing to answer with: the embedding
//! model is not on this machine (no query vector), or nothing matched in a scope that was never
//! distilled. One place decides what to show instead, so `cal`, the MCP server and the app
//! degrade the same way. They used not to: without the model MCP fell back to keyword
//! sessions, `cal decisions` printed nothing after saying it was "using keyword matches only",
//! and the app returned an empty list.
//!
//! The caller embeds the query BEFORE taking the database (inference must never hold the
//! lock) and passes the vector, or None when there is none.

use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;

use super::{check_contradiction, recall, RecallHit, DISTILLABLE};
use crate::scope::ProjectScope;
use crate::search::{self, SearchFilters};

/// Most sessions a fallback lists.
pub const CLOSEST_SESSIONS: usize = 3;

/// Longest snippet a fallback session carries, in chars.
pub const CLOSEST_SNIPPET_CHARS: usize = 240;

/// Message hits fetched per listed session: the search ranks messages, and one long thread can
/// hold the top few.
const HITS_PER_SESSION: u32 = 3;

/// Distillation coverage of a scope, over the threads worth distilling ([`DISTILLABLE`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DistillCoverage {
    pub distilled_threads: i64,
    pub threads: i64,
}

// A past session that may hold the answer, found by search instead of by memory; `snippet`
// is its best-matching message, `role: text`, redacted and clipped. Plain comments, not doc
// comments: a doc comment reorders the generated JSON Schema, and MCP's checked-in
// `schemas/mcp/recall_*.json` describe this type as it was when it lived in mcp_server.rs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClosestSession {
    pub id: i64,
    pub title: Option<String>,
    pub snippet: String,
}

/// What a recall or a contradiction check found, including the degraded states.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum RecallOutcome {
    /// The query ran (or was blank): these hits, possibly none.
    Hits { hits: Vec<RecallHit> },
    /// Nothing matched in a scope that holds no decision or gotcha at all and has undistilled
    /// threads, where an empty list would read as "never decided" when it means "never
    /// distilled" (117 of ~21,300 threads distilled on the author's index): the coverage and
    /// the closest sessions instead. `distill_on` says whether distillation is already on, so
    /// a caller advises turning it on only when it is off.
    #[serde(rename_all = "camelCase")]
    Thin {
        coverage: DistillCoverage,
        sessions: Vec<ClosestSession>,
        distill_on: bool,
    },
    /// No query vector because the embedding model is not on this machine (`cal` and the MCP
    /// server never download it): the closest sessions by keyword search, so the answer is
    /// leads rather than an empty list that reads as "nothing decided".
    NoModel { sessions: Vec<ClosestSession> },
}

/// `recall` of `kind` facts for `query` ([`recall`]), or the fallback when there is no hit.
#[allow(clippy::too_many_arguments)]
pub fn recall_or_fallback(
    conn: &Connection,
    query: &str,
    qv: Option<&[f32]>,
    kind: &str,
    project: Option<&str>,
    limit: usize,
    include_superseded: bool,
) -> Result<RecallOutcome> {
    let hits = match qv {
        Some(v) => recall(conn, v, kind, project, limit, include_superseded)?,
        None => Vec::new(),
    };
    settle(conn, query, qv, project, hits, model_missing(qv))
}

/// No vector, and not for want of a query: the model is not on this machine.
fn model_missing(qv: Option<&[f32]>) -> bool {
    qv.is_none() && !crate::embed::model_is_cached()
}

/// The contradiction guard for `proposal` ([`check_contradiction`]), or the fallback when it
/// returns nothing.
pub fn check_or_fallback(
    conn: &Connection,
    proposal: &str,
    qv: Option<&[f32]>,
    project: Option<&str>,
    k: usize,
) -> Result<RecallOutcome> {
    let hits = match qv {
        Some(v) => check_contradiction(conn, v, project, k)?,
        None => Vec::new(),
    };
    settle(conn, proposal, qv, project, hits, model_missing(qv))
}

/// Hits stand. With none: a thin scope gets its closest sessions (by meaning when there is a
/// vector, else by keyword); without a vector because the model is missing, any other scope
/// gets keyword leads; a blank query (the model is here) stays an empty answer.
fn settle(
    conn: &Connection,
    query: &str,
    qv: Option<&[f32]>,
    project: Option<&str>,
    hits: Vec<RecallHit>,
    model_missing: bool,
) -> Result<RecallOutcome> {
    if !hits.is_empty() || (qv.is_none() && !model_missing) {
        return Ok(RecallOutcome::Hits { hits });
    }
    if let Some(thin) = thin_memory(conn, query, qv, project)? {
        return Ok(thin);
    }
    if model_missing {
        let sessions = closest_sessions(conn, query, None, project)?;
        return Ok(RecallOutcome::NoModel { sessions });
    }
    Ok(RecallOutcome::Hits { hits })
}

/// [`RecallOutcome::Thin`] for a scope (a project fragment, as `recall` scopes it, or every
/// project), or None whenever the scope has any decision or gotcha (an active repo always has
/// threads waiting, so a coverage test alone would replace every "no conflict" answer of
/// check_decision with leads that read like conflicts), when every distillable thread is
/// distilled, or when there are none, so an empty list stays meaningful.
pub fn thin_memory(
    conn: &Connection,
    query: &str,
    qv: Option<&[f32]>,
    project: Option<&str>,
) -> Result<Option<RecallOutcome>> {
    let project = project.map(str::trim).filter(|p| !p.is_empty());
    if scope_has_knowledge(conn, project)? {
        return Ok(None);
    }
    let coverage = distill_coverage(conn, project)?;
    if coverage.threads == 0 || coverage.distilled_threads >= coverage.threads {
        return Ok(None);
    }
    let distill_on = super::app_config::get_flag(conn, "knowledge.enabled")?;
    let sessions = closest_sessions(conn, query, qv, project)?;
    Ok(Some(RecallOutcome::Thin {
        coverage,
        sessions,
        distill_on,
    }))
}

/// The [`CLOSEST_SESSIONS`] sessions that best match `query` in `project` (a fragment): by
/// the hybrid search when the caller has the vector (one more KNN), else by keyword alone.
/// Each with its best message as the snippet, stripped of the FTS match markers, redacted and
/// clipped.
pub fn closest_sessions(
    conn: &Connection,
    query: &str,
    qv: Option<&[f32]>,
    project: Option<&str>,
) -> Result<Vec<ClosestSession>> {
    let filters = SearchFilters {
        project: project.map(str::to_string),
        limit: Some(CLOSEST_SESSIONS as u32 * HITS_PER_SESSION),
        ..Default::default()
    };
    let hits = match qv {
        Some(v) => search::hybrid_vec(conn, query, Some(v), &filters)?,
        None => search::search(conn, query, &filters)?,
    };
    let mut out: Vec<ClosestSession> = Vec::with_capacity(CLOSEST_SESSIONS);
    for h in hits {
        if out.len() >= CLOSEST_SESSIONS {
            break;
        }
        if out.iter().any(|s| s.id == h.thread_id) {
            continue;
        }
        // Snippets come straight from the FTS index: its \u{1}/\u{2} match sentinels would
        // reach an agent as raw control bytes, and the index is not redacted at the source.
        let plain = h.snippet.replace(['\u{1}', '\u{2}'], "");
        let snippet = format!(
            "{}: {}",
            h.role,
            crate::context::redact_secrets(plain.trim())
        );
        out.push(ClosestSession {
            id: h.thread_id,
            title: h.title,
            snippet: clip_chars(&snippet, CLOSEST_SNIPPET_CHARS),
        });
    }
    Ok(out)
}

/// `s` cut to at most `max` chars, ending in an ellipsis when it was cut.
pub fn clip_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Distillation coverage for a recall scope. Runs only on an empty recall; a scan of the
/// threads table, about 2 ms on a 21,289-thread index.
fn distill_coverage(conn: &Connection, project: Option<&str>) -> Result<DistillCoverage> {
    let mut sql = format!(
        "SELECT COUNT(*), COALESCE(SUM(knowledge_extracted = 1), 0) FROM threads
         WHERE {DISTILLABLE}"
    );
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(cond) = ProjectScope::fragment(project).sql("threads", &mut args) {
        sql.push_str(&format!(" AND {cond}"));
    }
    let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    Ok(conn.query_row(&sql, refs.as_slice(), |r| {
        Ok(DistillCoverage {
            threads: r.get(0)?,
            distilled_threads: r.get(1)?,
        })
    })?)
}

/// Whether a recall scope holds any visible decision or gotcha. Runs only on an empty recall;
/// an EXISTS that stops at the first fact.
fn scope_has_knowledge(conn: &Connection, project: Option<&str>) -> Result<bool> {
    let mut sql = String::from(
        "SELECT EXISTS(SELECT 1 FROM facts f JOIN threads t ON t.id = f.thread_id
         WHERE f.kind IN ('decision', 'gotcha') AND f.hidden = 0",
    );
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(cond) = ProjectScope::fragment(project).sql("t", &mut args) {
        sql.push_str(&format!(" AND {cond}"));
    }
    sql.push(')');
    let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    Ok(conn.query_row(&sql, refs.as_slice(), |r| r.get(0))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::{source_id, upsert_thread, ParsedMessage, ParsedThread};

    fn temp_db() -> Connection {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "calli_fallback_{}_{}.db",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        crate::db::open(&p).unwrap()
    }

    /// A thread worth distilling ([`DISTILLABLE`] wants 4+ messages) in `project`.
    fn seed(conn: &mut Connection, ext: &str, project: &str, title: &str, text: &str) {
        let sid = source_id(conn, "claude_code").unwrap();
        let messages = (0..4)
            .map(|i| ParsedMessage {
                role: if i % 2 == 0 { "user" } else { "assistant" }.into(),
                text: format!("{text} (turn {i})"),
                tool_name: None,
                ts: Some(100 + i),
            })
            .collect();
        upsert_thread(
            conn,
            sid,
            &ParsedThread {
                external_id: ext.into(),
                title: Some(title.into()),
                project_path: Some(project.into()),
                messages,
                ..Default::default()
            },
        )
        .unwrap();
    }

    fn titles(sessions: &[ClosestSession]) -> Vec<&str> {
        sessions.iter().filter_map(|s| s.title.as_deref()).collect()
    }

    /// Without the model, a scope with memory still gets keyword leads (it used to get
    /// nothing in `cal` and the app), an undistilled one its coverage too, and a blank query
    /// with the model present stays an empty answer. The project fragment's `_` is literal.
    #[test]
    fn without_the_model_every_scope_gets_keyword_leads() {
        let mut conn = temp_db();
        seed(
            &mut conn,
            "a",
            "/r/my_app",
            "postgres migration plan",
            "move the index to postgres",
        );
        seed(
            &mut conn,
            "b",
            "/r/myXapp",
            "postgres elsewhere",
            "postgres in the other repo",
        );

        match settle(&conn, "postgres", None, Some("my_app"), Vec::new(), true).unwrap() {
            RecallOutcome::Thin {
                coverage,
                sessions,
                distill_on,
            } => {
                assert_eq!(
                    coverage,
                    DistillCoverage {
                        distilled_threads: 0,
                        threads: 1
                    }
                );
                assert_eq!(titles(&sessions), ["postgres migration plan"]);
                assert!(sessions[0].snippet.starts_with("user: "), "{sessions:?}");
                assert!(!distill_on);
            }
            other => panic!("{other:?}"),
        }

        crate::knowledge::record_fact(&conn, "/r/my_app", "decision", "Keep sqlite", None, 10)
            .unwrap();
        match settle(&conn, "postgres", None, Some("my_app"), Vec::new(), true).unwrap() {
            RecallOutcome::NoModel { sessions } => {
                assert_eq!(titles(&sessions), ["postgres migration plan"]);
            }
            other => panic!("{other:?}"),
        }
        // Unscoped: both repos' sessions.
        match settle(&conn, "postgres", None, None, Vec::new(), true).unwrap() {
            RecallOutcome::NoModel { sessions } => assert_eq!(sessions.len(), 2),
            other => panic!("{other:?}"),
        }

        // A blank query with the model here: no leads.
        assert!(matches!(
            settle(&conn, "", None, Some("my_app"), Vec::new(), false).unwrap(),
            RecallOutcome::Hits { hits } if hits.is_empty()
        ));
        // A vector that matches nothing in a scope with memory: an empty answer ("no
        // conflict" for the guard), not leads that read like one.
        let mut qv = vec![0.0_f32; crate::embed::DIM];
        qv[0] = 1.0;
        assert!(matches!(
            check_or_fallback(&conn, "postgres", Some(&qv), Some("my_app"), 8).unwrap(),
            RecallOutcome::Hits { hits } if hits.is_empty()
        ));
    }

    #[test]
    fn closest_session_snippets_are_capped() {
        let long = "é".repeat(500);
        let clipped = clip_chars(&long, CLOSEST_SNIPPET_CHARS);
        assert_eq!(clipped.chars().count(), CLOSEST_SNIPPET_CHARS);
        assert!(clipped.ends_with('…'));
        assert_eq!(clip_chars("short", CLOSEST_SNIPPET_CHARS), "short");
    }
}
