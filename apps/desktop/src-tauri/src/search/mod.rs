//! Full-text search over indexed messages (SQLite FTS5 + BM25) plus the read
//! queries the UI needs: recent threads and a single thread's messages.

pub(crate) mod cache;

use crate::embed::{self, Embedder};
use anyhow::Result;
use rusqlite::{Connection, ToSql};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

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
    /// The full text's length in UTF-16 code units (JavaScript's `length`). Larger than
    /// `text.length` when the thread viewer got a tool row's preview
    /// ([`thread_detail_preview`]); [`message_text`] has the rest.
    pub text_len: usize,
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

/// The page size a search asks for (default 100, at most 500).
fn page_limit(filters: &SearchFilters) -> usize {
    filters.limit.unwrap_or(100).min(500) as usize
}

/// Run a full-text search, capped to `MAX_HITS_PER_THREAD` per thread. Empty query returns
/// nothing (use recent_threads instead).
pub fn search(conn: &Connection, query: &str, filters: &SearchFilters) -> Result<Vec<SearchHit>> {
    let limit = page_limit(filters);
    let fetch = (limit * THREAD_CAP_OVERFETCH).min(2000) as i64;
    let Some(q) = FtsQuery::parse(query) else {
        return Ok(Vec::new());
    };
    let mut hits = Vec::new();
    capped_pass(conn, &q.strict, filters, fetch, limit, &mut hits)?;
    // Back-fill only when the visible page would come up short. The strict pass usually
    // returns fewer than `fetch` rows (4x the page, for the per-thread cap), and the OR
    // hits are appended after the strict ones, so once the capped page is full they could
    // never be shown: running the OR pass anyway cost seconds on a large index.
    if let Some(loose) = &q.loose {
        if (hits.len() as i64) < fetch && capped_len(&hits, limit) < limit {
            capped_pass(conn, loose, filters, fetch, limit, &mut hits)?;
        }
    }
    Ok(cap_per_thread(hits, limit))
}

/// One FTS pass for [`search`], appended to `hits` (deduped, total capped at `fetch`).
///
/// A pass that comes back with all `fetch` rows was cut off, and when a few long threads
/// own those rows the capped page still comes up short: 'sqlite vec' filled a 200 page
/// with 88 hits from 40 threads while 192 threads matched. Only then is the pass rerun
/// with the per-thread cap in SQL ([`fts_window_sql`]: 200 hits from 104 threads in
/// 38 ms). Not by default: on a very common term the window costs more than the plain
/// pass ('error'* 294 ms against 128), and those terms rarely underfill.
fn capped_pass(
    conn: &Connection,
    match_query: &str,
    filters: &SearchFilters,
    fetch: i64,
    limit: usize,
    hits: &mut Vec<SearchHit>,
) -> Result<()> {
    let mut pass = run_fts(conn, match_query, filters, fetch)?;
    if pass.len() as i64 >= fetch {
        let mut merged = hits.clone();
        append_new(&mut merged, pass.clone(), fetch);
        if capped_len(&merged, limit) < limit {
            // Room for the snippet collapse in cap_per_thread to drop a few, plus the rows
            // already in `hits`: a back-fill pass's best rows are mostly the strict hits
            // again, which append_new drops, so a window of `limit` alone could come up short.
            let room = (limit + limit / 4 + hits.len()) as i64;
            let (sql, args) = fts_window_sql(conn, match_query, filters, room);
            pass = query_hits(conn, &sql, &args)?;
        }
    }
    append_new(hits, pass, fetch);
    Ok(())
}

/// Append the hits of `more` not already in `hits`, until `hits` holds `cap`.
fn append_new(hits: &mut Vec<SearchHit>, more: Vec<SearchHit>, cap: i64) {
    let seen: HashSet<i64> = hits.iter().map(|h| h.message_id).collect();
    for h in more {
        if (hits.len() as i64) >= cap {
            break;
        }
        if !seen.contains(&h.message_id) {
            hits.push(h);
        }
    }
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
        // Balanced double quotes mark exact phrases. Unbalanced ones (a stray `"`, or one
        // typed mid-query) fall back to plain terms, where they are trimmed or literal.
        if query.contains('"') && query.matches('"').count().is_multiple_of(2) {
            return Self::parse_with_phrases(query);
        }
        Self::parse_terms(&terms(query))
    }

    /// Plain terms only: the strict AND of every token, and the OR back-fill.
    fn parse_terms(tokens: &[&str]) -> Option<Self> {
        if tokens.is_empty() {
            return None;
        }
        let strict = tokens
            .iter()
            .map(|t| fts_term(t))
            .collect::<Vec<_>>()
            .join(" ");
        let meaningful = meaningful_terms(tokens);
        let loose = (tokens.len() > 1 && !meaningful.is_empty()).then(|| meaningful.join(" OR "));
        Some(Self { strict, loose })
    }

    /// A query with `"..."` phrases. Each phrase is one exact FTS5 phrase: `"read pool"`
    /// matched 704 messages in 18 ms, against 22,152 in 260 ms for the AND of the prefix
    /// terms `"read"* "pool"*` it used to become (top hit `readFileSync(POOL ...`). The
    /// phrases are required in both passes; the back-fill only relaxes the other terms.
    fn parse_with_phrases(query: &str) -> Option<Self> {
        let mut phrases = Vec::new();
        let mut rest = Vec::new();
        for (i, part) in query.split('"').enumerate() {
            if i % 2 == 1 {
                // Inside quotes. The split leaves no quote in the part, but double any
                // anyway so the phrase can never end early.
                let inner = part.split_whitespace().collect::<Vec<_>>().join(" ");
                if !terms(&inner).is_empty() {
                    phrases.push(format!("\"{}\"", inner.replace('"', "\"\"")));
                }
            } else {
                rest.push(part);
            }
        }
        let rest = rest.join(" ");
        let tokens = terms(&rest);
        if phrases.is_empty() {
            return Self::parse_terms(&tokens);
        }
        let required = phrases.join(" ");
        let strict = std::iter::once(required.clone())
            .chain(tokens.iter().map(|t| fts_term(t)))
            .collect::<Vec<_>>()
            .join(" ");
        // Nothing to relax without other terms, or with a single meaningful one (the OR of
        // one term is the strict pass again).
        let meaningful = meaningful_terms(&tokens);
        let loose = if tokens.is_empty() || (tokens.len() == 1 && meaningful.len() == 1) {
            None
        } else if meaningful.is_empty() {
            Some(required)
        } else {
            Some(format!("{required} AND ({})", meaningful.join(" OR ")))
        };
        Some(Self { strict, loose })
    }
}

/// A query's tokens by their core, without leading/trailing punctuation, which the FTS
/// tokenizer drops anyway: "this?" is the stopword "this" and "ui," the short token "ui",
/// not the prefixes this* and ui*. A token that is all punctuation ("?", "--") tokenizes
/// to nothing, so it is not a term at all.
fn terms(query: &str) -> Vec<&str> {
    query
        .split_whitespace()
        .map(|t| t.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|t| !t.is_empty())
        .collect()
}

