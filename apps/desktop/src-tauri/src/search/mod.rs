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
    /// The model that produced this turn (assistant rows), when the source records it.
    pub model: Option<String>,
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
        let placeholders: Vec<String> = (0..filters.tags.len())
            .map(|i| format!("?{}", base + i + 1))
            .collect();
        sql.push_str(&format!(
            " AND EXISTS (SELECT 1 FROM thread_tags tt WHERE tt.thread_id = t.id AND tt.tag IN ({}))",
            placeholders.join(", ")
        ));
        for tag in &filters.tags {
            args.push(Box::new(tag.clone()));
        }
    }
}

/// Per-thread hit cap for result lists: at most this many message-hits from any one thread,
/// so a single long thread can't fill the list and bury every other thread. Discovery is
/// cross-thread; per-thread depth is via opening the thread.
const MAX_HITS_PER_THREAD: usize = 3;
/// How far past `limit` to fetch before capping, leaving room to back-fill the slots freed
/// by dropping a dominant thread's overflow.
const THREAD_CAP_OVERFETCH: usize = 4;

/// Run a full-text search, capped to `MAX_HITS_PER_THREAD` per thread. Empty query returns
/// nothing (use recent_threads instead).
pub fn search(conn: &Connection, query: &str, filters: &SearchFilters) -> Result<Vec<SearchHit>> {
    let limit = filters.limit.unwrap_or(100).min(500) as usize;
    let fetch = (limit * THREAD_CAP_OVERFETCH).min(2000) as i64;
    let hits = search_ranked(conn, query, filters, fetch)?;
    Ok(cap_per_thread(hits, limit))
}

/// BM25-ranked FTS hits (strict-AND, then OR back-fill), fetching up to `fetch` rows with NO
/// per-thread cap — `search` caps after, and the hybrid fusion needs the full per-thread
/// signal before it merges with the semantic arm.
fn search_ranked(
    conn: &Connection,
    query: &str,
    filters: &SearchFilters,
    fetch: i64,
) -> Result<Vec<SearchHit>> {
    // Each whitespace token becomes a quoted PREFIX term (`"tok"*`), so "embed" matches
    // "embedder"/"embedding" and a natural-language query isn't gated on exact words.
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|t| format!("\"{}\"*", t.replace('"', "\"\"")))
        .collect();
    if terms.is_empty() {
        return Ok(Vec::new());
    }

    // Strict pass: require ALL terms (precise). If it under-fills and there were several
    // terms, back-fill with a looser OR pass appended AFTER the precise hits (deduped).
    let mut hits = run_fts(conn, &terms.join(" "), filters, fetch)?;
    if (hits.len() as i64) < fetch && terms.len() > 1 {
        let seen: HashSet<i64> = hits.iter().map(|h| h.message_id).collect();
        for h in run_fts(conn, &terms.join(" OR "), filters, fetch)? {
            if (hits.len() as i64) >= fetch {
                break;
            }
            if !seen.contains(&h.message_id) {
                hits.push(h);
            }
        }
    }
    Ok(hits)
}

/// One FTS5 pass for an already-built MATCH string, with all the non-text filters applied.
fn run_fts(
    conn: &Connection,
    match_query: &str,
    filters: &SearchFilters,
    limit: i64,
) -> Result<Vec<SearchHit>> {
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
    let mut args: Vec<Box<dyn ToSql>> = vec![Box::new(match_query.to_string())];

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
        // Scope the same way facts/threads aggregate (COALESCE), matching the semantic arm.
        sql.push_str(&format!(
            " AND COALESCE(t.project_key, t.project_path) LIKE ?{}",
            args.len()
        ));
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
    let limit = filters.limit.unwrap_or(100).min(500) as usize;

    // Uncapped FTS arm (search_ranked, not search): fusion needs every per-thread hit before
    // it merges with the semantic arm; the per-thread cap is applied once, on the fused output.
    let fts = search_ranked(conn, query, filters, limit as i64)?;
    let sem = match qv {
        Some(v) => embed::semantic_search_vec(
            conn,
            v,
            filters.include_subagents,
            &filters.sources,
            filters.project.as_deref(),
            limit.max(50),
        )?,
        None => Vec::new(),
    };

    let fts_ids: Vec<i64> = fts.iter().map(|h| h.message_id).collect();
    let ranked = fuse_rrf(&fts_ids, &sem);

    // Materialize hits in fused order (FTS hits already in hand; semantic-only hits need one
    // PK fetch each — both lists are limit-bounded, so this stays small), then cap per thread.
    let fts_by_id: HashMap<i64, &SearchHit> = fts.iter().map(|h| (h.message_id, h)).collect();
    let mut ordered = Vec::with_capacity(ranked.len());
    for (id, _) in ranked {
        if let Some(h) = fts_by_id.get(&id) {
            ordered.push((*h).clone());
        } else if let Some(h) = hit_for_message(conn, id)? {
            ordered.push(h);
        }
    }
    Ok(cap_per_thread(ordered, limit))
}

