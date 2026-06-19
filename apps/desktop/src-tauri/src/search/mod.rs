//! Full-text search over indexed messages (SQLite FTS5 + BM25) plus the read
//! queries the UI needs: recent threads and a single thread's messages.

use crate::embed::{self, Embedder};
use anyhow::Result;
use rusqlite::{Connection, ToSql};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

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
    pub starred: Option<bool>,   // Some(true) = only starred; None = all
    pub tags: Vec<String>,       // empty = all; else threads having ANY of these tags
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
    pub starred: bool,
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
    pub starred: bool,
    pub tags: Vec<String>,
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

/// Append the starred + tags ("collections") WHERE clauses, assuming the threads
/// table is aliased `t`. Shared by `search` and `recent_threads`.
fn push_collection_filters(
    sql: &mut String,
    args: &mut Vec<Box<dyn ToSql>>,
    filters: &SearchFilters,
) {
    if let Some(starred) = filters.starred {
        args.push(Box::new(i64::from(starred)));
        sql.push_str(&format!(" AND t.starred = ?{}", args.len()));
    }
    if !filters.tags.is_empty() {
        let base = args.len();
        let placeholders: Vec<String> =
            (0..filters.tags.len()).map(|i| format!("?{}", base + i + 1)).collect();
        sql.push_str(&format!(
            " AND EXISTS (SELECT 1 FROM thread_tags tt WHERE tt.thread_id = t.id AND tt.tag IN ({}))",
            placeholders.join(", ")
        ));
        for tag in &filters.tags {
            args.push(Box::new(tag.clone()));
        }
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
    push_collection_filters(&mut sql, &mut args, filters);
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
    let qv = embed::embed_query(embedder, query)?;
    hybrid_vec(conn, query, qv.as_deref(), filters)
}

/// Hybrid (keyword + semantic) search with an ALREADY-embedded query vector. Holds
/// only `conn` — no model inference — so callers on the UI path should run
/// `embed::embed_query` BEFORE taking the DB lock. `qv == None` skips the semantic
/// arm (keyword-only).
pub fn hybrid_vec(
    conn: &Connection,
    query: &str,
    qv: Option<&[f32]>,
    filters: &SearchFilters,
) -> Result<Vec<SearchHit>> {
    const RRF_K: f32 = 60.0;
    let limit = filters.limit.unwrap_or(100).min(500) as usize;

    let fts = search(conn, query, filters)?;
    let sem = match qv {
        Some(v) => embed::semantic_search_vec(
            conn,
            v,
            filters.include_subagents,
            &filters.sources,
            limit.max(50),
        )?,
        None => Vec::new(),
    };

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
        "SELECT t.id, s.kind, t.title, t.project_path, t.message_count, t.updated_at, t.starred
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
    push_collection_filters(&mut sql, &mut args, filters);
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
            starred: r.get::<_, i64>(6)? != 0,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Threads most semantically related to arbitrary context text (a code selection,
/// an error, a symbol) — the engine behind "ambient recall". Reuses the message-
/// level semantic KNN, then dedupes to one summary per thread preserving rank.
/// All-projects by design; a set `filters.project` post-filters. Returns empty
/// (not an error) when the index has no embeddings yet.
pub fn related(
    conn: &Connection,
    embedder: &Embedder,
    context: &str,
    filters: &SearchFilters,
) -> Result<Vec<ThreadSummary>> {
    let limit = filters.limit.unwrap_or(5).clamp(1, 50) as usize;
    // Over-fetch message hits so dedup-to-thread still leaves `limit` threads.
    let hits = embed::semantic_search(
        conn,
        embedder,
        context,
        filters.include_subagents,
        &filters.sources,
        limit * 8,
    )?;

    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(limit);
    for (message_id, _sim) in hits {
        let thread_id: i64 =
            conn.query_row("SELECT thread_id FROM messages WHERE id = ?1", [message_id], |r| {
                r.get(0)
            })?;
        if !seen.insert(thread_id) {
            continue; // a better-ranked message already claimed this thread
        }
        if let Some(summary) = thread_summary(conn, thread_id)? {
            if let Some(p) = &filters.project {
                if !summary.project_path.as_deref().unwrap_or("").contains(p.as_str()) {
                    continue;
                }
            }
            out.push(summary);
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(out)
}

/// Threads that mention a file path (substring, case-insensitive), newest first.
/// Backs code-aware search: "find every thread that touched embed/mod.rs".
pub fn threads_with_file(conn: &Connection, path: &str, limit: i64) -> Result<Vec<ThreadSummary>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT fm.thread_id
         FROM file_mentions fm
         JOIN threads t ON t.id = fm.thread_id
         WHERE fm.path LIKE ?1 AND t.is_subagent = 0
         ORDER BY t.updated_at DESC, t.id DESC
         LIMIT ?2",
    )?;
    let ids = stmt
        .query_map(rusqlite::params![format!("%{path}%"), limit], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(s) = thread_summary(conn, id)? {
            out.push(s);
        }
    }
    Ok(out)
}

/// One thread's summary row, or None if the id is unknown.
fn thread_summary(conn: &Connection, id: i64) -> Result<Option<ThreadSummary>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, s.kind, t.title, t.project_path, t.message_count, t.updated_at, t.starred
         FROM threads t JOIN sources s ON s.id = t.source_id WHERE t.id = ?1",
    )?;
    let mut rows = stmt.query_map([id], |r| {
        Ok(ThreadSummary {
            id: r.get(0)?,
            source: r.get(1)?,
            title: r.get(2)?,
            project_path: r.get(3)?,
            message_count: r.get(4)?,
            updated_at: r.get(5)?,
            starred: r.get::<_, i64>(6)? != 0,
        })
    })?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
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
                t.created_at, t.updated_at, t.starred
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
                starred: r.get::<_, i64>(8)? != 0,
                tags: Vec::new(),
                messages: Vec::new(),
            })
        },
    );
    let mut detail = match head {
        Ok(d) => d,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    detail.tags = thread_tags(conn, thread_id)?;

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

// ---- stars & tags ("collections") ----

/// All tags on a thread, alphabetical.
pub fn thread_tags(conn: &Connection, thread_id: i64) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT tag FROM thread_tags WHERE thread_id = ?1 ORDER BY tag")?;
    let rows = stmt.query_map([thread_id], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Star or unstar a thread.
pub fn set_star(conn: &Connection, thread_id: i64, starred: bool) -> Result<()> {
    conn.execute(
        "UPDATE threads SET starred = ?1 WHERE id = ?2",
        rusqlite::params![i64::from(starred), thread_id],
    )?;
    Ok(())
}

/// Replace a thread's tags with `tags` (trimmed, deduped, blanks dropped). `now`
/// is the epoch-seconds timestamp to record on each tag.
pub fn set_thread_tags(
    conn: &mut Connection,
    thread_id: i64,
    tags: &[String],
    now: i64,
) -> Result<()> {
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM thread_tags WHERE thread_id = ?1", [thread_id])?;
    {
        let mut ins = tx.prepare(
            "INSERT OR IGNORE INTO thread_tags (thread_id, tag, added_at) VALUES (?1, ?2, ?3)",
        )?;
        let mut seen = HashSet::new();
        for tag in tags {
            let t = tag.trim();
            if t.is_empty() || !seen.insert(t.to_string()) {
                continue;
            }
            ins.execute(rusqlite::params![thread_id, t, now])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Distinct tags across the index with their thread counts, most-used first.
pub fn list_tags(conn: &Connection) -> Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT tag, COUNT(DISTINCT thread_id) AS n FROM thread_tags
         GROUP BY tag ORDER BY n DESC, tag",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::{source_id, upsert_thread, ParsedMessage, ParsedThread};

    fn temp_db() -> Connection {
        // Unique per call: tests run in parallel, and two opening the same WAL file
        // race their migrations into a SQLITE_PROTOCOL lock error.
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "calli_test_{}_{}.db",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
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

    #[test]
    fn stars_and_tags_filter_and_survive_reindex() {
        let mut conn = temp_db();
        let sid = source_id(&conn, "claude_code").unwrap();
        let t1 = ParsedThread {
            external_id: "s1".into(),
            title: Some("auth bug".into()),
            created_at: Some(100),
            updated_at: Some(200),
            messages: vec![msg("user", "jwt refresh", 100)],
            ..Default::default()
        };
        let t2 = ParsedThread {
            external_id: "s2".into(),
            title: Some("ui tweak".into()),
            created_at: Some(300),
            updated_at: Some(400),
            messages: vec![msg("user", "css", 300)],
            ..Default::default()
        };
        upsert_thread(&mut conn, sid, &t1).unwrap();
        upsert_thread(&mut conn, sid, &t2).unwrap();
        let id1: i64 = conn
            .query_row("SELECT id FROM threads WHERE external_id = 's1'", [], |r| r.get(0))
            .unwrap();

        set_star(&conn, id1, true).unwrap();
        // Includes a duplicate (" auth ") and a blank — both should be dropped.
        set_thread_tags(&mut conn, id1, &["auth".into(), "wip".into(), " auth ".into(), "".into()], 500)
            .unwrap();

        // starred filter returns only the starred thread.
        let starred =
            recent_threads(&conn, &SearchFilters { starred: Some(true), ..Default::default() }).unwrap();
        assert_eq!(starred.len(), 1);
        assert_eq!(starred[0].id, id1);
        assert!(starred[0].starred);

        // tag filter returns only the tagged thread.
        let tagged =
            recent_threads(&conn, &SearchFilters { tags: vec!["auth".into()], ..Default::default() })
                .unwrap();
        assert_eq!(tagged.iter().map(|t| t.id).collect::<Vec<_>>(), vec![id1]);

        // dedup + trim: just the two distinct tags, alphabetical.
        assert_eq!(thread_tags(&conn, id1).unwrap(), vec!["auth".to_string(), "wip".to_string()]);
        assert!(list_tags(&conn).unwrap().iter().any(|(t, n)| t == "auth" && *n == 1));

        // Re-indexing the thread must NOT wipe the star or tags.
        upsert_thread(&mut conn, sid, &t1).unwrap();
        let d = thread_detail(&conn, id1).unwrap().unwrap();
        assert!(d.starred, "star lost on re-index");
        assert_eq!(d.tags, vec!["auth".to_string(), "wip".to_string()], "tags lost on re-index");
    }
}