/// The tokens worth an OR alternative, as FTS terms. A lone letter is as unselective as a
/// stopword once it stands on its own in an OR.
fn meaningful_terms(tokens: &[&str]) -> Vec<String> {
    tokens
        .iter()
        .filter(|t| !is_stopword(t) && !is_lone_ascii(t))
        .map(|t| fts_term(t))
        .collect()
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

/// One FTS pass for an already-built MATCH string, with all the non-text filters applied.
fn run_fts(
    conn: &Connection,
    match_query: &str,
    filters: &SearchFilters,
    limit: i64,
) -> Result<Vec<SearchHit>> {
    let (sql, args) = fts_sql(conn, match_query, filters, limit);
    query_hits(conn, &sql, &args)
}

/// Run a statement whose columns are a [`SearchHit`]'s, in field order.
fn query_hits(conn: &Connection, sql: &str, args: &[Box<dyn ToSql>]) -> Result<Vec<SearchHit>> {
    let args_ref: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(sql)?;
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

/// The recency boost window: a hit from the last 30 days ranks up to 15% better.
const RECENT_SECS: i64 = 30 * 86_400;

/// The keyword ranking: BM25 with tool output demoted and recent messages nudged up. BM25
/// is negative (better = more negative), so a factor under 1 demotes and over 1 promotes.
///
/// Tool output is 43% of the indexed rows (617k of 1.43M) and flooded the top: all ten of
/// the first results for 'migration' were git diff stats, `ls` listings and prisma logs.
/// Halving its score lets prose that names the term outrank a log that repeats it. The
/// recency factor ramps from 1.0 at 30 days old to 1.15 now: mild enough that a strong
/// old match still beats a weak new one. Both read only m.role and m.ts, which come from
/// idx_messages_cover, so the ranking adds no I/O. `recent_ph` binds now - RECENT_SECS.
fn rank_sql(recent_ph: usize) -> String {
    format!(
        "bm25(messages_fts)
           * (CASE m.role WHEN 'tool' THEN 0.5 ELSE 1.0 END)
           * (CASE WHEN m.ts > ?{recent_ph}
                   THEN min(1.15, 1.0 + 0.15 * (m.ts - ?{recent_ph}) / {RECENT_SECS}.0)
                   ELSE 1.0 END)"
    )
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A hit's snippet column. Use control-char sentinels (\u{1}/\u{2}) for match boundaries,
/// not HTML tags: snippet() does not escape the body, so the frontend HTML-escapes the text
/// and only then swaps the sentinels for <mark>, preventing injection from indexed content
/// (a thread might literally contain "<script>").
fn snippet_sql() -> String {
    format!(
        "CASE WHEN octet_length(m.text) > {SNIPPET_MAX_BYTES} THEN substr(m.text, 1, 240)
              ELSE snippet(messages_fts, 0, char(1), char(2), '…', 12) END"
    )
}

/// The SQL and arguments for [`run_fts`].
fn fts_sql(
    conn: &Connection,
    match_query: &str,
    filters: &SearchFilters,
    limit: i64,
) -> (String, Vec<Box<dyn ToSql>>) {
    let (from_where, mut args) = fts_from_where(conn, match_query, filters);
    args.push(Box::new(now_secs() - RECENT_SECS));
    let rank = rank_sql(args.len());
    args.push(Box::new(limit));
    let limit_ph = args.len();
    let snippet = snippet_sql();
    let sql = format!(
        "SELECT t.id, m.id, s.kind, t.title, t.project_path, m.role, {snippet}, m.ts
         {from_where}
         ORDER BY {rank} LIMIT ?{limit_ph}"
    );
    (sql, args)
}

/// [`fts_sql`] with the per-thread cap applied in SQL: at most [`MAX_HITS_PER_THREAD`] best
/// hits per thread, then the best `limit` of those. The window runs over ids and scores
/// only, and the snippets come from a second scan of the same MATCH that computes them
/// just for the chosen rows: computing them inside the window's subquery costs one per
/// match (2.7 s for 'migration'*'s 72k), and a MATCH per chosen rowid ~1.5 ms each.
fn fts_window_sql(
    conn: &Connection,
    match_query: &str,
    filters: &SearchFilters,
    limit: i64,
) -> (String, Vec<Box<dyn ToSql>>) {
    let (from_where, mut args) = fts_from_where(conn, match_query, filters);
    args.push(Box::new(now_secs() - RECENT_SECS));
    let rank = rank_sql(args.len());
    args.push(Box::new(limit));
    let limit_ph = args.len();
    let snippet = snippet_sql();
    // The `+` keeps the chosen-id list a filter on each FTS row rather than a rowid
    // constraint FTS5 would answer with one MATCH per id.
    let sql = format!(
        "WITH chosen AS MATERIALIZED (
            SELECT mid FROM (
                SELECT mid, score,
                       row_number() OVER (PARTITION BY tid ORDER BY score) AS rn
                FROM (SELECT m.id AS mid, m.thread_id AS tid, {rank} AS score {from_where})
            )
            WHERE rn <= {MAX_HITS_PER_THREAD} ORDER BY score LIMIT ?{limit_ph}
         )
         SELECT t.id, m.id, s.kind, t.title, t.project_path, m.role, {snippet}, m.ts
         {from_where} AND +m.id IN (SELECT mid FROM chosen)
         ORDER BY {rank} LIMIT ?{limit_ph}"
    );
    (sql, args)
}

/// The `FROM ... WHERE` of an FTS query (MATCH bound as ?1, then the filters), shared by
/// [`fts_sql`] and [`fts_window_sql`].
fn fts_from_where(
    conn: &Connection,
    match_query: &str,
    filters: &SearchFilters,
) -> (String, Vec<Box<dyn ToSql>>) {
    let mut sql = format!(
        "FROM messages_fts
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
    (sql, args)
}

/// Most semantic hits the hybrid fusion takes. RRF weight past rank 60 is at most 1/121,
/// and the similarity floor empties the tail anyway, while the KNN's cost grows with k: the
/// UI's page of 200 asked for 200 (knn_k 1000), now 60 (knn_k 300).
const SEM_ARM_MAX: usize = 60;

/// The semantic arm of a hybrid search: the KNN under every filter, then the relative
/// cutoff ([`embed::SEM_RELATIVE_CUTOFF`]).
fn semantic_arm(conn: &Connection, qv: &[f32], filters: &SearchFilters) -> Result<Vec<(i64, f32)>> {
    let k = page_limit(filters).clamp(50, SEM_ARM_MAX);
    let sem = embed::semantic_search_scoped(conn, qv, &embed::KnnScope::of(filters), k)?;
    Ok(relative_cutoff(sem))
}

/// Drop semantic hits more than [`embed::SEM_RELATIVE_CUTOFF`] below the best one. The
/// absolute floor can't separate a weak real match from noise on its own (bge-small scores
/// near-duplicates of anything at 0.6+), but a hit far below this query's own best is
/// filler. `sem` is best-first, as the KNN returns it.
fn relative_cutoff(mut sem: Vec<(i64, f32)>) -> Vec<(i64, f32)> {
    if let Some(&(_, top)) = sem.first() {
        sem.retain(|&(_, sim)| sim >= top - embed::SEM_RELATIVE_CUTOFF);
    }
    sem
}

/// Hybrid search: fuse keyword (FTS5/BM25) and semantic (cosine) result lists with
/// Reciprocal Rank Fusion. Keyword-matched hits keep their highlighted snippet;
/// semantic-only hits get a plain leading-text snippet.
///
/// The keyword arm runs on `conn` while the query embedding and the KNN run on a second
/// read-only connection to the same file, so the latency is the slower arm's rather than
/// the sum (the KNN alone is 250-480 ms on a 317k-vector index). An in-memory or unnamed
/// database has no second connection, and runs the arms one after the other.
pub fn hybrid(
    conn: &Connection,
    embedder: &Embedder,
    query: &str,
    filters: &SearchFilters,
) -> Result<Vec<SearchHit>> {
    let path = conn
        .path()
        .filter(|p| !p.is_empty())
        .map(std::path::PathBuf::from);
    match path {
        Some(path) => hybrid_with(conn, query, filters, || {
            let Some(qv) = embed::embed_query(embedder, query)? else {
                return Ok(Vec::new());
            };
            let sem_conn = crate::db::open_readonly(&path)?;
            semantic_arm(&sem_conn, &qv, filters)
        }),
        None => {
            let qv = embed::embed_query(embedder, query)?;
            hybrid_vec(conn, query, qv.as_deref(), filters)
        }
    }
}

/// Hybrid (keyword + semantic) search with an ALREADY-embedded query vector, both arms on
/// `conn`, one after the other. No model inference, so callers on the UI path should run
/// `embed::embed_query` BEFORE taking the DB lock. `qv == None` skips the semantic arm
/// (keyword-only).
pub fn hybrid_vec(
    conn: &Connection,
    query: &str,
    qv: Option<&[f32]>,
    filters: &SearchFilters,
) -> Result<Vec<SearchHit>> {
    // The keyword arm is the capped page search() would show: an uncapped arm of `limit`
    // rows came up short after the fused cap the same way search() used to ('sqlite vec'
    // gave 30 hybrid hits from 11 threads once the floor stopped padding it with noise).
    // The fused output is capped again, so a thread's semantic hits still compete with
    // its keyword ones for its slots.
    let fts = search(conn, query, filters)?;
    let sem = match qv {
        Some(v) => semantic_arm(conn, v, filters)?,
        None => Vec::new(),
    };
    fuse_hits(conn, fts, &sem, filters)
}

/// Hybrid search with the semantic arm (`semantic`: embed + KNN on a connection of its
/// own) on a scoped thread while the keyword arm runs on `conn`.
fn hybrid_with<F>(
    conn: &Connection,
    query: &str,
    filters: &SearchFilters,
    semantic: F,
) -> Result<Vec<SearchHit>>
where
    F: FnOnce() -> Result<Vec<(i64, f32)>> + Send,
{
    let (fts, sem) = std::thread::scope(|s| {
        let sem = s.spawn(semantic);
        let fts = search(conn, query, filters);
        let sem = sem
            .join()
            .unwrap_or_else(|_| Err(anyhow::anyhow!("the semantic search thread panicked")));
        (fts, sem)
    });
    fuse_hits(conn, fts?, &sem?, filters)
}

/// Fuse the two arms (RRF), materialize the hits in fused order and cap them per thread.
fn fuse_hits(
    conn: &Connection,
    fts: Vec<SearchHit>,
    sem: &[(i64, f32)],
    filters: &SearchFilters,
) -> Result<Vec<SearchHit>> {
    let fts_ids: Vec<i64> = fts.iter().map(|h| h.message_id).collect();
    let ranked = fuse_rrf(&fts_ids, sem);

    // FTS hits are already in hand; semantic-only hits need one PK fetch each (at most
    // SEM_ARM_MAX of them).
    let mut fts_by_id: HashMap<i64, SearchHit> =
        fts.into_iter().map(|h| (h.message_id, h)).collect();
    let mut ordered = Vec::with_capacity(ranked.len());
    for (id, _) in ranked {
        if let Some(h) = fts_by_id.remove(&id) {
            ordered.push(h);
        } else if let Some(h) = hit_for_message(conn, id)? {
            ordered.push(h);
        }
    }
    Ok(cap_per_thread(ordered, page_limit(filters)))
}

/// The app's search response.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResponse {
    pub hits: Vec<SearchHit>,
    /// Why a hybrid search ran keyword-only, or None when it did not: [`SEMANTIC_LOADING`]
    /// while another thread loads the model (the UI retries shortly), or
    /// [`SEMANTIC_UNAVAILABLE`] (not downloaded, failed to load or to embed, no connection
    /// for the KNN). The UI should not keep this result as the semantic answer.
    pub semantic_skipped: Option<&'static str>,
    /// A newer search started before this one finished, so it was cut short and `hits` is
    /// empty. Ignore it.
    pub superseded: bool,
}

/// Supersession for the app's search box. Every keystroke the debounce lets through starts
/// a search, and warm common-term and sentence queries take 250-370 ms keyword and 600-950
/// ms hybrid, longer than the 260 ms debounce, so stale searches used to pile up on the
/// 2-6 pooled readers and delay the thread viewer behind them.
///
/// Each search takes a generation from [`SearchGen::begin`], which interrupts every older
/// search's connections (`sqlite3_interrupt`, so their statements fail with
/// SQLITE_INTERRUPT and the result comes back `superseded`). A connection is registered
/// only while the search holds it ([`SearchGen::watch`]); the registration ends before the
/// connection goes back to the pool, so an interrupt can never land on another command's
/// query.
#[derive(Default)]
pub struct SearchGen {
    current: AtomicU64,
    next_token: AtomicU64,
    running: Mutex<Vec<(u64, u64, rusqlite::InterruptHandle)>>,
}

impl SearchGen {
    /// Start a search: it becomes the current generation, and every older one still
    /// running is interrupted.
    pub fn begin(&self) -> u64 {
        let gen = self.current.fetch_add(1, Ordering::SeqCst) + 1;
        for (g, _, handle) in self.running().iter() {
            if *g < gen {
                handle.interrupt();
            }
        }
        gen
    }

    /// Whether `gen` is still the newest search.
    pub fn is_current(&self, gen: u64) -> bool {
        self.current.load(Ordering::SeqCst) == gen
    }

    /// Make `conn` interruptible by newer searches while the returned guard lives. None
    /// when `gen` is already superseded: checked under the same lock `begin` interrupts
    /// under, so a search can't slip in between the check and the registration. Keep the
    /// guard declared after the connection, so it drops first.
    pub fn watch(&self, gen: u64, conn: &Connection) -> Option<SearchWatch<'_>> {
        let mut running = self.running();
        if !self.is_current(gen) {
            return None;
        }
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        running.push((gen, token, conn.get_interrupt_handle()));
        Some(SearchWatch { gens: self, token })
    }

    fn running(&self) -> std::sync::MutexGuard<'_, Vec<(u64, u64, rusqlite::InterruptHandle)>> {
        self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A connection's registration with [`SearchGen`]; dropping it ends the registration.
pub struct SearchWatch<'a> {
    gens: &'a SearchGen,
    token: u64,
}

impl Drop for SearchWatch<'_> {
    fn drop(&mut self) {
        self.gens.running().retain(|(_, t, _)| *t != self.token);
    }
}

