//! Full-text search over indexed messages (SQLite FTS5 + BM25) plus the read
//! queries the UI needs: recent threads and a single thread's messages.

use crate::embed::{self, Embedder};
use anyhow::Result;
use rusqlite::{Connection, ToSql};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Optional filters applied alongside the text query.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SearchFilters {
    pub sources: Vec<String>,    // source kinds; empty = all
    pub project: Option<String>, // substring match on project_path
    pub after: Option<i64>,      // epoch seconds, inclusive
    pub before: Option<i64>,     // epoch seconds, inclusive
    pub limit: Option<u32>,
    pub include_subagents: bool, // default false: hide Claude Code subagent transcripts
    pub hybrid: bool,            // default false: fuse keyword + semantic results
}

/// One message-level search hit.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub thread_id: i64,
    pub message_id: i64,
    pub source: String,
    pub title: Option<String>,
    pub project_path: Option<String>,
    pub role: String,
    pub snippet: String, // HTML with <mark> around matches
    pub ts: Option<i64>,
}

/// A thread summary row for lists.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSummary {
    pub id: i64,
    pub source: String,
    pub title: Option<String>,
    pub project_path: Option<String>,
    pub message_count: i64,
    pub updated_at: Option<i64>,
}

/// A single message in the thread viewer.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageRow {
    pub id: i64,
    pub role: String,
    pub text: String,
    pub tool_name: Option<String>,
    pub ts: Option<i64>,
}

/// Full thread detail.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadDetail {
    pub id: i64,
    pub source: String,
    pub external_id: String,
    pub title: Option<String>,
    pub project_path: Option<String>,
    pub git_branch: Option<String>,
    pub created_at: Option<i64>,
    pub updated_at: Option<i64>,
    pub messages: Vec<MessageRow>,
}

/// Escape a user query into a safe FTS5 MATCH string: each whitespace-separated
/// token becomes a quoted term (implicit AND). Prevents syntax errors on stray
/// operators/quotes while still doing sensible multi-term search.
fn to_fts_query(raw: &str) -> Option<String> {
    let terms: Vec<String> = raw
        .split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" "))
    }
}