/// Reciprocal-rank fusion of the keyword (FTS/BM25) and semantic (cosine) result lists,
/// returning `(message_id, score)` sorted best-first. The keyword arm contributes the
/// classic rank-only term `1/(K+rank)`. The semantic arm scales that term by
/// `sem_weight(similarity)`, so a marginal near-floor match contributes less than a strong
/// one at the same rank — without ever exceeding the keyword arm's weight (factor caps at
/// 1.0), so the previously-tuned keyword/semantic balance can't blow out.
fn fuse_rrf(fts_ids: &[i64], sem: &[(i64, f32)]) -> Vec<(i64, f32)> {
    const RRF_K: f32 = 60.0;
    let mut scores: HashMap<i64, f32> = HashMap::new();
    for (rank, id) in fts_ids.iter().enumerate() {
        *scores.entry(*id).or_default() += 1.0 / (RRF_K + rank as f32 + 1.0);
    }
    for (rank, (id, sim)) in sem.iter().enumerate() {
        *scores.entry(*id).or_default() += sem_weight(*sim) / (RRF_K + rank as f32 + 1.0);
    }
    let mut ranked: Vec<(i64, f32)> = scores.into_iter().collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    ranked
}

/// Limit how many hits any single thread contributes (preserving fused/ranked order), then
/// trim to `limit`. One long thread can otherwise occupy every top slot and bury the rest;
/// the dropped overflow lets other threads back-fill (callers over-fetch to leave room).
fn cap_per_thread(ordered: Vec<SearchHit>, limit: usize) -> Vec<SearchHit> {
    let mut per_thread: HashMap<i64, usize> = HashMap::new();
    let mut out = Vec::with_capacity(limit.min(ordered.len()));
    for h in ordered {
        if out.len() >= limit {
            break;
        }
        let count = per_thread.entry(h.thread_id).or_insert(0);
        if *count >= MAX_HITS_PER_THREAD {
            continue;
        }
        *count += 1;
        out.push(h);
    }
    out
}