/// Whether `e` is a statement cut short by `sqlite3_interrupt`.
fn is_interrupted(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<rusqlite::Error>().is_some_and(|e| {
            e.sqlite_error_code() == Some(rusqlite::ErrorCode::OperationInterrupted)
        })
    })
}

/// How long the semantic arm waits for a pooled reader before opening its own. Not
/// `try_get`: the pool keeps one reader idle, so that would skip the pool whenever the
/// keyword arm holds it, while r2d2 opens another below `max_size` in about 1 ms.
const SEM_CONN_WAIT: std::time::Duration = std::time::Duration::from_millis(50);

/// `SearchResponse::semantic_skipped` while the model is loading: the UI retries shortly.
pub const SEMANTIC_LOADING: &str = "loading";
/// `SearchResponse::semantic_skipped` when semantic search can't run for this query.
pub const SEMANTIC_UNAVAILABLE: &str = "unavailable";

/// The app's search: keyword, or hybrid with the query embedding and the KNN on a second
/// reader while the keyword arm runs, under generation `gen` from `gens.begin()` (see
/// [`SearchGen`]). The embedding is cached-only: a keystroke never waits on (or starts) a
/// model download, and without a usable model the search is keyword-only.
pub fn ui_search(
    pool: &crate::db::ReadPool,
    gens: &SearchGen,
    gen: u64,
    embedder: &Embedder,
    query: &str,
    filters: &SearchFilters,
) -> Result<SearchResponse> {
    ui_search_with(pool, gens, gen, query, filters, |q| {
        embed::embed_query_no_wait(embedder, q)
    })
}