/// Run a full-text search. Empty query returns nothing (use recent_threads instead).
pub fn search(conn: &Connection, query: &str, filters: &SearchFilters) -> Result<Vec<SearchHit>> {
    let Some(match_query) = to_fts_query(query) else {
        return Ok(Vec::new());
    };
    let limit = filters.limit.unwrap_or(100).min(500) as i64;

    // Use control-char sentinels (\u{1}/\u{2}) for match boundaries, not HTML tags:
    // snippet() does not escape the body, so the frontend HTML-escapes the text and
    // only then swaps the sentinels for <mark> — preventing injection from indexed
    // content (a thread might literally contain "<script>").
    let mut sql = String::from(
        "SELECT t.id, m.id, s.kind, t.title, t.project_path, m.role,
                snippet(messages_fts, 0, char(1), char(2), '…', 12), m.ts
         FROM messages_fts
         JOIN messages m ON m.id = messages_fts.rowid
         JOIN threads t ON t.id = m.thread_id
         JOIN sources s ON s.id = t.source_id
         WHERE messages_fts MATCH ?1",
    );
    if !filters.include_subagents {
        sql.push_str(" AND t.is_subagent = 0");
    }
    let mut args: Vec<Box<dyn ToSql>> = vec![Box::new(match_query)];

    if !filters.sources.is_empty() {
        let placeholders: Vec<String> = filters
            .sources
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 2))
            .collect();
        sql.push_str(&format!(" AND s.kind IN ({})", placeholders.join(", ")));
        for src in &filters.sources {
            args.push(Box::new(src.clone()));
        }
    }
    if let Some(project) = &filters.project {
        args.push(Box::new(format!("%{project}%")));
        sql.push_str(&format!(" AND t.project_path LIKE ?{}", args.len()));
    }
    if let Some(after) = filters.after {
        args.push(Box::new(after));
        sql.push_str(&format!(" AND m.ts >= ?{}", args.len()));
    }
    if let Some(before) = filters.before {
        args.push(Box::new(before));
        sql.push_str(&format!(" AND m.ts <= ?{}", args.len()));
    }
    sql.push_str(" ORDER BY bm25(messages_fts) LIMIT ?");
    args.push(Box::new(limit));

    let args_ref: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(args_ref.as_slice(), |r| {
        Ok(SearchHit {
            thread_id: r.get(0)?,
            message_id: r.get(1)?,
            source: r.get(2)?,
            title: r.get(3)?,
            project_path: r.get(4)?,
            role: r.get(5)?,
            snippet: r.get(6)?,
            ts: r.get(7)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Hybrid search: fuse keyword (FTS5/BM25) and semantic (cosine) result lists with
/// Reciprocal Rank Fusion. Keyword-matched hits keep their highlighted snippet;
/// semantic-only hits get a plain leading-text snippet.
pub fn hybrid(
    conn: &Connection,
    embedder: &Embedder,
    query: &str,
    filters: &SearchFilters,
) -> Result<Vec<SearchHit>> {
    const RRF_K: f32 = 60.0;
    let limit = filters.limit.unwrap_or(100).min(500) as usize;

    let fts = search(conn, query, filters)?;
    let sem = embed::semantic_search(
        conn,
        embedder,
        query,
        filters.include_subagents,
        &filters.sources,
        limit.max(50),
    )?;

    let mut scores: HashMap<i64, f32> = HashMap::new();
    for (rank, h) in fts.iter().enumerate() {
        *scores.entry(h.message_id).or_default() += 1.0 / (RRF_K + rank as f32 + 1.0);
    }
    for (rank, (id, _)) in sem.iter().enumerate() {
        *scores.entry(*id).or_default() += 1.0 / (RRF_K + rank as f32 + 1.0);
    }

    let mut ranked: Vec<(i64, f32)> = scores.into_iter().collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    ranked.truncate(limit);

    let fts_by_id: HashMap<i64, &SearchHit> = fts.iter().map(|h| (h.message_id, h)).collect();
    let mut out = Vec::with_capacity(ranked.len());
    for (id, _) in ranked {
        if let Some(h) = fts_by_id.get(&id) {
            out.push((*h).clone());
        } else if let Some(h) = hit_for_message(conn, id)? {
            out.push(h);
        }
    }
    Ok(out)
}

/// Build a SearchHit for a message that matched only semantically (plain snippet).
fn hit_for_message(conn: &Connection, message_id: i64) -> Result<Option<SearchHit>> {
    let hit = conn
        .query_row(
            "SELECT t.id, m.id, s.kind, t.title, t.project_path, m.role, substr(m.text, 1, 240), m.ts
             FROM messages m
             JOIN threads t ON t.id = m.thread_id
             JOIN sources s ON s.id = t.source_id
             WHERE m.id = ?1",
            [message_id],
            |r| {
                Ok(SearchHit {
                    thread_id: r.get(0)?,
                    message_id: r.get(1)?,
                    source: r.get(2)?,
                    title: r.get(3)?,
                    project_path: r.get(4)?,
                    role: r.get(5)?,
                    snippet: r.get(6)?,
                    ts: r.get(7)?,
                })
            },
        )
        .ok();
    Ok(hit)
}

/// Most recently updated threads, optionally filtered by source/project.
pub fn recent_threads(conn: &Connection, filters: &SearchFilters) -> Result<Vec<ThreadSummary>> {
    let limit = filters.limit.unwrap_or(100).min(500) as i64;
    let mut sql = String::from(
        "SELECT t.id, s.kind, t.title, t.project_path, t.message_count, t.updated_at
         FROM threads t JOIN sources s ON s.id = t.source_id WHERE 1=1",
    );
    if !filters.include_subagents {
        sql.push_str(" AND t.is_subagent = 0");
    }
    let mut args: Vec<Box<dyn ToSql>> = Vec::new();
    if !filters.sources.is_empty() {
        let placeholders: Vec<String> = (0..filters.sources.len())
            .map(|i| format!("?{}", i + 1))
            .collect();
        sql.push_str(&format!(" AND s.kind IN ({})", placeholders.join(", ")));
        for src in &filters.sources {
            args.push(Box::new(src.clone()));
        }
    }
    if let Some(project) = &filters.project {
        args.push(Box::new(format!("%{project}%")));
        sql.push_str(&format!(" AND t.project_path LIKE ?{}", args.len()));
    }
    sql.push_str(" ORDER BY t.updated_at DESC LIMIT ?");
    args.push(Box::new(limit));

    let args_ref: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(args_ref.as_slice(), |r| {
        Ok(ThreadSummary {
            id: r.get(0)?,
            source: r.get(1)?,
            title: r.get(2)?,
            project_path: r.get(3)?,
            message_count: r.get(4)?,
            updated_at: r.get(5)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Aggregate stats over the whole index, for the dashboard / `cal stats`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    pub threads: i64,
    pub messages: i64,
    pub embedded: i64,   // distinct messages with a vector chunk
    pub embeddable: i64, // user/assistant messages eligible for embedding
    pub earliest: Option<i64>,
    pub latest: Option<i64>,
    pub per_source: Vec<SourceStat>,
    pub per_role: Vec<RoleStat>,
    pub top_projects: Vec<ProjectStat>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceStat {
    pub kind: String,
    pub threads: i64,
    pub messages: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoleStat {
    pub role: String,
    pub messages: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectStat {
    pub project: String,
    pub threads: i64,
}

/// Compute index-wide aggregate stats in one pass of small queries.
pub fn stats(conn: &Connection) -> Result<Stats> {
    let one = |sql: &str| -> Result<i64> { Ok(conn.query_row(sql, [], |r| r.get(0))?) };

    let threads = one("SELECT COUNT(*) FROM threads")?;
    let messages = one("SELECT COUNT(*) FROM messages")?;
    let embedded = one("SELECT COUNT(DISTINCT message_id) FROM vec_chunks")?;
    let embeddable = one("SELECT COUNT(*) FROM messages WHERE role IN ('user','assistant')")?;
    let (earliest, latest): (Option<i64>, Option<i64>) = conn.query_row(
        "SELECT MIN(created_at), MAX(updated_at) FROM threads",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;

    // Per-source thread + message counts (skip sources with no threads).
    let mut src_stmt = conn.prepare(
        "SELECT s.kind,
                (SELECT COUNT(*) FROM threads t WHERE t.source_id = s.id),
                (SELECT COUNT(*) FROM messages m JOIN threads t ON m.thread_id = t.id
                 WHERE t.source_id = s.id)
         FROM sources s",
    )?;
    let mut per_source: Vec<SourceStat> = src_stmt
        .query_map([], |r| {
            Ok(SourceStat { kind: r.get(0)?, threads: r.get(1)?, messages: r.get(2)? })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    per_source.retain(|s| s.threads > 0);
    per_source.sort_by(|a, b| b.messages.cmp(&a.messages));

    let mut role_stmt =
        conn.prepare("SELECT role, COUNT(*) FROM messages GROUP BY role ORDER BY 2 DESC")?;
    let per_role = role_stmt
        .query_map([], |r| Ok(RoleStat { role: r.get(0)?, messages: r.get(1)? }))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut proj_stmt = conn.prepare(
        "SELECT project_path, COUNT(*) AS n FROM threads
         WHERE project_path IS NOT NULL AND project_path <> ''
         GROUP BY project_path ORDER BY n DESC LIMIT 8",
    )?;
    let top_projects = proj_stmt
        .query_map([], |r| Ok(ProjectStat { project: r.get(0)?, threads: r.get(1)? }))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(Stats {
        threads,
        messages,
        embedded,
        embeddable,
        earliest,
        latest,
        per_source,
        per_role,
        top_projects,
    })
}

/// Full detail + ordered messages for one thread.
pub fn thread_detail(conn: &Connection, thread_id: i64) -> Result<Option<ThreadDetail>> {
    let head = conn.query_row(
        "SELECT t.id, s.kind, t.external_id, t.title, t.project_path, t.git_branch,
                t.created_at, t.updated_at
         FROM threads t JOIN sources s ON s.id = t.source_id WHERE t.id = ?1",
        [thread_id],
        |r| {
            Ok(ThreadDetail {
                id: r.get(0)?,
                source: r.get(1)?,
                external_id: r.get(2)?,
                title: r.get(3)?,
                project_path: r.get(4)?,
                git_branch: r.get(5)?,
                created_at: r.get(6)?,
                updated_at: r.get(7)?,
                messages: Vec::new(),
            })
        },
    );
    let mut detail = match head {
        Ok(d) => d,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(e) => return Err(e.into()),
    };

    let mut stmt = conn.prepare(
        "SELECT id, role, text, tool_name, ts FROM messages
         WHERE thread_id = ?1 ORDER BY seq",
    )?;
    let rows = stmt.query_map([thread_id], |r| {
        Ok(MessageRow {
            id: r.get(0)?,
            role: r.get(1)?,
            text: r.get(2)?,
            tool_name: r.get(3)?,
            ts: r.get(4)?,
        })
    })?;
    detail.messages = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Some(detail))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::{source_id, upsert_thread, ParsedMessage, ParsedThread};

    fn temp_db() -> Connection {
        let p = std::env::temp_dir().join(format!("calli_stats_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        crate::db::open(&p).unwrap()
    }

    fn msg(role: &str, text: &str, ts: i64) -> ParsedMessage {
        ParsedMessage { role: role.into(), text: text.into(), tool_name: None, ts: Some(ts) }
    }

    #[test]
    fn stats_aggregates_sources_roles_projects() {
        let mut conn = temp_db();
        let sid = source_id(&conn, "claude_code").unwrap();
        let t1 = ParsedThread {
            external_id: "t1".into(),
            title: Some("one".into()),
            project_path: Some("/proj/a".into()),
            created_at: Some(100),
            updated_at: Some(200),
            messages: vec![msg("user", "hi", 100), msg("assistant", "yo", 150)],
            ..Default::default()
        };
        let t2 = ParsedThread {
            external_id: "t2".into(),
            project_path: Some("/proj/a".into()),
            created_at: Some(300),
            updated_at: Some(400),
            messages: vec![msg("user", "again", 300)],
            ..Default::default()
        };
        upsert_thread(&mut conn, sid, &t1).unwrap();
        upsert_thread(&mut conn, sid, &t2).unwrap();

        let s = stats(&conn).unwrap();
        assert_eq!(s.threads, 2);
        assert_eq!(s.messages, 3);
        assert_eq!(s.embeddable, 3); // 2 user + 1 assistant
        assert_eq!(s.embedded, 0); // nothing embedded yet
        assert_eq!(s.earliest, Some(100));
        assert_eq!(s.latest, Some(400));

        // Per-source: only sources that actually have threads are reported.
        assert!(s.per_source.iter().all(|x| x.threads > 0));
        let cc = s.per_source.iter().find(|x| x.kind == "claude_code").unwrap();
        assert_eq!((cc.threads, cc.messages), (2, 3));

        let users = s.per_role.iter().find(|r| r.role == "user").unwrap();
        assert_eq!(users.messages, 2);

        let proj = s.top_projects.iter().find(|p| p.project == "/proj/a").unwrap();
        assert_eq!(proj.threads, 2);
    }
}