/// Map a cosine similarity in `[SEM_SIMILARITY_FLOOR, 1.0]` to an RRF weight in `[0.5, 1.0]`:
/// a top match keeps full rank-weight, the weakest retained match keeps half. Monotonic and
/// clamped, so it only ever demotes weak semantic matches — never inflates above the keyword
/// arm. (The semantic list is already returned floored at `SEM_SIMILARITY_FLOOR`.)
fn sem_weight(sim: f32) -> f32 {
    const MIN_W: f32 = 0.5;
    let floor = embed::SEM_SIMILARITY_FLOOR;
    let norm = ((sim - floor) / (1.0 - floor)).clamp(0.0, 1.0);
    MIN_W + (1.0 - MIN_W) * norm
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
        // Scope the same way facts/threads aggregate (COALESCE), matching the semantic arm.
        sql.push_str(&format!(
            " AND COALESCE(t.project_key, t.project_path) LIKE ?{}",
            args.len()
        ));
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
        filters.project.as_deref(),
        limit * 8,
    )?;

    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(limit);
    for (message_id, _sim) in hits {
        let thread_id: i64 = conn.query_row(
            "SELECT thread_id FROM messages WHERE id = ?1",
            [message_id],
            |r| r.get(0),
        )?;
        if !seen.insert(thread_id) {
            continue; // a better-ranked message already claimed this thread
        }
        if let Some(summary) = thread_summary(conn, thread_id)? {
            if let Some(p) = &filters.project {
                if !summary
                    .project_path
                    .as_deref()
                    .unwrap_or("")
                    .contains(p.as_str())
                {
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
    let path = path.trim();
    // Matching thread ids: trigram FTS (indexed substring) for >= 3 chars, else a LIKE
    // fallback (trigram can't index shorter terms). Then build every summary in ONE join
    // (no per-id round-trip).
    let (id_clause, bind): (&str, Box<dyn rusqlite::ToSql>) = if path.chars().count() >= 3 {
        (
            "SELECT fm.thread_id FROM file_mentions fm
             JOIN fm_fts ON fm_fts.rowid = fm.rowid
             WHERE fm_fts MATCH ?1",
            // Quote as an FTS5 phrase so '/' '.' etc. are literal, not operators.
            Box::new(format!("\"{}\"", path.replace('"', ""))),
        )
    } else {
        (
            "SELECT fm.thread_id FROM file_mentions fm WHERE fm.path LIKE ?1",
            Box::new(format!("%{path}%")),
        )
    };
    let sql = format!(
        "SELECT t.id, s.kind, t.title, t.project_path, t.message_count, t.updated_at, t.starred
         FROM threads t
         JOIN sources s ON s.id = t.source_id
         WHERE t.is_subagent = 0 AND t.id IN ({id_clause})
         ORDER BY t.updated_at DESC, t.id DESC
         LIMIT ?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params![bind, limit], |r| {
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
            Ok(SourceStat {
                kind: r.get(0)?,
                threads: r.get(1)?,
                messages: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    per_source.retain(|s| s.threads > 0);
    per_source.sort_by_key(|s| std::cmp::Reverse(s.messages));

    let mut role_stmt =
        conn.prepare("SELECT role, COUNT(*) FROM messages GROUP BY role ORDER BY 2 DESC")?;
    let per_role = role_stmt
        .query_map([], |r| {
            Ok(RoleStat {
                role: r.get(0)?,
                messages: r.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut proj_stmt = conn.prepare(
        "SELECT project_path, COUNT(*) AS n FROM threads
         WHERE project_path IS NOT NULL AND project_path <> ''
         GROUP BY project_path ORDER BY n DESC LIMIT 8",
    )?;
    let top_projects = proj_stmt
        .query_map([], |r| {
            Ok(ProjectStat {
                project: r.get(0)?,
                threads: r.get(1)?,
            })
        })?
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

/// One day's message activity, for the Coach coding heatmap.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DayActivity {
    pub day: i64, // unix seconds at UTC midnight
    pub messages: i64,
}

/// A distilled decision or gotcha surfaced in the Coach "this week" digest.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CoachFact {
    pub id: i64,
    pub thread_id: i64,
    pub text: String,
    pub title: Option<String>,
    pub project: Option<String>,
    pub created_at: i64,
}

/// Proactive dashboard data: a daily-activity heatmap plus the decisions and gotchas
/// captured in the last week (so the memory layer surfaces insight, not just answers).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CoachOverview {
    pub heatmap: Vec<DayActivity>,
    pub decisions: Vec<CoachFact>,
    pub gotchas: Vec<CoachFact>,
    pub since: i64, // window start (unix s) for the decisions/gotchas digest
}

/// Build the Coach overview as of `now` (unix seconds): ~52 weeks of daily activity and
/// the last 7 days of decisions / gotchas (LLM-distilled or agent-recorded).
pub fn coach_overview(conn: &Connection, now: i64) -> Result<CoachOverview> {
    let heatmap_since = now - 364 * 86_400;
    let heatmap = {
        // Human-facing activity only: skip subagent transcripts and tool/system rows so the
        // grid reflects sessions you drove, not machine chatter (mirrors the app's lists).
        let mut stmt = conn.prepare(
            "SELECT (m.ts / 86400) * 86400 AS day, COUNT(*)
             FROM messages m JOIN threads t ON t.id = m.thread_id
             WHERE m.ts IS NOT NULL AND m.ts >= ?1
               AND t.is_subagent = 0 AND m.role IN ('user', 'assistant')
             GROUP BY day ORDER BY day",
        )?;
        let rows = stmt
            .query_map([heatmap_since], |r| {
                Ok(DayActivity {
                    day: r.get(0)?,
                    messages: r.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };

    let since = now - 7 * 86_400;
    let recent = |kind: &str| -> Result<Vec<CoachFact>> {
        let mut stmt = conn.prepare(
            "SELECT f.id, f.thread_id, f.text, t.title,
                    COALESCE(t.project_key, t.project_path), f.created_at
             FROM facts f JOIN threads t ON t.id = f.thread_id
             WHERE f.kind = ?1 AND f.hidden = 0 AND f.created_at >= ?2
             ORDER BY f.created_at DESC LIMIT 40",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![kind, since], |r| {
                Ok(CoachFact {
                    id: r.get(0)?,
                    thread_id: r.get(1)?,
                    text: r.get(2)?,
                    title: r.get(3)?,
                    project: r.get(4)?,
                    created_at: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    };

    Ok(CoachOverview {
        heatmap,
        decisions: recent("decision")?,
        gotchas: recent("gotcha")?,
        since,
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
        "SELECT id, role, text, tool_name, ts, model FROM messages
         WHERE thread_id = ?1 ORDER BY seq",
    )?;
    let rows = stmt.query_map([thread_id], |r| {
        Ok(MessageRow {
            id: r.get(0)?,
            role: r.get(1)?,
            text: r.get(2)?,
            tool_name: r.get(3)?,
            ts: r.get(4)?,
            model: r.get(5)?,
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
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
        ParsedMessage {
            role: role.into(),
            text: text.into(),
            tool_name: None,
            ts: Some(ts),
        }
    }

    #[test]
    fn sem_weight_full_at_top_half_at_floor_and_monotonic() {
        let floor = crate::embed::SEM_SIMILARITY_FLOOR;
        assert!(
            (sem_weight(1.0) - 1.0).abs() < 1e-6,
            "top similarity keeps full weight"
        );
        assert!(
            (sem_weight(floor) - 0.5).abs() < 1e-6,
            "floor similarity keeps half weight"
        );
        // Below the floor clamps (the arm is pre-floored, but be safe).
        assert!((sem_weight(floor - 0.1) - 0.5).abs() < 1e-6);
        // Strictly increasing across the retained range.
        assert!(sem_weight(0.5) < sem_weight(0.7));
        assert!(sem_weight(0.7) < sem_weight(0.95));
    }

    #[test]
    fn fuse_rrf_lets_strong_similarity_outrank_weak_at_same_rank() {
        // Two semantic-only hits: id 1 a weak match at rank 0, id 2 a strong match at rank 1.
        // Pure rank-only RRF ranks id 1 first (1/61 > 1/62); similarity weighting flips it.
        let ranked = fuse_rrf(&[], &[(1, 0.40), (2, 0.95)]);
        assert_eq!(
            ranked[0].0, 2,
            "strong semantic match wins despite a worse rank"
        );
        assert_eq!(ranked[1].0, 1);
    }

    #[test]
    fn fuse_rrf_keyword_arm_unchanged() {
        // No semantic input: scores are exactly the classic 1/(K+rank+1), order = input order.
        let ranked = fuse_rrf(&[10, 20, 30], &[]);
        assert_eq!(
            ranked.iter().map(|x| x.0).collect::<Vec<_>>(),
            vec![10, 20, 30]
        );
        assert!((ranked[0].1 - 1.0 / 61.0).abs() < 1e-6);
    }

    fn mk_hit(thread_id: i64, message_id: i64) -> SearchHit {
        SearchHit {
            thread_id,
            message_id,
            source: "claude_code".into(),
            title: None,
            project_path: None,
            role: "user".into(),
            snippet: String::new(),
            ts: None,
        }
    }

    #[test]
    fn cap_per_thread_caps_dominant_thread_and_preserves_order() {
        // thread 1 appears 5x, interleaved with threads 2 and 3.
        let ordered = vec![
            mk_hit(1, 100),
            mk_hit(1, 101),
            mk_hit(1, 102),
            mk_hit(2, 200),
            mk_hit(1, 103),
            mk_hit(1, 104),
            mk_hit(3, 300),
        ];
        let threads: Vec<i64> = cap_per_thread(ordered, 100)
            .iter()
            .map(|h| h.thread_id)
            .collect();
        // thread 1 capped at 3, original order kept, threads 2 and 3 retained.
        assert_eq!(threads, vec![1, 1, 1, 2, 3]);
    }

    #[test]
    fn cap_per_thread_respects_limit() {
        let ordered: Vec<SearchHit> = (0..10).map(|i| mk_hit(i, i + 1000)).collect();
        assert_eq!(cap_per_thread(ordered, 4).len(), 4);
    }

    #[test]
    fn search_caps_hits_from_a_dominant_thread() {
        let mut conn = temp_db();
        let sid = source_id(&conn, "claude_code").unwrap();
        // One thread with 6 messages all matching "alpha"; a second thread matches once.
        let big = ParsedThread {
            external_id: "big".into(),
            title: Some("alpha thread".into()),
            messages: (0..6)
                .map(|i| msg("user", "alpha alpha keyword", 100 + i as i64))
                .collect(),
            ..Default::default()
        };
        let small = ParsedThread {
            external_id: "small".into(),
            title: Some("other thread".into()),
            messages: vec![msg("user", "alpha here too", 50)],
            ..Default::default()
        };
        upsert_thread(&mut conn, sid, &big).unwrap();
        upsert_thread(&mut conn, sid, &small).unwrap();

        let hits = search(
            &conn,
            "alpha",
            &SearchFilters {
                limit: Some(20),
                ..Default::default()
            },
        )
        .unwrap();

        // Without the cap the 6-message thread would take 6 of the top slots; capped to 3.
        let big_id = hits
            .iter()
            .find(|h| h.title.as_deref() == Some("alpha thread"))
            .map(|h| h.thread_id)
            .unwrap();
        let from_big = hits.iter().filter(|h| h.thread_id == big_id).count();
        assert_eq!(from_big, 3, "dominant thread should be capped to 3");
        // The other thread still surfaces (wasn't crowded out).
        assert!(hits
            .iter()
            .any(|h| h.title.as_deref() == Some("other thread")));
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
        let cc = s
            .per_source
            .iter()
            .find(|x| x.kind == "claude_code")
            .unwrap();
        assert_eq!((cc.threads, cc.messages), (2, 3));

        let users = s.per_role.iter().find(|r| r.role == "user").unwrap();
        assert_eq!(users.messages, 2);

        let proj = s
            .top_projects
            .iter()
            .find(|p| p.project == "/proj/a")
            .unwrap();
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
            .query_row("SELECT id FROM threads WHERE external_id = 's1'", [], |r| {
                r.get(0)
            })
            .unwrap();

        set_star(&conn, id1, true).unwrap();
        // Includes a duplicate (" auth ") and a blank — both should be dropped.
        set_thread_tags(
            &mut conn,
            id1,
            &["auth".into(), "wip".into(), " auth ".into(), "".into()],
            500,
        )
        .unwrap();

        // starred filter returns only the starred thread.
        let starred = recent_threads(
            &conn,
            &SearchFilters {
                starred: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(starred.len(), 1);
        assert_eq!(starred[0].id, id1);
        assert!(starred[0].starred);

        // tag filter returns only the tagged thread.
        let tagged = recent_threads(
            &conn,
            &SearchFilters {
                tags: vec!["auth".into()],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(tagged.iter().map(|t| t.id).collect::<Vec<_>>(), vec![id1]);

        // dedup + trim: just the two distinct tags, alphabetical.
        assert_eq!(
            thread_tags(&conn, id1).unwrap(),
            vec!["auth".to_string(), "wip".to_string()]
        );
        assert!(list_tags(&conn)
            .unwrap()
            .iter()
            .any(|(t, n)| t == "auth" && *n == 1));

        // Re-indexing the thread must NOT wipe the star or tags.
        upsert_thread(&mut conn, sid, &t1).unwrap();
        let d = thread_detail(&conn, id1).unwrap().unwrap();
        assert!(d.starred, "star lost on re-index");
        assert_eq!(
            d.tags,
            vec!["auth".to_string(), "wip".to_string()],
            "tags lost on re-index"
        );
    }

    #[test]
    fn threads_with_file_matches_via_trigram_and_fallback() {
        let mut conn = temp_db();
        let sid = source_id(&conn, "claude_code").unwrap();
        let t = ParsedThread {
            external_id: "tf".into(),
            title: Some("file thread".into()),
            project_path: Some("/proj/x".into()),
            created_at: Some(100),
            updated_at: Some(200),
            messages: vec![
                msg(
                    "user",
                    "please edit src/embed/mod.rs and apps/desktop/package.json",
                    100,
                ),
                msg("assistant", "done", 150),
            ],
            ..Default::default()
        };
        upsert_thread(&mut conn, sid, &t).unwrap();

        // >= 3 chars → fm_fts trigram MATCH; substring of the stored "src/embed/mod.rs".
        let hits = threads_with_file(&conn, "embed/mod.rs", 20).unwrap();
        assert_eq!(hits.len(), 1, "found the thread that touched embed/mod.rs");
        assert_eq!(hits[0].title.as_deref(), Some("file thread"));

        // Not mentioned → none. Short query (< 3 chars) hits the LIKE fallback cleanly.
        assert!(threads_with_file(&conn, "nope/absent.go", 20)
            .unwrap()
            .is_empty());
        let _ = threads_with_file(&conn, "go", 20).unwrap();
    }
}