/// [`ui_search`] with the query embedding injected (tests stub the model).
fn ui_search_with<E>(
    pool: &crate::db::ReadPool,
    gens: &SearchGen,
    gen: u64,
    query: &str,
    filters: &SearchFilters,
    embed_query: E,
) -> Result<SearchResponse>
where
    E: Fn(&str) -> std::result::Result<Option<Vec<f32>>, embed::EmbedError> + Sync,
{
    let superseded = SearchResponse {
        superseded: true,
        ..Default::default()
    };
    if !gens.is_current(gen) {
        return Ok(superseded);
    }
    let semantic_skipped = std::sync::OnceLock::<&'static str>::new();
    let run = || -> Result<Option<Vec<SearchHit>>> {
        let conn = pool
            .0
            .get()
            .map_err(|e| anyhow::anyhow!("read pool: {e}"))?;
        let Some(_watch) = gens.watch(gen, &conn) else {
            return Ok(None);
        };
        if !filters.hybrid {
            return search(&conn, query, filters).map(Some);
        }
        let index_path = conn
            .path()
            .filter(|p| !p.is_empty())
            .map(std::path::PathBuf::from);
        hybrid_with(&conn, query, filters, || {
            if !gens.is_current(gen) {
                return Ok(Vec::new());
            }
            let qv = match embed_query(query) {
                Ok(Some(qv)) => qv,
                Ok(None) => return Ok(Vec::new()), // an empty query: nothing to skip
                Err(e) => {
                    // Only an inference failure is news: a missing or failed model gives
                    // the same answer on every keystroke and would flood the log.
                    if !e.is_model_unavailable() {
                        app_log!("[search] semantic arm skipped: {e}");
                    }
                    let why = match e {
                        embed::EmbedError::ModelLoading => SEMANTIC_LOADING,
                        _ => SEMANTIC_UNAVAILABLE,
                    };
                    let _ = semantic_skipped.set(why);
                    return Ok(Vec::new());
                }
            };
            // The embedding took a while: a newer search makes the KNN pointless.
            if !gens.is_current(gen) {
                return Ok(Vec::new());
            }
            let knn = |c: &Connection| {
                let Some(_watch) = gens.watch(gen, c) else {
                    return Ok(Vec::new());
                };
                semantic_arm(c, &qv, filters)
            };
            // Never wait long for a pooled reader: with the pool at its minimum of 2, two
            // overlapping keystroke searches left both semantic arms waiting (2 s, not
            // interruptible) on readers the two keyword arms held. A full pool gets a
            // fresh read-only connection instead, as `hybrid` uses.
            if let Ok(c) = pool.0.get_timeout(SEM_CONN_WAIT) {
                return knn(&c);
            }
            match index_path.as_deref().map(crate::db::open_readonly) {
                Some(Ok(c)) => knn(&c),
                Some(Err(e)) => {
                    app_log!("[search] semantic arm skipped, no reader: {e:#}");
                    let _ = semantic_skipped.set(SEMANTIC_UNAVAILABLE);
                    Ok(Vec::new())
                }
                None => {
                    let _ = semantic_skipped.set(SEMANTIC_UNAVAILABLE);
                    Ok(Vec::new())
                }
            }
        })
        .map(Some)
    };
    match run() {
        Ok(Some(hits)) if gens.is_current(gen) => Ok(SearchResponse {
            hits,
            semantic_skipped: semantic_skipped.get().copied(),
            superseded: false,
        }),
        Ok(_) => Ok(superseded),
        Err(e) if is_interrupted(&e) => Ok(superseded),
        Err(e) => Err(e),
    }
}

