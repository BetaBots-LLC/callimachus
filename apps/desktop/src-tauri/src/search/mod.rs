//! Full-text search over indexed messages (SQLite FTS5 + BM25) plus the read
//! queries the UI needs: recent threads and a single thread's messages.

pub(crate) mod cache;

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
    // Back-fill only when the visible page would come up short. The strict pass usually
    // returns fewer than `fetch` rows (4x the page, for the per-thread cap), and the OR
    // hits are appended after the strict ones, so once the capped page is full they could
    // never be shown: running the OR pass anyway cost seconds on a large index.
    let hits = ranked_hits(conn, query, filters, fetch, |strict| {
        capped_len(strict, limit) < limit
    })?;
    Ok(cap_per_thread(hits, limit))
}

/// BM25-ranked FTS hits (strict-AND, then OR back-fill), fetching up to `fetch` rows with NO
/// per-thread cap, for the hybrid fusion, which needs the full per-thread signal before it
/// merges with the semantic arm. Back-fills whenever the strict pass under-fills `fetch`.
fn search_ranked(
    conn: &Connection,
    query: &str,
    filters: &SearchFilters,
    fetch: i64,
) -> Result<Vec<SearchHit>> {
    ranked_hits(conn, query, filters, fetch, |_| true)
}

/// The strict pass for `query`, then, if it returned fewer than `fetch` rows and
/// `wants_more(strict hits)` agrees, the looser OR pass appended AFTER the precise hits
/// (deduped, total still capped at `fetch`).
fn ranked_hits(
    conn: &Connection,
    query: &str,
    filters: &SearchFilters,
    fetch: i64,
    wants_more: impl Fn(&[SearchHit]) -> bool,
) -> Result<Vec<SearchHit>> {
    let Some(q) = FtsQuery::parse(query) else {
        return Ok(Vec::new());
    };
    let mut hits = run_fts(conn, &q.strict, filters, fetch)?;
    let Some(loose) = q.loose else {
        return Ok(hits);
    };
    if (hits.len() as i64) < fetch && wants_more(&hits) {
        let seen: HashSet<i64> = hits.iter().map(|h| h.message_id).collect();
        for h in run_fts(conn, &loose, filters, fetch)? {
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

/// Words too common to narrow a keyword search. They stay exact terms in the strict pass
/// (the prefix `"i"*` matched 970k of 1.4M messages, and an all-prefix natural-language
/// query took 3-6 s against 0.1 s with these exact) and are left out of the OR back-fill,
/// where `"how"* OR "the"* OR ...` matched nearly every message and took 11-27 s.
const STOPWORDS: &[&str] = &[
    "the", "a", "an", "is", "are", "do", "did", "how", "why", "what", "we", "i", "to", "of", "in",
    "on", "for", "and", "or", "it", "this", "that",
];

/// ASCII tokens shorter than this match as exact terms, not prefixes: a 1-2 letter prefix
/// expands to a large share of the vocabulary. ("ui" no longer matches "uikit".)
const MIN_PREFIX_CHARS: usize = 3;

/// The FTS5 MATCH strings for a free-text query.
#[derive(Debug, PartialEq)]
struct FtsQuery {
    /// Every token, implicitly ANDed: the precise pass.
    strict: String,
    /// The OR of the tokens that carry meaning, to back-fill a short strict result. None
    /// when that would add nothing: a single token, or nothing but stopwords and letters.
    loose: Option<String>,
}

impl FtsQuery {
    fn parse(query: &str) -> Option<Self> {
        // Classify (and quote) each token by its core, without leading/trailing punctuation,
        // which the FTS tokenizer drops anyway: "this?" is the stopword "this" and "ui," the
        // short token "ui", not the prefixes this* and ui*. A token that is all punctuation
        // ("?", "--") tokenizes to nothing, so it is not a term at all.
        let tokens: Vec<&str> = query
            .split_whitespace()
            .map(|t| t.trim_matches(|c: char| !c.is_alphanumeric()))
            .filter(|t| !t.is_empty())
            .collect();
        if tokens.is_empty() {
            return None;
        }
        let strict = tokens
            .iter()
            .map(|t| fts_term(t))
            .collect::<Vec<_>>()
            .join(" ");
        // A lone letter is as unselective as a stopword once it stands on its own in an OR.
        let meaningful: Vec<String> = tokens
            .iter()
            .filter(|t| !is_stopword(t) && !is_lone_ascii(t))
            .map(|t| fts_term(t))
            .collect();
        let loose = (tokens.len() > 1 && !meaningful.is_empty()).then(|| meaningful.join(" OR "));
        Some(Self { strict, loose })
    }
}

fn is_stopword(token: &str) -> bool {
    STOPWORDS.iter().any(|w| token.eq_ignore_ascii_case(w))
}

/// One query token as a quoted FTS5 term, so punctuation is literal rather than an
/// operator. A prefix term (`"tok"*`, so "embed" matches "embedder"/"embedding" and a
/// natural-language query isn't gated on exact word forms), except that stopwords and
/// ASCII tokens under [`MIN_PREFIX_CHARS`] stay exact.
fn fts_term(token: &str) -> String {
    let quoted = token.replace('"', "\"\"");
    if is_stopword(token) || is_short_ascii(token) {
        format!("\"{quoted}\"")
    } else {
        format!("\"{quoted}\"*")
    }
}

/// The short-token rules (exact match, no lone letters in the OR) are ASCII-only. unicode61
/// keeps an unspaced CJK run, and a Korean word with its attached particles, as one token,
/// so a 1-2 character word there ("数据", "오류") only matches as a prefix of a longer token.
/// Other non-ASCII text keeps the prefix form it always had.
fn is_short_ascii(token: &str) -> bool {
    token.is_ascii() && token.len() < MIN_PREFIX_CHARS
}

/// A single ASCII letter or digit (see [`is_short_ascii`] for why only ASCII).
fn is_lone_ascii(token: &str) -> bool {
    token.is_ascii() && token.len() == 1
}

/// Above this many bytes a hit's snippet is its leading text, unhighlighted. snippet() is
/// quadratic in the matched term's occurrences: one 5.8 MB pasted log with 40k hits for
/// "index" added ~8 s to every search that ranked it. octet_length() reads only the record
/// header, and CASE evaluates only the branch it takes.
const SNIPPET_MAX_BYTES: usize = 256 * 1024;

/// The `messages m` join for a query driven by `messages_fts` hits. The filters need
/// m.thread_id / m.role / m.ts for every hit before ranking, and the planner always picks
/// the rowid lookup, one random read of the messages table per hit (149k reads for
/// "error"). The covering (id, thread_id, role, ts) index from 0031 answers those from the
/// index, and the table row is read only for rows that pass every filter. An index built
/// before 0031 (a newer read-only sidecar on an older app's index) gets the plain join,
/// since INDEXED BY a missing index is an error.
pub(crate) fn fts_message_join(conn: &Connection) -> &'static str {
    if crate::db::has_index(conn, "idx_messages_cover") {
        "JOIN messages m INDEXED BY idx_messages_cover ON m.id = messages_fts.rowid"
    } else {
        "JOIN messages m ON m.id = messages_fts.rowid"
    }
}

/// One FTS5 pass for an already-built MATCH string, with all the non-text filters applied.
fn run_fts(
    conn: &Connection,
    match_query: &str,
    filters: &SearchFilters,
    limit: i64,
) -> Result<Vec<SearchHit>> {
    let (sql, args) = fts_sql(conn, match_query, filters, limit);
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

/// The SQL and arguments for [`run_fts`].
fn fts_sql(
    conn: &Connection,
    match_query: &str,
    filters: &SearchFilters,
    limit: i64,
) -> (String, Vec<Box<dyn ToSql>>) {
    // Use control-char sentinels (\u{1}/\u{2}) for match boundaries, not HTML tags:
    // snippet() does not escape the body, so the frontend HTML-escapes the text and
    // only then swaps the sentinels for <mark> — preventing injection from indexed
    // content (a thread might literally contain "<script>").
    let mut sql = format!(
        "SELECT t.id, m.id, s.kind, t.title, t.project_path, m.role,
                CASE WHEN octet_length(m.text) > {SNIPPET_MAX_BYTES} THEN substr(m.text, 1, 240)
                     ELSE snippet(messages_fts, 0, char(1), char(2), '…', 12) END,
                m.ts
         FROM messages_fts
         {join}
         JOIN threads t ON t.id = m.thread_id
         JOIN sources s ON s.id = t.source_id
         WHERE messages_fts MATCH ?1",
        join = fts_message_join(conn),
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
    (sql, args)
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

/// How many of `ordered` [`cap_per_thread`] would keep, without consuming them.
fn capped_len(ordered: &[SearchHit], limit: usize) -> usize {
    let mut per_thread: HashMap<i64, usize> = HashMap::new();
    ordered
        .iter()
        .filter(|h| {
            let count = per_thread.entry(h.thread_id).or_insert(0);
            *count += 1;
            *count <= MAX_HITS_PER_THREAD
        })
        .take(limit)
        .count()
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
#[derive(Debug, Clone, Serialize)]
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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceStat {
    pub kind: String,
    pub threads: i64,
    pub messages: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoleStat {
    pub role: String,
    pub messages: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectStat {
    pub project: String,
    pub threads: i64,
}

/// Compute index-wide aggregate stats in one pass of small queries. Every query here is
/// served by an index or the (small) threads table; none reads message bodies.
pub fn stats(conn: &Connection) -> Result<Stats> {
    let one = |sql: &str| -> Result<i64> { Ok(conn.query_row(sql, [], |r| r.get(0))?) };

    let threads = one("SELECT COUNT(*) FROM threads")?;
    // Same numbers (and predicate) as the embedding progress bar, from the covering count
    // index, instead of a COUNT(DISTINCT) full scan of the vec0 table.
    let (embedded, embeddable) = embed::embed_progress(conn)?;
    let (earliest, latest): (Option<i64>, Option<i64>) = conn.query_row(
        "SELECT MIN(created_at), MAX(updated_at) FROM threads",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;

    // Per-source thread + message counts (skip sources with no threads). message_count is
    // maintained per thread by upsert_thread, so this never touches the messages table.
    let mut src_stmt = conn.prepare(
        "SELECT s.kind, COUNT(t.id), COALESCE(SUM(t.message_count), 0)
         FROM sources s LEFT JOIN threads t ON t.source_id = s.id
         GROUP BY s.id",
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
    // The per-role counts already cover every message (one scan of the covering count
    // index), so the total is their sum, not a second full scan for COUNT(*).
    let messages = per_role.iter().map(|r| r.messages).sum();

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
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DayActivity {
    pub day: i64, // unix seconds at UTC midnight
    pub messages: i64,
}

/// A distilled decision or gotcha surfaced in the Coach "this week" digest.
#[derive(Debug, Clone, Serialize)]
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
#[derive(Debug, Clone, Serialize)]
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
    let mut overview = coach_overview_facts(conn, now)?;
    overview.heatmap = coach_heatmap(conn, now)?;
    Ok(overview)
}

/// Daily user/assistant message counts since `?1`, for the Coach heatmap.
///
/// Human-facing activity only: skip subagent transcripts and tool/system rows so the grid
/// reflects sessions you drove, not machine chatter (mirrors the app's lists).
///
/// The `+` on `m.role` keeps the planner off the (role, tool_name, embedded) count index
/// (0026). With no ANALYZE stats it would otherwise drive from that index, fetching every
/// user/assistant row in the table (each one walking its overflow pages to reach `ts`),
/// which took 20-50s on a large index instead of well under a second via threads ->
/// idx_messages_thread_ts (0031), which also carries ts and role, so the whole query is
/// index-only (7-8 s down to ~0.2 s from a cold cache). Pinned by
/// `coach_heatmap_plan_avoids_the_role_index`.
const HEATMAP_SQL: &str = "SELECT (m.ts / 86400) * 86400 AS day, COUNT(*)
     FROM messages m JOIN threads t ON t.id = m.thread_id
     WHERE m.ts IS NOT NULL AND m.ts >= ?1
       AND t.is_subagent = 0 AND +m.role IN ('user', 'assistant')
     GROUP BY day ORDER BY day";

/// ~52 weeks of daily user/assistant message counts ending at `now`.
fn coach_heatmap(conn: &Connection, now: i64) -> Result<Vec<DayActivity>> {
    let heatmap_since = now - 364 * 86_400;
    let heatmap = {
        let mut stmt = conn.prepare(HEATMAP_SQL)?;
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
    Ok(heatmap)
}

/// The Coach overview without its heatmap: the last 7 days of decisions and gotchas.
fn coach_overview_facts(conn: &Connection, now: i64) -> Result<CoachOverview> {
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
        heatmap: Vec::new(),
        decisions: recent("decision")?,
        gotchas: recent("gotcha")?,
        since,
    })
}

static STATS_CACHE: std::sync::LazyLock<cache::AggCache<(), Stats>> =
    std::sync::LazyLock::new(cache::AggCache::new);
static HEATMAP_CACHE: std::sync::LazyLock<cache::AggCache<i64, Vec<DayActivity>>> =
    std::sync::LazyLock::new(cache::AggCache::new);

/// [`stats`], reused for up to [`cache::ANALYTICS_TTL`] while the thread set is unchanged.
/// For the app's Stats view; `cal stats` and tests want the live numbers.
///
/// The embedding counts are always read live: they move while a semantic-index build runs
/// without the thread set changing, so a cached copy would show stale coverage for the
/// whole TTL. Both are covering-index COUNTs, so this stays cheap.
pub fn stats_cached(conn: &Connection) -> Result<Stats> {
    let mut cached = STATS_CACHE.get_or_try(cache::index_version(conn)?, (), || stats(conn))?;
    (cached.embedded, cached.embeddable) = embed::embed_progress(conn)?;
    Ok(cached)
}

/// [`coach_overview`] with the year-long activity heatmap (a GROUP BY over most of the
/// messages table) cached per day for up to [`cache::ANALYTICS_TTL`]. The week's decisions
/// and gotchas are cheap and always read live, so a fact the user just hid or edited
/// disappears from the digest immediately.
pub fn coach_overview_cached(conn: &Connection, now: i64) -> Result<CoachOverview> {
    let day = now.div_euclid(86_400);
    let heatmap = HEATMAP_CACHE.get_or_try(cache::index_version(conn)?, day, || {
        coach_heatmap(conn, now)
    })?;
    let mut overview = coach_overview_facts(conn, now)?;
    overview.heatmap = heatmap;
    Ok(overview)
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

    /// The planner has no ANALYZE stats, so it picks indexes from the schema alone. Pin the
    /// heatmap to the threads -> idx_messages_thread_ts plan: driving from the role-prefixed
    /// count index fetched every user/assistant row in the table.
    #[test]
    fn coach_heatmap_plan_avoids_the_role_index() {
        let conn = temp_db();
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {HEATMAP_SQL}"))
            .unwrap()
            .query_map([0_i64], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let plan = plan.join(" | ");
        assert!(!plan.contains("idx_messages_embed_counts"), "{plan}");
        // 0031's (thread_id, ts, role) index answers the ts range and role filter without
        // reading a single message row.
        assert!(
            plan.contains("COVERING INDEX idx_messages_thread_ts (thread_id=? AND ts>?)"),
            "{plan}"
        );
    }

    #[test]
    fn query_terms_keep_stopwords_and_short_tokens_exact() {
        let q = FtsQuery::parse("how do I fix the stripe webhook").unwrap();
        assert_eq!(
            q.strict,
            r#""how" "do" "I" "fix"* "the" "stripe"* "webhook"*"#
        );
        assert_eq!(
            q.loose.as_deref(),
            Some(r#""fix"* OR "stripe"* OR "webhook"*"#)
        );
        // Short tokens are exact everywhere; a lone letter is no OR alternative.
        let q = FtsQuery::parse("ui x bug").unwrap();
        assert_eq!(q.strict, r#""ui" "x" "bug"*"#);
        assert_eq!(q.loose.as_deref(), Some(r#""ui" OR "bug"*"#));
        // Nothing to back-fill with: one token, or only stopwords.
        assert_eq!(FtsQuery::parse("embedding").unwrap().loose, None);
        assert_eq!(FtsQuery::parse("how do I").unwrap().loose, None);
        assert_eq!(FtsQuery::parse("   "), None);
        // Quotes inside a token stay literal.
        assert_eq!(
            FtsQuery::parse(r#"say"hi"#).unwrap().strict,
            r#""say""hi"*"#
        );
    }

    /// The rules apply to the word, not its punctuation: a trailing "?" or "," must not turn
    /// a stopword or a short token back into a prefix term.
    #[test]
    fn query_terms_ignore_surrounding_punctuation() {
        let q = FtsQuery::parse("how do I zqxjw this?").unwrap();
        assert_eq!(q.strict, r#""how" "do" "I" "zqxjw"* "this""#);
        assert_eq!(q.loose.as_deref(), Some(r#""zqxjw"*"#));
        let q = FtsQuery::parse("ui, (x) bug?").unwrap();
        assert_eq!(q.strict, r#""ui" "x" "bug"*"#);
        assert_eq!(q.loose.as_deref(), Some(r#""ui" OR "bug"*"#));
        assert_eq!(FtsQuery::parse("what did I do?").unwrap().loose, None);
        // Inner punctuation stays; a punctuation-only token is no term.
        let q = FtsQuery::parse("embed/mod.rs, -- ?").unwrap();
        assert_eq!(q.strict, r#""embed/mod.rs"*"#);
        assert_eq!(q.loose, None);
        assert_eq!(FtsQuery::parse("? !!"), None);
    }

    /// CJK and Hangul keep prefix terms at any length, in both passes: a 1-2 character word
    /// there is usually the start of a longer unicode61 token.
    #[test]
    fn short_non_ascii_tokens_stay_prefixes() {
        let q = FtsQuery::parse("오류 数 db").unwrap();
        assert_eq!(q.strict, r#""오류"* "数"* "db""#);
        assert_eq!(q.loose.as_deref(), Some(r#""오류"* OR "数"* OR "db""#));

        let mut conn = temp_db();
        thread_of(
            &mut conn,
            "cjk",
            &["빌드 오류가 발생했습니다 数据库连接失败"],
        );
        thread_of(&mut conn, "other", &["build error"]);
        let f = SearchFilters::default();
        // "오류" (error) is inside the token 오류가, "数据" (data) inside 数据库连接失败.
        assert_eq!(titles(&search(&conn, "오류", &f).unwrap()), vec!["cjk"]);
        assert_eq!(titles(&search(&conn, "数据", &f).unwrap()), vec!["cjk"]);
        assert_eq!(titles(&search(&conn, "오류?", &f).unwrap()), vec!["cjk"]);
    }

    fn thread_of(conn: &mut Connection, ext: &str, texts: &[&str]) {
        let sid = source_id(conn, "claude_code").unwrap();
        let t = ParsedThread {
            external_id: ext.into(),
            title: Some(ext.into()),
            messages: texts.iter().map(|x| msg("user", x, 100)).collect(),
            ..Default::default()
        };
        upsert_thread(conn, sid, &t).unwrap();
    }

    fn titles(hits: &[SearchHit]) -> Vec<String> {
        let mut t: Vec<String> = hits.iter().filter_map(|h| h.title.clone()).collect();
        t.sort();
        t.dedup();
        t
    }

    #[test]
    fn short_tokens_match_exactly_and_longer_ones_as_prefixes() {
        let mut conn = temp_db();
        thread_of(&mut conn, "exact", &["the ui is broken"]);
        thread_of(&mut conn, "prefixed", &["uikit layout and embeddings"]);
        let f = SearchFilters::default();
        assert_eq!(titles(&search(&conn, "ui", &f).unwrap()), vec!["exact"]);
        assert_eq!(titles(&search(&conn, "emb", &f).unwrap()), vec!["prefixed"]);
    }

    #[test]
    fn back_fill_ignores_stopwords() {
        let mut conn = temp_db();
        thread_of(&mut conn, "stopword-only", &["the cat sat on the mat"]);
        thread_of(&mut conn, "meaningful", &["webhook retries"]);
        // No message has both words, so the OR pass runs, on "webhook" alone: the old
        // all-prefix back-fill (`"the"* OR "webhook"*`) also returned the first thread.
        let hits = search(&conn, "the webhook", &SearchFilters::default()).unwrap();
        assert_eq!(titles(&hits), vec!["meaningful"]);
        // Only stopwords: the strict pass still matches them exactly.
        let hits = search(&conn, "the", &SearchFilters::default()).unwrap();
        assert_eq!(titles(&hits), vec!["stopword-only"]);
    }

    #[test]
    fn capped_len_matches_cap_per_thread() {
        let ordered = vec![
            mk_hit(1, 1),
            mk_hit(1, 2),
            mk_hit(1, 3),
            mk_hit(1, 4),
            mk_hit(2, 5),
            mk_hit(3, 6),
        ];
        for limit in [0, 2, 4, 5, 10] {
            assert_eq!(
                capped_len(&ordered, limit),
                cap_per_thread(ordered.clone(), limit).len(),
                "limit {limit}"
            );
        }
    }

    #[test]
    fn huge_messages_get_a_plain_snippet() {
        let mut conn = temp_db();
        let huge = format!("needle at the start {}", "needle filler ".repeat(20_000));
        assert!(huge.len() > SNIPPET_MAX_BYTES);
        thread_of(&mut conn, "huge", &[&huge]);
        thread_of(&mut conn, "small", &["one needle here"]);
        let hits = search(&conn, "needle", &SearchFilters::default()).unwrap();
        let snippet = |title: &str| {
            hits.iter()
                .find(|h| h.title.as_deref() == Some(title))
                .unwrap()
                .snippet
                .clone()
        };
        assert_eq!(snippet("huge"), huge[..240]);
        assert!(
            snippet("small").contains('\u{1}'),
            "small hits keep highlights"
        );
    }

    fn plan_of(conn: &Connection, sql: &str, args: &[Box<dyn ToSql>]) -> String {
        let refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let rows = stmt
            .query_map(refs.as_slice(), |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        rows.join(" | ")
    }

    /// FTS hits drive the query and each hit's filters come from the covering index; on an
    /// index from before 0031 the same search runs on the plain rowid join.
    #[test]
    fn fts_join_uses_the_covering_index_and_falls_back_without_it() {
        let mut conn = temp_db();
        thread_of(&mut conn, "t", &["covering index search"]);
        let filters = SearchFilters {
            sources: vec!["claude_code".into()],
            project: Some("proj".into()),
            after: Some(1),
            starred: Some(false),
            tags: vec!["x".into()],
            ..Default::default()
        };
        for f in [&SearchFilters::default(), &filters] {
            let (sql, args) = fts_sql(&conn, "\"covering\"*", f, 10);
            let plan = plan_of(&conn, &sql, &args);
            // FTS drives the messages side (a source filter may put the tiny sources table
            // outermost), and no message is ever reached by a scan or a rowid lookup.
            let fts = plan.find("SCAN messages_fts VIRTUAL TABLE").expect(&plan);
            let m = plan
                .find("SEARCH m USING INDEX idx_messages_cover (id=?)")
                .expect(&plan);
            assert!(fts < m, "{plan}");
            assert!(!plan.contains("SCAN m "), "{plan}");
        }

        conn.execute_batch("DROP INDEX idx_messages_cover").unwrap();
        let (sql, _) = fts_sql(&conn, "\"covering\"*", &SearchFilters::default(), 10);
        assert!(!sql.contains("INDEXED BY"), "{sql}");
        let hits = search(&conn, "covering index", &SearchFilters::default()).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn cached_stats_report_live_embedding_progress() {
        let mut conn = temp_db();
        let sid = source_id(&conn, "claude_code").unwrap();
        let thread = ParsedThread {
            external_id: "embed-progress".into(),
            messages: vec![msg("user", "hello there", 1), msg("assistant", "hi", 2)],
            ..Default::default()
        };
        upsert_thread(&mut conn, sid, &thread).unwrap();
        assert_eq!(stats_cached(&conn).unwrap().embedded, 0);
        // Embedding progress moves without the thread set changing (no cache invalidation).
        conn.execute("UPDATE messages SET embedded = 1", [])
            .unwrap();
        let s = stats_cached(&conn).unwrap();
        assert_eq!((s.embedded, s.embeddable), (2, 2));
    }
}