/// When [`warm_semantic`] last ran: it reads the whole vector table, so a user toggling the
/// semantic switch repeatedly warms once per window.
static LAST_WARM: Mutex<Option<std::time::Instant>> = Mutex::new(None);
const WARM_EVERY: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Get the semantic arm ready before the first hybrid query: load the model (only from the
/// cache, never a download) and read the vector table once so the first KNN is not page
/// faulting through it. After a cold start the first sentence query took 2,316 ms hybrid
/// against 947 warm, and one that arrived while the model was loading ran keyword-only.
/// Reads on a connection of its own, not a pooled one, so searches never wait behind it.
pub fn warm_semantic(embedder: &Embedder, index: &std::path::Path) -> Result<()> {
    {
        let mut last = LAST_WARM.lock().unwrap_or_else(PoisonError::into_inner);
        if last.is_some_and(|at| at.elapsed() < WARM_EVERY) {
            return Ok(());
        }
        *last = Some(std::time::Instant::now());
    }
    if embed::model_is_cached() {
        if let Err(e) = embedder.ensure_loaded() {
            app_log!("[search] warm: model load failed: {e}");
        }
    }
    let conn = crate::db::open_readonly(index)?;
    if !crate::db::has_table(&conn, "vec_chunks_vector_chunks00") {
        return Ok(());
    }
    // Reading each blob (not length(), which SQLite answers from the record header) pulls
    // the vector pages into the OS cache, in rowid order, so sequentially.
    let mut stmt = conn.prepare("SELECT vectors FROM vec_chunks_vector_chunks00")?;
    let mut rows = stmt.query([])?;
    let mut bytes = 0usize;
    while let Some(row) = rows.next()? {
        bytes += row.get_ref(0)?.as_blob_or_null()?.map_or(0, <[u8]>::len);
    }
    app_log!("[search] warm: read {} MB of vectors", bytes >> 20);
    Ok(())
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

/// Snippets at least this long are collapsed across the list when identical. The same
/// file read or pasted CLAUDE.md shows up verbatim in many threads (one sentence query had
/// 27 duplicate snippets in its 200 hits, the same read in threads 18374, 19574 and 19787).
/// Shorter ones ("yes", "done") are legitimately the same in different threads.
const MIN_COLLAPSED_SNIPPET_BYTES: usize = 32;

/// The per-thread cap and the duplicate-snippet collapse, shared by [`cap_per_thread`] and
/// [`capped_len`] so the two can never disagree.
#[derive(Default)]
struct Capper {
    per_thread: HashMap<i64, usize>,
    snippets: HashSet<u64>,
}

impl Capper {
    /// Whether `h` makes the list, given every hit kept before it.
    fn keep(&mut self, h: &SearchHit) -> bool {
        let count = self.per_thread.entry(h.thread_id).or_insert(0);
        if *count >= MAX_HITS_PER_THREAD {
            return false;
        }
        if h.snippet.len() >= MIN_COLLAPSED_SNIPPET_BYTES {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            h.snippet.hash(&mut hasher);
            if !self.snippets.insert(hasher.finish()) {
                return false;
            }
        }
        *count += 1;
        true
    }
}

/// How many of `ordered` [`cap_per_thread`] would keep, without consuming them.
fn capped_len(ordered: &[SearchHit], limit: usize) -> usize {
    let mut capper = Capper::default();
    ordered
        .iter()
        .filter(|h| capper.keep(h))
        .take(limit)
        .count()
}

/// Limit how many hits any single thread contributes (preserving fused/ranked order), drop
/// repeats of an identical snippet, then trim to `limit`. One long thread can otherwise
/// occupy every top slot and bury the rest; the dropped overflow lets other threads
/// back-fill (callers over-fetch to leave room).
fn cap_per_thread(ordered: Vec<SearchHit>, limit: usize) -> Vec<SearchHit> {
    let mut capper = Capper::default();
    let mut out = Vec::with_capacity(limit.min(ordered.len()));
    for h in ordered {
        if out.len() >= limit {
            break;
        }
        if capper.keep(&h) {
            out.push(h);
        }
    }
    out
}

/// Map a cosine similarity in `[SEM_SIMILARITY_FLOOR, SEM_FULL_WEIGHT_SIM]` to an RRF weight
/// in `[0.5, 1.0]`: a strong match keeps full rank-weight, the weakest retained match keeps
/// half. Monotonic and clamped, so it only ever demotes weak semantic matches, never
/// inflates above the keyword arm. bge-small's cosines for real matches top out well
/// below 1.0, so the scale ends at [`embed::SEM_FULL_WEIGHT_SIM`]: over [floor, 1.0] even
/// the best hit of a typical query kept only about three quarters of its weight.
fn sem_weight(sim: f32) -> f32 {
    const MIN_W: f32 = 0.5;
    let floor = embed::SEM_SIMILARITY_FLOOR;
    let full = embed::SEM_FULL_WEIGHT_SIM;
    let norm = ((sim - floor) / (full - floor)).clamp(0.0, 1.0);
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
    let Some(qv) = embed::embed_query(embedder, context)? else {
        return Ok(Vec::new());
    };
    related_vec(conn, &qv, filters)
}

/// [`related`] with the context already embedded. Its own floor
/// ([`embed::SEM_RELATED_FLOOR`]): code and error context scores lower against prose than
/// a search-box query, so the hybrid floor could leave a lookup that found threads before
/// it with none. The relative cutoff still sheds the filler under a strong best hit.
fn related_vec(
    conn: &Connection,
    qv: &[f32],
    filters: &SearchFilters,
) -> Result<Vec<ThreadSummary>> {
    let limit = filters.limit.unwrap_or(5).clamp(1, 50) as usize;
    let scope = embed::KnnScope {
        floor: embed::SEM_RELATED_FLOOR,
        include_subagents: filters.include_subagents,
        sources: &filters.sources,
        project: filters.project.as_deref(),
        ..embed::KnnScope::default()
    };
    // Over-fetch message hits so dedup-to-thread still leaves `limit` threads.
    let hits = relative_cutoff(embed::semantic_search_scoped(conn, qv, &scope, limit * 8)?);

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
    thread_detail_with(conn, thread_id, None)
}

/// How much of a tool row's text the thread viewer gets up front.
pub const TOOL_PREVIEW_CHARS: usize = 300;

/// [`thread_detail`] for the thread viewer: tool rows (tool output, and tool calls with
/// `tool_name` set) carry only their first [`TOOL_PREVIEW_CHARS`] characters, with the full
/// length in `text_len`. The viewer shows them collapsed and fetches the rest with
/// [`message_text`] on expand. Tool rows were 83% of the biggest thread's payload (4.49M of
/// 5.40M chars, 6.3 MB of JSON for 7,570 messages), all of it sent on every open.
pub fn thread_detail_preview(conn: &Connection, thread_id: i64) -> Result<Option<ThreadDetail>> {
    thread_detail_with(conn, thread_id, Some(TOOL_PREVIEW_CHARS))
}

fn thread_detail_with(
    conn: &Connection,
    thread_id: i64,
    tool_preview: Option<usize>,
) -> Result<Option<ThreadDetail>> {
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
        let role: String = r.get(1)?;
        let tool_name: Option<String> = r.get(3)?;
        let mut text: String = r.get(2)?;
        // Measured and cut in Rust, not SQL: the length must be in the frontend's units
        // (SQLite's length() counts code points), and substr() reads the whole value anyway.
        let text_len = text.encode_utf16().count();
        if let Some(keep) = tool_preview {
            if role == "tool" || tool_name.is_some() {
                if let Some((cut, _)) = text.char_indices().nth(keep) {
                    text.truncate(cut);
                }
            }
        }
        Ok(MessageRow {
            id: r.get(0)?,
            role,
            text,
            tool_name,
            ts: r.get(4)?,
            model: r.get(5)?,
            text_len,
        })
    })?;
    detail.messages = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Some(detail))
}

/// One message's full text (a tool row the thread viewer got as a preview). None for an
/// unknown id.
pub fn message_text(conn: &Connection, message_id: i64) -> Result<Option<String>> {
    match conn.query_row(
        "SELECT text FROM messages WHERE id = ?1",
        [message_id],
        |r| r.get(0),
    ) {
        Ok(text) => Ok(Some(text)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
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
        let full = crate::embed::SEM_FULL_WEIGHT_SIM;
        assert!(
            (sem_weight(full) - 1.0).abs() < 1e-6,
            "a strong match keeps full weight"
        );
        assert!((sem_weight(1.0) - 1.0).abs() < 1e-6, "clamped above");
        assert!(
            (sem_weight(floor) - 0.5).abs() < 1e-6,
            "floor similarity keeps half weight"
        );
        // Below the floor clamps (the arm is pre-floored, but be safe).
        assert!((sem_weight(floor - 0.1) - 0.5).abs() < 1e-6);
        // Strictly increasing across the retained range.
        let mid = (floor + full) / 2.0;
        assert!(sem_weight(floor + 0.01) < sem_weight(mid));
        assert!(sem_weight(mid) < sem_weight(full - 0.01));
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

    /// A vector at a known cosine from `direction(0.0)`: cos = 1/sqrt(1 + x^2).
    fn direction(x: f32) -> Vec<f32> {
        let mut v = vec![0.0_f32; crate::embed::DIM];
        v[0] = 1.0;
        v[1] = x;
        v
    }

    /// `(external id, text, starred, tag, ts, x)`: one thread of [`semantic_corpus`].
    type CorpusRow<'a> = (&'a str, &'a str, bool, Option<&'a str>, i64, f32);

    /// One thread per row, its one message with a vector at `direction(x)`. Returns the
    /// thread ids in input order.
    fn semantic_corpus(conn: &mut Connection, rows: &[CorpusRow<'_>]) -> Vec<i64> {
        let sid = source_id(conn, "claude_code").unwrap();
        let mut threads = Vec::new();
        let mut ids = Vec::new();
        let mut vecs = Vec::new();
        for (ext, text, starred, tag, ts, x) in rows {
            let t = ParsedThread {
                external_id: (*ext).into(),
                title: Some((*ext).into()),
                messages: vec![msg("assistant", text, *ts)],
                ..Default::default()
            };
            upsert_thread(conn, sid, &t).unwrap();
            let (tid, mid): (i64, i64) = conn
                .query_row(
                    "SELECT t.id, m.id FROM threads t JOIN messages m ON m.thread_id = t.id
                     WHERE t.external_id = ?1",
                    [ext],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            set_star(conn, tid, *starred).unwrap();
            if let Some(tag) = tag {
                set_thread_tags(conn, tid, &[(*tag).to_string()], 1).unwrap();
            }
            threads.push(tid);
            ids.push(mid);
            vecs.push(direction(*x));
        }
        crate::embed::store_batch(conn, &ids, &ids, &vecs).unwrap();
        threads
    }

    /// Hybrid scopes its semantic arm by every filter the keyword arm honors: semantic-only
    /// hits from unstarred, untagged or out-of-range threads used to leak through (24 of 30
    /// `--starred` hybrid hits on the real index), with or without the owner pre-filter.
    #[test]
    fn hybrid_semantic_arm_honors_every_filter() {
        let mut conn = temp_db();
        let ids = semantic_corpus(
            &mut conn,
            &[
                ("starred", "talk about databases", true, None, 1_000, 0.10),
                ("plain", "talk about databases", false, None, 1_000, 0.05),
                (
                    "tagged",
                    "talk about databases",
                    false,
                    Some("infra"),
                    5_000,
                    0.15,
                ),
            ],
        );
        let (starred, plain, tagged) = (ids[0], ids[1], ids[2]);
        let q = direction(0.0);
        let threads = |conn: &Connection, f: SearchFilters| {
            let mut t: Vec<i64> = hybrid_vec(conn, "zzqx", Some(&q), &f)
                .unwrap()
                .iter()
                .map(|h| h.thread_id)
                .collect();
            t.sort_unstable();
            t
        };
        for owner_map in [true, false] {
            if !owner_map {
                conn.execute_batch("DROP TABLE vec_chunk_owner").unwrap();
            }
            let all = threads(&conn, SearchFilters::default());
            assert_eq!(all.len(), 3, "unfiltered: every thread ({owner_map})");
            let only = |f: SearchFilters| threads(&conn, f);
            assert_eq!(
                only(SearchFilters {
                    starred: Some(true),
                    ..Default::default()
                }),
                vec![starred],
                "starred ({owner_map})"
            );
            assert_eq!(
                only(SearchFilters {
                    tags: vec!["infra".into()],
                    ..Default::default()
                }),
                vec![tagged],
                "tags ({owner_map})"
            );
            assert_eq!(
                only(SearchFilters {
                    after: Some(2_000),
                    ..Default::default()
                }),
                vec![tagged],
                "after ({owner_map})"
            );
            let mut early = only(SearchFilters {
                before: Some(2_000),
                ..Default::default()
            });
            early.sort_unstable();
            let mut want = vec![starred, plain];
            want.sort_unstable();
            assert_eq!(early, want, "before ({owner_map})");
        }
    }

    #[test]
    fn relative_cutoff_drops_hits_far_below_the_best() {
        let cut = crate::embed::SEM_RELATIVE_CUTOFF;
        let kept = relative_cutoff(vec![
            (1, 0.82),
            (2, 0.82 - cut + 0.001),
            (3, 0.82 - cut - 0.01),
        ]);
        assert_eq!(kept.iter().map(|x| x.0).collect::<Vec<_>>(), vec![1, 2]);
        assert!(relative_cutoff(Vec::new()).is_empty());
    }

    #[test]
    fn quoted_phrases_become_fts_phrases() {
        let q = FtsQuery::parse(r#""read pool""#).unwrap();
        assert_eq!(q.strict, r#""read pool""#);
        assert_eq!(q.loose, None);
        // Phrases are required in the back-fill too; only the other terms relax.
        let q = FtsQuery::parse(r#"the "read pool" timeout fix"#).unwrap();
        assert_eq!(q.strict, r#""read pool" "the" "timeout"* "fix"*"#);
        assert_eq!(
            q.loose.as_deref(),
            Some(r#""read pool" AND ("timeout"* OR "fix"*)"#)
        );
        // One extra term: its OR is the strict pass again, so no back-fill.
        assert_eq!(
            FtsQuery::parse(r#""read pool" timeout"#).unwrap().loose,
            None
        );
        // Only stopwords besides the phrase: the back-fill drops them.
        let q = FtsQuery::parse(r#""read pool" the"#).unwrap();
        assert_eq!(q.loose.as_deref(), Some(r#""read pool""#));
        // Two phrases; whitespace inside a phrase is normalized.
        let q = FtsQuery::parse(r#""embed/mod.rs"  "semantic   search""#).unwrap();
        assert_eq!(q.strict, r#""embed/mod.rs" "semantic search""#);
        // Empty or punctuation-only quotes are no phrase.
        let q = FtsQuery::parse(r#""" webhook "?""#).unwrap();
        assert_eq!(q.strict, r#""webhook"*"#);
        assert_eq!(FtsQuery::parse(r#""""#), None);
    }

    /// An odd number of quotes is a typo (or a quote still being typed), not a phrase: the
    /// query falls back to plain terms, as before phrases existed.
    #[test]
    fn unbalanced_quotes_fall_back_to_terms() {
        let q = FtsQuery::parse(r#""read pool"#).unwrap();
        assert_eq!(q.strict, r#""read"* "pool"*"#);
        assert_eq!(q.loose.as_deref(), Some(r#""read"* OR "pool"*"#));
        let q = FtsQuery::parse(r#"say"hi "read pool""#).unwrap();
        assert_eq!(q.strict, r#""say""hi"* "read"* "pool"*"#);
    }

    #[test]
    fn phrase_search_matches_adjacent_words_only() {
        let mut conn = temp_db();
        thread_of(&mut conn, "phrase", &["the read pool is exhausted"]);
        thread_of(
            &mut conn,
            "apart",
            &["read the file, then pool the results"],
        );
        let f = SearchFilters::default();
        assert_eq!(
            titles(&search(&conn, r#""read pool""#, &f).unwrap()),
            vec!["phrase"]
        );
        assert_eq!(
            titles(&search(&conn, "read pool", &f).unwrap()),
            vec!["apart", "phrase"]
        );
    }

    fn thread_with(conn: &mut Connection, ext: &str, messages: Vec<ParsedMessage>) {
        let sid = source_id(conn, "claude_code").unwrap();
        let t = ParsedThread {
            external_id: ext.into(),
            title: Some(ext.into()),
            messages,
            ..Default::default()
        };
        upsert_thread(conn, sid, &t).unwrap();
    }

    #[test]
    fn tool_output_ranks_below_prose_and_recent_above_old() {
        let mut conn = temp_db();
        let now = now_secs();
        let old = now - 90 * 86_400;
        // The same text, so BM25 alone would tie: the role decides.
        thread_with(
            &mut conn,
            "tool",
            vec![msg("tool", "prisma migration applied", old)],
        );
        thread_with(
            &mut conn,
            "prose",
            vec![msg("assistant", "prisma migration applied", old)],
        );
        let hits = search(&conn, "migration", &SearchFilters::default()).unwrap();
        assert_eq!(hits[0].title.as_deref(), Some("prose"), "{hits:?}");
        assert_eq!(hits[1].title.as_deref(), Some("tool"));

        let mut conn = temp_db();
        thread_with(&mut conn, "old", vec![msg("user", "webhook retries", old)]);
        thread_with(
            &mut conn,
            "new",
            vec![msg("user", "webhook retries", now - 3_600)],
        );
        let hits = search(&conn, "webhook", &SearchFilters::default()).unwrap();
        assert_eq!(hits[0].title.as_deref(), Some("new"), "{hits:?}");
    }

    #[test]
    fn identical_snippets_collapse_across_threads() {
        let mut conn = temp_db();
        let doc = "CLAUDE.md says: run the linter before every commit, and keep the tests green";
        thread_of(&mut conn, "a", &[doc]);
        thread_of(&mut conn, "b", &[doc]);
        thread_of(&mut conn, "c", &["the linter runs in CI as well"]);
        let hits = search(&conn, "linter", &SearchFilters::default()).unwrap();
        assert_eq!(hits.len(), 2, "{hits:?}");
        // Short snippets ("yes", "done") are the same in many threads on purpose.
        let mut a = mk_hit(1, 1);
        a.snippet = "done".into();
        let mut b = mk_hit(2, 2);
        b.snippet = "done".into();
        assert_eq!(cap_per_thread(vec![a, b], 10).len(), 2);
    }

    /// When one long thread owns every fetched row, the capped page comes up short although
    /// other threads match; the windowed rerun fills it with them.
    #[test]
    fn underfilled_page_reruns_with_the_window_cap() {
        let mut conn = temp_db();
        // limit 5 fetches 20 rows: 25 strong matches in one thread fill them all.
        thread_with(
            &mut conn,
            "dominant",
            (0..25)
                .map(|i| msg("user", "alpha alpha alpha", 100 + i))
                .collect(),
        );
        for name in ["w1", "w2", "w3", "w4"] {
            // Distinct texts: identical snippets would collapse to one.
            let text =
                format!("alpha appears once in {name}, a much longer message about other things");
            thread_of(&mut conn, name, &[&text]);
        }
        let f = SearchFilters {
            limit: Some(5),
            ..Default::default()
        };
        let strict = run_fts(&conn, "\"alpha\"*", &f, 20).unwrap();
        assert!(strict
            .iter()
            .all(|h| h.title.as_deref() == Some("dominant")));
        let hits = search(&conn, "alpha", &f).unwrap();
        assert_eq!(hits.len(), 5, "{hits:?}");
        assert_eq!(
            hits.iter()
                .filter(|h| h.title.as_deref() == Some("dominant"))
                .count(),
            3
        );
        assert!(
            hits.iter().all(|h| h.snippet.contains('\u{1}')),
            "highlighted"
        );
    }

    /// The windowed rerun keeps the covering-index plan in both of its FTS scans.
    #[test]
    fn window_sql_uses_the_covering_index() {
        let mut conn = temp_db();
        thread_of(&mut conn, "t", &["covering index search"]);
        let filters = SearchFilters {
            sources: vec!["claude_code".into()],
            starred: Some(false),
            ..Default::default()
        };
        for f in [&SearchFilters::default(), &filters] {
            let (sql, args) = fts_window_sql(&conn, "\"covering\"*", f, 10);
            let plan = plan_of(&conn, &sql, &args);
            assert_eq!(
                plan.matches("SCAN messages_fts VIRTUAL TABLE").count(),
                2,
                "{plan}"
            );
            assert_eq!(
                plan.matches("USING INDEX idx_messages_cover (id=?)")
                    .count(),
                1,
                "{plan}"
            );
            assert_eq!(
                plan.matches("USING COVERING INDEX idx_messages_cover (id=?)")
                    .count(),
                1,
                "{plan}"
            );
            assert!(!plan.contains("SCAN m "), "{plan}");
        }
    }

    #[test]
    fn a_newer_search_interrupts_the_running_one() {
        let path = {
            let conn = temp_db();
            std::path::PathBuf::from(conn.path().unwrap())
        };
        let gens = SearchGen::default();
        let gen = gens.begin();
        let (registered, ready) = std::sync::mpsc::channel();
        std::thread::scope(|s| {
            let worker = s.spawn(|| {
                let conn = crate::db::open_readonly(&path).unwrap();
                let watch = gens.watch(gen, &conn).expect("still current");
                registered.send(()).unwrap();
                // Long enough to still be running when the newer search starts, and bounded:
                // if every interrupt were missed it ends with a count (a failure) instead of
                // hanging CI.
                let r: Result<i64> = conn
                    .query_row(
                        "WITH RECURSIVE n(x) AS (
                             SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 2000000000
                         )
                         SELECT count(*) FROM n",
                        [],
                        |r| r.get(0),
                    )
                    .map_err(Into::into);
                drop(watch);
                r
            });
            ready.recv().unwrap();
            // The worker signals before its statement starts stepping, and SQLite clears an
            // interrupt that lands before the first step, so keep starting newer searches
            // until it stops.
            let mut newer = gen;
            while !worker.is_finished() {
                newer = gens.begin();
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            let err = worker.join().unwrap().unwrap_err();
            assert!(is_interrupted(&err), "{err:#}");
            assert!(gens.is_current(newer) && !gens.is_current(gen));
        });
        // Superseded before it took a connection: never registered at all.
        let conn = crate::db::open_readonly(&path).unwrap();
        assert!(gens.watch(gen, &conn).is_none());
        assert!(
            gens.running().is_empty(),
            "registrations end with their guard"
        );
    }

    #[test]
    fn ui_search_reports_supersession() {
        let mut conn = temp_db();
        thread_of(&mut conn, "t", &["alpha beta"]);
        let pool = crate::db::read_pool(std::path::Path::new(conn.path().unwrap()), 2).unwrap();
        let gens = SearchGen::default();
        let embedder = Embedder::cached_only();
        let f = SearchFilters::default();
        let stale = gens.begin();
        let gen = gens.begin();
        let r = ui_search(&pool, &gens, gen, &embedder, "alpha", &f).unwrap();
        assert!(!r.superseded && r.semantic_skipped.is_none());
        assert_eq!(r.hits.len(), 1);
        let r = ui_search(&pool, &gens, stale, &embedder, "alpha", &f).unwrap();
        assert!(r.superseded && r.hits.is_empty());
    }

    /// The app's parallel hybrid path: the semantic arm runs on a second reader (its own
    /// connection when the pool is taken), and a skipped arm says why.
    #[test]
    fn ui_search_hybrid_runs_the_semantic_arm_and_says_why_it_skipped() {
        let mut conn = temp_db();
        let ids = semantic_corpus(
            &mut conn,
            &[("sem", "talk about databases", false, None, 1_000, 0.10)],
        );
        let pool = crate::db::read_pool(std::path::Path::new(conn.path().unwrap()), 2).unwrap();
        let gens = SearchGen::default();
        let f = SearchFilters {
            hybrid: true,
            ..Default::default()
        };
        // "zzqx" matches no keyword, so every hit comes from the semantic arm.
        let run = |stub: std::result::Result<Option<Vec<f32>>, embed::EmbedError>| {
            ui_search_with(&pool, &gens, gens.begin(), "zzqx", &f, |_| stub.clone()).unwrap()
        };
        let r = run(Ok(Some(direction(0.0))));
        assert_eq!(r.hits.iter().map(|h| h.thread_id).collect::<Vec<_>>(), ids);
        assert_eq!(r.semantic_skipped, None);
        {
            // The keyword arm takes the pool's other reader: the semantic arm opens its own.
            let _held = pool.0.get().unwrap();
            let r = run(Ok(Some(direction(0.0))));
            assert_eq!(r.hits.len(), 1, "fallback connection");
            assert_eq!(r.semantic_skipped, None);
        }
        let r = run(Err(embed::EmbedError::ModelLoading));
        assert!(r.hits.is_empty());
        assert_eq!(r.semantic_skipped, Some(SEMANTIC_LOADING));
        let r = run(Err(embed::EmbedError::ModelNotCached(Vec::new())));
        assert_eq!(r.semantic_skipped, Some(SEMANTIC_UNAVAILABLE));
        let r = run(Ok(None));
        assert_eq!(r.semantic_skipped, None, "an empty query skips nothing");
    }

    /// Related context (a code selection, an error) scores lower against prose than a
    /// search-box query: a 0.65 match is below the hybrid floor but still related.
    #[test]
    fn related_keeps_a_match_below_the_hybrid_floor() {
        let mut conn = temp_db();
        // direction(x) is at cosine 1/sqrt(1 + x^2) from direction(0.0).
        let at = |cos: f32| (1.0 / (cos * cos) - 1.0).sqrt();
        let ids = semantic_corpus(
            &mut conn,
            &[
                ("near", "talk about databases", false, None, 1_000, at(0.65)),
                ("far", "talk about gardening", false, None, 1_000, at(0.55)),
            ],
        );
        let q = direction(0.0);
        let f = SearchFilters::default();
        assert!(
            semantic_arm(&conn, &q, &f).unwrap().is_empty(),
            "hybrid floor"
        );
        let related: Vec<i64> = related_vec(&conn, &q, &f)
            .unwrap()
            .iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(related, vec![ids[0]]);
    }

    #[test]
    fn thread_viewer_gets_tool_previews_and_the_full_text_on_demand() {
        let mut conn = temp_db();
        let long = format!("{}{}", "é".repeat(TOOL_PREVIEW_CHARS), "😀 tail");
        thread_with(
            &mut conn,
            "t",
            vec![
                msg("user", &"prose ".repeat(100), 1),
                msg("tool", &long, 2),
                ParsedMessage {
                    role: "assistant".into(),
                    text: format!("Bash: {}", "x".repeat(400)),
                    tool_name: Some("Bash".into()),
                    ts: Some(3),
                },
                msg("tool", "short output", 4),
            ],
        );
        let tid: i64 = conn
            .query_row("SELECT id FROM threads", [], |r| r.get(0))
            .unwrap();
        let view = thread_detail_preview(&conn, tid).unwrap().unwrap();
        let m = &view.messages;
        assert_eq!(m[0].text.len(), 600, "prose stays whole");
        assert_eq!(m[0].text_len, 600);
        assert_eq!(m[1].text, "é".repeat(TOOL_PREVIEW_CHARS));
        // UTF-16 units, as JavaScript counts: the emoji is two.
        assert_eq!(m[1].text_len, TOOL_PREVIEW_CHARS + 2 + 5);
        assert_eq!(m[2].text.chars().count(), TOOL_PREVIEW_CHARS);
        assert_eq!(m[2].text_len, 406);
        assert_eq!((m[3].text.as_str(), m[3].text_len), ("short output", 12));
        assert_eq!(
            message_text(&conn, m[1].id).unwrap().as_deref(),
            Some(long.as_str())
        );
        assert_eq!(message_text(&conn, -1).unwrap(), None);
        // Everything else (export, context packing, snapshots) still gets the whole text.
        let full = thread_detail(&conn, tid).unwrap().unwrap();
        assert_eq!(full.messages[1].text, long);
    }
}
