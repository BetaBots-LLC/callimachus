//! The memory-truth eval: a small committed coding-domain corpus run through the real recall
//! paths, so a change to search, recall, supersession or the HOOK renderer shows up as a
//! number instead of an impression.
//!
//! The corpus (`fixtures/memory_truth_v1.json`) holds eight synthetic projects of three or
//! four sessions each, with planted pairs: a proposal and its later correction (15 s -> 45 s), a
//! resolved error and its fix, a decision reversed two sessions later (bincode -> JSON), a
//! decision restated in different words, a same-topic but different decision, and an unrelated
//! one. Each session carries a hand-written distill output with its links, stored through
//! `knowledge::store_distilled`: the eval measures retrieval and rendering, not the LLM.
//!
//! Three tests:
//! - `corpus_builds_with_the_planted_links` (default CI, no model): the corpus parses, every
//!   reference resolves, every planted supersede and reaffirm is stored, and the HOOK block
//!   leaves the superseded facts out.
//! - `eval_memory_truth` (`#[ignore]`): embeds the corpus with the cached model and prints a
//!   markdown report (search hit@1/@5 and recall@5 per slice, check_decision status checks,
//!   latency), then asserts floors set from the first run.
//! - `calibrate_guard_close` (`#[ignore]`): the cosine distributions of the labelled
//!   proposal/fact pairs and the threshold that best separates a restatement from a
//!   same-topic decision, for `knowledge::GUARD_CLOSE`.
//!
//! The model is read from the usual cache roots (`FASTEMBED_CACHE_DIR` first); without it the
//! ignored tests print why and pass. They download it only with `CALLIMACHUS_EVAL_DOWNLOAD=1`.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use rusqlite::{params, Connection};
use serde::Deserialize;

use crate::agent::{DistillLink, Distilled};
use crate::embed::{self, Embedder};
use crate::export::{agent_memory_md_budgeted, MemoryBudget};
use crate::indexer::{source_id, upsert_thread, ParsedMessage, ParsedThread};
use crate::knowledge::truth::{distill_memory_context, FactStatus};
use crate::knowledge::{self, store_distilled, RecallHit};
use crate::search::{self, SearchFilters};

/// The corpus. Bump to a new file (v2) rather than editing this one, so numbers quoted in
/// comments (GUARD_CLOSE, the floors below) stay tied to the text they were measured on.
const CORPUS_V1: &str = include_str!("fixtures/memory_truth_v1.json");

#[derive(Debug, Deserialize)]
struct Corpus {
    version: String,
    projects: Vec<Project>,
    search: Vec<SearchCase>,
    guard: Vec<GuardCase>,
    pairs: Vec<Pair>,
}

#[derive(Debug, Deserialize)]
struct Project {
    path: String,
    sessions: Vec<Session>,
}

#[derive(Debug, Deserialize)]
struct Session {
    id: String,
    title: String,
    /// `YYYY-MM-DD`; the session starts at 09:00 UTC that day.
    date: String,
    /// `[role, text]`.
    messages: Vec<(String, String)>,
    distilled: DistilledCase,
}

#[derive(Debug, Deserialize)]
struct DistilledCase {
    summary: String,
    #[serde(default)]
    decisions: Vec<FactCase>,
    #[serde(default)]
    gotchas: Vec<FactCase>,
    #[serde(default)]
    supersedes: Vec<Supersede>,
    #[serde(default)]
    reaffirms: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct FactCase {
    id: String,
    text: String,
}

#[derive(Debug, Deserialize)]
struct Supersede {
    new: String,
    old: String,
}

#[derive(Debug, Deserialize)]
struct SearchCase {
    slice: String,
    query: String,
    /// Session ids that answer the query.
    expect: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct GuardCase {
    project: String,
    proposal: String,
    expect: Vec<GuardExpect>,
}

#[derive(Debug, Deserialize)]
struct GuardExpect {
    fact: String,
    status: String,
    #[serde(default)]
    replaced_by: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Pair {
    label: String,
    fact: String,
    proposal: String,
}

fn corpus() -> Corpus {
    serde_json::from_str(CORPUS_V1).expect("the eval corpus parses")
}

/// Unix seconds of 09:00 UTC on `date` (`YYYY-MM-DD`).
fn day_ts(date: &str) -> i64 {
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .unwrap_or_else(|e| panic!("bad session date {date}: {e}"))
        .and_hms_opt(9, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp()
}

/// A fresh, migrated temp index.
fn temp_db() -> Connection {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let p = std::env::temp_dir().join(format!(
        "calli_eval_{}_{}.db",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    for ext in ["db", "db-wal", "db-shm"] {
        let _ = std::fs::remove_file(p.with_extension(ext));
    }
    crate::db::open(&p).unwrap()
}

/// The corpus as stored: ids the measurements resolve corpus references through.
struct Built {
    /// Session id -> thread id.
    threads: HashMap<String, i64>,
    /// Fact id -> facts.id.
    facts: HashMap<String, i64>,
    /// Fact id -> its text.
    fact_text: HashMap<String, String>,
    /// Corpus project path -> the canonical key memory scopes on.
    project_keys: HashMap<String, String>,
    /// Fact ids a planted supersede replaced.
    superseded: HashSet<String>,
}

/// `text -> ref` from a distill memory block (`D2 (2026-10-01): text`).
fn refs_by_text(block: &str) -> HashMap<String, String> {
    block
        .lines()
        .filter_map(|l| {
            let (r, rest) = l.split_once(" (")?;
            let (_, text) = rest.split_once("): ")?;
            Some((text.to_string(), r.to_string()))
        })
        .collect()
}

/// Index every session, then distill each project's sessions oldest first the way the
/// callers do (memory context, then the output stored against its refs), so a later
/// session's links name the earlier facts by the refs the distiller would have seen.
fn build(conn: &mut Connection, c: &Corpus) -> Built {
    let sid = source_id(conn, "claude_code").unwrap();
    let mut b = Built {
        threads: HashMap::new(),
        facts: HashMap::new(),
        fact_text: HashMap::new(),
        project_keys: HashMap::new(),
        superseded: HashSet::new(),
    };
    for p in &c.projects {
        for s in &p.sessions {
            let start = day_ts(&s.date);
            let messages: Vec<ParsedMessage> = s
                .messages
                .iter()
                .enumerate()
                .map(|(i, (role, text))| ParsedMessage {
                    role: role.clone(),
                    text: text.clone(),
                    tool_name: None,
                    ts: Some(start + 60 * i as i64),
                })
                .collect();
            let thread = ParsedThread {
                external_id: s.id.clone(),
                title: Some(s.title.clone()),
                project_path: Some(p.path.clone()),
                created_at: Some(start),
                updated_at: Some(start + 60 * messages.len() as i64),
                messages,
                ..Default::default()
            };
            upsert_thread(conn, sid, &thread).unwrap();
            let tid: i64 = conn
                .query_row(
                    "SELECT id FROM threads WHERE external_id = ?1",
                    [&s.id],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(
                b.threads.insert(s.id.clone(), tid).is_none(),
                "duplicate session id {}",
                s.id
            );
            for f in s.distilled.decisions.iter().chain(&s.distilled.gotchas) {
                assert!(
                    b.fact_text.insert(f.id.clone(), f.text.clone()).is_none(),
                    "duplicate fact id {}",
                    f.id
                );
            }
        }
        let first = &p.sessions[0].id;
        let key: String = conn
            .query_row(
                "SELECT COALESCE(project_key, project_path) FROM threads WHERE id = ?1",
                [b.threads[first]],
                |r| r.get(0),
            )
            .unwrap();
        b.project_keys.insert(p.path.clone(), key);
    }

    // Distilled well after the last session, as a backlog is; one second apart so a
    // re-distill tiebreak never depends on insertion order.
    let mut now = day_ts("2026-10-01");
    for p in &c.projects {
        let mut sessions: Vec<&Session> = p.sessions.iter().collect();
        sessions.sort_by_key(|s| day_ts(&s.date));
        for s in sessions {
            let tid = b.threads[&s.id];
            let (block, refs) = distill_memory_context(conn, tid).unwrap();
            let listed = refs_by_text(&block);
            let d = &s.distilled;
            let own_ref = |id: &str| -> String {
                if let Some(i) = d.decisions.iter().position(|f| f.id == id) {
                    return format!("d{}", i + 1);
                }
                if let Some(i) = d.gotchas.iter().position(|f| f.id == id) {
                    return format!("g{}", i + 1);
                }
                panic!("{}: supersede names {id}, not one of its own facts", s.id);
            };
            let listed_ref = |id: &str| -> String {
                let text = &b.fact_text[id];
                listed.get(text).cloned().unwrap_or_else(|| {
                    panic!(
                        "{}: {id} is not in the memory block it was shown:\n{block}",
                        s.id
                    )
                })
            };
            let out = Distilled {
                summary: d.summary.clone(),
                decisions: d.decisions.iter().map(|f| f.text.clone()).collect(),
                gotchas: d.gotchas.iter().map(|f| f.text.clone()).collect(),
                links: d
                    .supersedes
                    .iter()
                    .map(|l| DistillLink {
                        from: own_ref(&l.new),
                        to: listed_ref(&l.old),
                        kind: "supersedes".into(),
                    })
                    .collect(),
                reaffirms: d.reaffirms.iter().map(|id| listed_ref(id)).collect(),
                retires: Vec::new(),
            };
            store_distilled(conn, tid, &out, &refs, now).unwrap();
            now += 1;
            for f in d.decisions.iter().chain(&d.gotchas) {
                let id: i64 = conn
                    .query_row(
                        "SELECT id FROM facts WHERE thread_id = ?1 AND text = ?2",
                        params![tid, f.text],
                        |r| r.get(0),
                    )
                    .unwrap_or_else(|e| panic!("{} was not stored: {e}", f.id));
                b.facts.insert(f.id.clone(), id);
            }
            b.superseded
                .extend(d.supersedes.iter().map(|l| l.old.clone()));
        }
    }
    b
}

/// Embed every message and fact of the built index.
fn embed_all(conn: &mut Connection, embedder: &Embedder) {
    while embed::embed_batch(conn, embedder, 256).unwrap() > 0 {}
    embed::embed_pending_facts_conn(conn, embedder).unwrap();
    let pending: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM facts WHERE embedded = 0 AND kind IN ('decision', 'gotcha')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pending, 0, "every fact has a vector");
}

/// The HOOK block for `project` as `cal hook` renders it at session start for Claude Code
/// (`cli_core::hook_output`, facts only: the digest is not memory).
fn hook_block(conn: &Connection, project: &str) -> String {
    let budget = MemoryBudget::HOOK;
    let per_kind = budget
        .fetch_limit()
        .max(budget.max_decisions)
        .max(budget.max_gotchas);
    let mem = knowledge::get_project_memory_facts_excluding(
        conn,
        project,
        per_kind,
        knowledge::harvest::EXTRACTOR_CLAUDE_MEMORY,
    )
    .unwrap();
    agent_memory_md_budgeted(project, &mem, None, &budget)
}

/// A memory line holds the fact's text followed by its provenance suffix (` (`).
fn block_has_fact(block: &str, text: &str) -> bool {
    block.contains(&format!("- {text} ("))
}

/// The model to evaluate with, or why there is none (the ignored tests then pass).
fn eval_embedder() -> Option<Embedder> {
    let download = std::env::var("CALLIMACHUS_EVAL_DOWNLOAD").is_ok_and(|v| v == "1");
    if download {
        // The app's embedder: downloads into the usual cache root when missing.
        return Some(Embedder::default());
    }
    if !embed::model_is_cached() {
        eprintln!(
            "SKIPPED: the embedding model is not cached. Point FASTEMBED_CACHE_DIR at a \
             models directory that holds it, or set CALLIMACHUS_EVAL_DOWNLOAD=1 to fetch it \
             (about 128 MB)."
        );
        return None;
    }
    Some(Embedder::cached_only())
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

/// Nearest-rank percentile of sorted `v` (`p` in 0..=100).
fn percentile<T: Copy>(sorted: &[T], p: usize) -> T {
    let i = ((sorted.len() - 1) * p + 50) / 100;
    sorted[i.min(sorted.len() - 1)]
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Search scores, summed over a slice.
#[derive(Default, Clone, Copy)]
struct SearchScore {
    n: usize,
    hit1: usize,
    hit5: usize,
    recall5: f64,
}

impl SearchScore {
    fn add(&mut self, ranked: &[i64], expect: &[i64]) {
        self.n += 1;
        if ranked.first().is_some_and(|t| expect.contains(t)) {
            self.hit1 += 1;
        }
        let top5 = &ranked[..ranked.len().min(5)];
        let found = expect.iter().filter(|t| top5.contains(t)).count();
        if found > 0 {
            self.hit5 += 1;
        }
        self.recall5 += found as f64 / expect.len() as f64;
    }

    fn hit1_rate(&self) -> f64 {
        self.hit1 as f64 / self.n.max(1) as f64
    }

    fn hit5_rate(&self) -> f64 {
        self.hit5 as f64 / self.n.max(1) as f64
    }

    fn recall5_rate(&self) -> f64 {
        self.recall5 / self.n.max(1) as f64
    }
}

/// Thread ids of `hits` in first-seen order: one entry per thread, as `search_threads`
/// groups them (`mcp_server::group_by_thread`).
fn ranked_threads(hits: &[search::SearchHit]) -> Vec<i64> {
    let mut out: Vec<i64> = Vec::new();
    for h in hits {
        if !out.contains(&h.thread_id) {
            out.push(h.thread_id);
        }
    }
    out
}

/// Thread ids of semantic `(message id, similarity)` hits, best first, one per thread.
fn message_threads(conn: &Connection, hits: &[(i64, f32)]) -> Vec<i64> {
    let mut out: Vec<i64> = Vec::new();
    for (mid, _) in hits {
        let tid: i64 = conn
            .query_row("SELECT thread_id FROM messages WHERE id = ?1", [mid], |r| {
                r.get(0)
            })
            .unwrap();
        if !out.contains(&tid) {
            out.push(tid);
        }
    }
    out
}

/// `search_threads`' filters: its default 10 threads at 3 hits each
/// (`SEARCH_DEFAULT * SEARCH_HITS_PER_THREAD` in mcp_server.rs), every project.
fn search_filters(hybrid: bool) -> SearchFilters {
    SearchFilters {
        hybrid,
        limit: Some(30),
        ..Default::default()
    }
}

/// What one check_decision call got right.
struct GuardOutcome {
    /// Every expected current fact returned with status `current`, none behind a superseded hit.
    current_first: bool,
    /// The first hit is an expected current fact.
    top1_current: bool,
    /// Expected superseded facts returned with the right replacement / all expected
    /// superseded facts.
    superseded_ok: usize,
    superseded_n: usize,
    /// Expected facts not returned, or returned with another status.
    problems: Vec<String>,
}

fn grade_guard(case: &GuardCase, b: &Built, hits: &[RecallHit]) -> GuardOutcome {
    let pos = |fact: &str| hits.iter().position(|h| h.id == b.facts[fact]);
    let first_superseded = hits
        .iter()
        .position(|h| h.status == FactStatus::Superseded)
        .unwrap_or(usize::MAX);
    let mut out = GuardOutcome {
        current_first: true,
        top1_current: false,
        superseded_ok: 0,
        superseded_n: 0,
        problems: Vec::new(),
    };
    for e in &case.expect {
        let Some(i) = pos(&e.fact) else {
            out.problems.push(format!("{} missing", e.fact));
            if e.status == "current" {
                out.current_first = false;
            } else {
                out.superseded_n += 1;
            }
            continue;
        };
        let h = &hits[i];
        if h.status.as_str() != e.status {
            out.problems.push(format!(
                "{} is {}, want {}",
                e.fact,
                h.status.as_str(),
                e.status
            ));
        }
        match e.status.as_str() {
            "current" => {
                // A wrong status fails the case outright: an expected-current fact that came
                // back superseded can be the first superseded hit itself (i ==
                // first_superseded), which the ordering check alone would pass.
                if h.status != FactStatus::Current || i > first_superseded {
                    out.current_first = false;
                }
                if i == 0 && h.status == FactStatus::Current {
                    out.top1_current = true;
                }
            }
            "superseded" => {
                out.superseded_n += 1;
                let want = e.replaced_by.as_deref().map(|r| b.facts[r]);
                let got = h.superseded_by.as_ref().map(|l| l.id);
                if h.status == FactStatus::Superseded && got.is_some() && got == want {
                    out.superseded_ok += 1;
                } else {
                    out.problems
                        .push(format!("{} replaced by {got:?}, want {want:?}", e.fact));
                }
            }
            other => panic!("unknown expected status {other}"),
        }
    }
    out
}

/// The eval's numbers, for the floors.
struct Report {
    hybrid: SearchScore,
    current_first: f64,
    superseded_labelled: f64,
    hook_leaks: usize,
}

fn run_eval(embedder: &Embedder) -> Report {
    let c = corpus();
    let mut conn = temp_db();
    let t0 = Instant::now();
    let b = build(&mut conn, &c);
    embed_all(&mut conn, embedder);
    let build_time = t0.elapsed();

    // Warm the model so the first timed query does not pay its load.
    embed::embed_query(embedder, "warm up the query path").unwrap();

    let mut md = String::new();
    let messages: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    md.push_str(&format!(
        "## Memory truth eval ({})\n\n{} projects, {} sessions, {} messages, {} facts; built and embedded in {:.1} s.\n\n",
        c.version,
        c.projects.len(),
        b.threads.len(),
        messages,
        b.facts.len(),
        build_time.as_secs_f64()
    ));

    // search_threads: hybrid as the tool runs it; each arm alone as a baseline, so a
    // regression can be traced to the arm (or the fusion) that caused it.
    let mut slices: Vec<String> = Vec::new();
    for s in &c.search {
        if !slices.contains(&s.slice) {
            slices.push(s.slice.clone());
        }
    }
    let mut hybrid: HashMap<String, SearchScore> = HashMap::new();
    let mut keyword: HashMap<String, SearchScore> = HashMap::new();
    let mut semantic: HashMap<String, SearchScore> = HashMap::new();
    let (mut all_h, mut all_k, mut all_s) = (
        SearchScore::default(),
        SearchScore::default(),
        SearchScore::default(),
    );
    let mut search_lat: Vec<Duration> = Vec::new();
    let mut misses: Vec<String> = Vec::new();
    for s in &c.search {
        let expect: Vec<i64> = s.expect.iter().map(|e| b.threads[e]).collect();
        let t = Instant::now();
        let hits = search::hybrid(&conn, embedder, &s.query, &search_filters(true)).unwrap();
        search_lat.push(t.elapsed());
        let ranked = ranked_threads(&hits);
        hybrid
            .entry(s.slice.clone())
            .or_default()
            .add(&ranked, &expect);
        all_h.add(&ranked, &expect);
        if !ranked.first().is_some_and(|t| expect.contains(t)) {
            let by_id: HashMap<i64, &str> =
                b.threads.iter().map(|(k, v)| (*v, k.as_str())).collect();
            let top: Vec<&str> = ranked.iter().take(3).map(|t| by_id[t]).collect();
            misses.push(format!(
                "- [{}] \"{}\": want {:?}, top 3 {:?}",
                s.slice, s.query, s.expect, top
            ));
        }
        let kw = search::search(&conn, &s.query, &search_filters(false)).unwrap();
        let ranked = ranked_threads(&kw);
        keyword
            .entry(s.slice.clone())
            .or_default()
            .add(&ranked, &expect);
        all_k.add(&ranked, &expect);
        // The semantic arm alone, with its similarity floor (embed::SEM_SIMILARITY_FLOOR).
        let sem = embed::semantic_search(&conn, embedder, &s.query, false, &[], None, 50).unwrap();
        let ranked = message_threads(&conn, &sem);
        semantic
            .entry(s.slice.clone())
            .or_default()
            .add(&ranked, &expect);
        all_s.add(&ranked, &expect);
    }
    md.push_str("### search_threads (hybrid; each arm alone in the last two columns)\n\n");
    md.push_str(
        "| slice | n | hit@1 | hit@5 | recall@5 | keyword hit@5 | semantic hit@5 |\n|---|---|---|---|---|---|---|\n",
    );
    for slice in slices.iter().map(String::as_str).chain(["all"]) {
        let (h, k, sm) = if slice == "all" {
            (all_h, all_k, all_s)
        } else {
            (hybrid[slice], keyword[slice], semantic[slice])
        };
        md.push_str(&format!(
            "| {slice} | {} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} |\n",
            h.n,
            h.hit1_rate(),
            h.hit5_rate(),
            h.recall5_rate(),
            k.hit5_rate(),
            sm.hit5_rate()
        ));
    }
    if !misses.is_empty() {
        md.push_str("\nhit@1 misses:\n");
        md.push_str(&misses.join("\n"));
        md.push('\n');
    }

    // check_decision: the MCP tool's path (embed the proposal, then the guard).
    let mut guard_lat: Vec<Duration> = Vec::new();
    let (mut cf, mut top1, mut sup_ok, mut sup_n) = (0usize, 0usize, 0usize, 0usize);
    let mut lines: Vec<String> = Vec::new();
    for g in &c.guard {
        let key = &b.project_keys[&g.project];
        let t = Instant::now();
        let qv = embed::embed_query(embedder, &g.proposal).unwrap().unwrap();
        // CHECK_DECISION_K in mcp_server.rs.
        let hits = knowledge::check_contradiction(&conn, &qv, Some(key), 8).unwrap();
        guard_lat.push(t.elapsed());
        let o = grade_guard(g, &b, &hits);
        cf += usize::from(o.current_first);
        top1 += usize::from(o.top1_current);
        sup_ok += o.superseded_ok;
        sup_n += o.superseded_n;
        let returned: Vec<String> = hits
            .iter()
            .map(|h| {
                let id = b
                    .facts
                    .iter()
                    .find(|(_, v)| **v == h.id)
                    .map_or("?", |(k, _)| k.as_str());
                let close = match h.match_strength {
                    Some(knowledge::GuardMatch::Close) => "close",
                    _ => "related",
                };
                format!("{id} {} {:.3} {close}", h.status.as_str(), h.similarity)
            })
            .collect();
        lines.push(format!(
            "- \"{}\": {}{}",
            g.proposal,
            returned.join(", "),
            if o.problems.is_empty() {
                String::new()
            } else {
                format!(" PROBLEMS: {}", o.problems.join("; "))
            }
        ));
    }
    let n_guard = c.guard.len();
    let current_first = cf as f64 / n_guard as f64;
    let superseded_labelled = sup_ok as f64 / sup_n.max(1) as f64;
    md.push_str(&format!(
        "\n### check_decision\n\n- current-first rate: {cf}/{n_guard} ({current_first:.2})\n\
         - top hit is the expected current decision: {top1}/{n_guard}\n\
         - superseded hits labelled with the right replacement: {sup_ok}/{sup_n}\n\nPer proposal (fact, status, similarity, match):\n{}\n",
        lines.join("\n")
    ));

    // HOOK block: no superseded fact, every planted current decision of the project.
    let mut hook_leaks = 0usize;
    let mut hook_notes: Vec<String> = Vec::new();
    for p in &c.projects {
        let block = hook_block(&conn, &b.project_keys[&p.path]);
        for s in &p.sessions {
            for f in s.distilled.decisions.iter().chain(&s.distilled.gotchas) {
                let shown = block_has_fact(&block, &f.text);
                if b.superseded.contains(&f.id) && shown {
                    hook_leaks += 1;
                    hook_notes.push(format!("{} (superseded) is in the HOOK block", f.id));
                } else if !b.superseded.contains(&f.id) && !shown {
                    hook_notes.push(format!("{} (current) is missing from the HOOK block", f.id));
                }
            }
        }
    }
    md.push_str(&format!(
        "\n### HOOK block\n\n- superseded facts shown: {hook_leaks} (of {} planted)\n{}\n",
        b.superseded.len(),
        hook_notes
            .iter()
            .map(|n| format!("- {n}"))
            .collect::<Vec<_>>()
            .join("\n")
    ));

    search_lat.sort();
    guard_lat.sort();
    md.push_str(&format!(
        "\n### Latency (warm model, {} search and {} guard calls)\n\n| path | p50 ms | p95 ms |\n|---|---|---|\n\
         | search_threads hybrid (embed + both arms) | {:.1} | {:.1} |\n\
         | check_decision (embed + guard) | {:.1} | {:.1} |\n",
        search_lat.len(),
        guard_lat.len(),
        ms(percentile(&search_lat, 50)),
        ms(percentile(&search_lat, 95)),
        ms(percentile(&guard_lat, 50)),
        ms(percentile(&guard_lat, 95)),
    ));
    println!("{md}");
    Report {
        hybrid: all_h,
        current_first,
        superseded_labelled,
        hook_leaks,
    }
}

/// One labelled pair's similarity, as check_contradiction computes it: the proposal embedded
/// as a query (with bge's query prefix), the stored fact as a passage.
struct Scored {
    label: String,
    fact: String,
    proposal: String,
    sim: f32,
}

fn score_pairs(embedder: &Embedder) -> Vec<Scored> {
    let c = corpus();
    let texts: HashMap<&str, &str> = c
        .projects
        .iter()
        .flat_map(|p| &p.sessions)
        .flat_map(|s| s.distilled.decisions.iter().chain(&s.distilled.gotchas))
        .map(|f| (f.id.as_str(), f.text.as_str()))
        .collect();
    let facts: Vec<String> = c
        .pairs
        .iter()
        .map(|p| texts[p.fact.as_str()].to_string())
        .collect();
    let passages = embedder.embed(facts).unwrap();
    c.pairs
        .iter()
        .zip(passages)
        .map(|(p, fv)| {
            let qv = embed::embed_query(embedder, &p.proposal).unwrap().unwrap();
            Scored {
                label: p.label.clone(),
                fact: p.fact.clone(),
                proposal: p.proposal.clone(),
                sim: cosine(&qv, &fv),
            }
        })
        .collect()
}

/// Balanced accuracy of `sim >= t` as "restatement" against "same_topic".
fn balanced_accuracy(pos: &[f32], neg: &[f32], t: f32) -> f64 {
    let tpr = pos.iter().filter(|s| **s >= t).count() as f64 / pos.len() as f64;
    let tnr = neg.iter().filter(|s| **s < t).count() as f64 / neg.len() as f64;
    (tpr + tnr) / 2.0
}

/// The threshold with the best balanced accuracy, and the gap it sits in. Every `t` between two
/// neighbouring observed similarities classifies the pairs the same way, so the candidates are
/// those gaps: the best ones that touch merge into one region, the widest such region wins,
/// and the threshold is its middle (as far from either class as the data allows). Returns
/// (threshold, balanced accuracy, region low, region high).
fn best_threshold(pos: &[f32], neg: &[f32]) -> (f32, f64, f32, f32) {
    let mut v: Vec<f32> = pos.iter().chain(neg).copied().collect();
    v.sort_by(f32::total_cmp);
    v.dedup();
    // Gap i is (v[i - 1], v[i]]: `sim >= v[i]` is its rule (the last gap reaches 1.0).
    let gaps: Vec<(f32, f32, f64)> = (0..=v.len())
        .map(|i| {
            let lo = if i == 0 { 0.0 } else { v[i - 1] };
            let hi = v.get(i).copied().unwrap_or(1.0);
            let t = v.get(i).copied().unwrap_or(f32::INFINITY);
            (lo, hi, balanced_accuracy(pos, neg, t))
        })
        .collect();
    let best = gaps.iter().map(|g| g.2).fold(f64::MIN, f64::max);
    let mut regions: Vec<(f32, f32)> = Vec::new();
    let mut prev_best = false;
    for g in &gaps {
        let is_best = (g.2 - best).abs() < 1e-12;
        match (is_best, prev_best, regions.last_mut()) {
            (true, true, Some(r)) => r.1 = g.1,
            (true, _, _) => regions.push((g.0, g.1)),
            _ => {}
        }
        prev_best = is_best;
    }
    let (lo, hi) = regions
        .into_iter()
        .max_by(|a, b| (a.1 - a.0).total_cmp(&(b.1 - b.0)))
        .expect("at least one gap");
    ((lo + hi) / 2.0, best, lo, hi)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs in CI with no model: the corpus parses, its references resolve, and the planted
    /// links come out of `store_distilled` as the eval assumes. A corpus edit that breaks a
    /// link fails here instead of skewing the ignored eval.
    #[test]
    fn corpus_builds_with_the_planted_links() {
        let c = corpus();
        let mut conn = temp_db();
        let b = build(&mut conn, &c);
        assert!((6..=10).contains(&c.projects.len()));

        for s in &c.search {
            for e in &s.expect {
                assert!(b.threads.contains_key(e), "search expects unknown {e}");
            }
        }
        for g in &c.guard {
            assert!(b.project_keys.contains_key(&g.project), "{}", g.project);
            for e in &g.expect {
                assert!(
                    b.facts.contains_key(&e.fact),
                    "guard expects unknown {}",
                    e.fact
                );
                if let Some(r) = &e.replaced_by {
                    assert!(b.facts.contains_key(r), "unknown replacement {r}");
                }
            }
        }
        for p in &c.pairs {
            assert!(
                b.facts.contains_key(&p.fact),
                "pair names unknown {}",
                p.fact
            );
        }
        for label in ["restatement", "same_topic", "unrelated"] {
            assert!(c.pairs.iter().any(|p| p.label == label), "no {label} pairs");
        }

        // Every planted supersede holds, naming its replacement; every reaffirm counts.
        let truth_of = |id: i64| {
            let uid = crate::knowledge::truth::uid_of(&conn, id).unwrap().unwrap();
            crate::knowledge::truth::fact_truth(&conn, &[&uid])
                .unwrap()
                .remove(&uid)
                .unwrap()
        };
        for p in &c.projects {
            for s in &p.sessions {
                for l in &s.distilled.supersedes {
                    let t = truth_of(b.facts[&l.old]);
                    assert_eq!(t.status, FactStatus::Superseded, "{} in {}", l.old, s.id);
                    assert_eq!(t.superseded_by.map(|f| f.id), Some(b.facts[&l.new]));
                }
                for r in &s.distilled.reaffirms {
                    assert_eq!(
                        truth_of(b.facts[r]).support,
                        2,
                        "{r} reaffirmed in {}",
                        s.id
                    );
                }
            }
        }

        // The HOOK block leaves every superseded fact out and keeps every other one (the
        // corpus is far under its budget).
        for p in &c.projects {
            let block = hook_block(&conn, &b.project_keys[&p.path]);
            for s in &p.sessions {
                for f in s.distilled.decisions.iter().chain(&s.distilled.gotchas) {
                    assert_eq!(
                        block_has_fact(&block, &f.text),
                        !b.superseded.contains(&f.id),
                        "{} in the HOOK block of {}:\n{block}",
                        f.id,
                        p.path
                    );
                }
            }
        }
    }

    #[test]
    fn threshold_search_takes_the_middle_of_the_best_gap() {
        let (t, ba, lo, hi) = best_threshold(&[0.9, 0.85], &[0.6, 0.7]);
        assert_eq!(ba, 1.0);
        assert_eq!((lo, hi), (0.7, 0.85));
        assert!(t > 0.7 && t <= 0.85, "{t}");
        assert_eq!(percentile(&[1, 2, 3, 4], 50), 3);
        assert_eq!(percentile(&[1, 2, 3, 4], 95), 4);
    }

    /// The guard grader fails an expected-current fact that comes back with another status,
    /// even when it is the only superseded hit (so no current fact sits behind one).
    #[test]
    fn guard_grader_fails_a_wrong_status() {
        let b = Built {
            threads: HashMap::new(),
            facts: HashMap::from([("C1".to_string(), 1)]),
            fact_text: HashMap::new(),
            project_keys: HashMap::new(),
            superseded: HashSet::new(),
        };
        let case = GuardCase {
            project: "p".into(),
            proposal: "q".into(),
            expect: vec![GuardExpect {
                fact: "C1".into(),
                status: "current".into(),
                replaced_by: None,
            }],
        };
        let hit = |status| RecallHit {
            id: 1,
            thread_id: 1,
            kind: "decision".into(),
            text: String::new(),
            source: String::new(),
            title: None,
            project_path: None,
            rationale: None,
            similarity: 0.8,
            extractor: "llm".into(),
            origin_ref: None,
            decided_at: 0,
            status,
            superseded_by: None,
            disputed_by: Vec::new(),
            replaces: Vec::new(),
            support: 1,
            last_confirmed: None,
            match_strength: None,
        };
        let ok = grade_guard(&case, &b, &[hit(FactStatus::Current)]);
        assert!(ok.current_first && ok.top1_current && ok.problems.is_empty());
        let bad = grade_guard(&case, &b, &[hit(FactStatus::Superseded)]);
        assert!(
            !bad.current_first && !bad.top1_current,
            "{:?}",
            bad.problems
        );
    }

    /// The eval. Run on demand (see CONTRIBUTING.md):
    /// `FASTEMBED_CACHE_DIR=<models dir> cargo test -p callimachus-core eval_memory_truth -- --ignored --nocapture`
    #[test]
    #[ignore = "needs the embedding model; run on demand"]
    fn eval_memory_truth() {
        let Some(embedder) = eval_embedder() else {
            return;
        };
        let r = run_eval(&embedder);
        // Floors from the first run on memory_truth_v1 (2026-11-09, bge-small-en-v1.5, two
        // identical runs): hybrid hit@1 0.89 (32/36), hit@5 0.94 (34/36), recall@5 0.93;
        // current-first 9/9; superseded labelled 6/6; 0 HOOK leaks. The corpus and the model
        // are fixed, so a rerun differs only by ONNX float noise (about 1e-6 in a cosine),
        // which can flip a near tie: the search floors sit one query (1/36 = 0.028) below
        // the measured rate, so a flip passes and a real regression of two queries fails.
        // The status checks are exact, as they do not depend on ranking.
        assert!(
            r.hybrid.hit1_rate() >= 0.86,
            "hybrid hit@1 {:.2}",
            r.hybrid.hit1_rate()
        );
        assert!(
            r.hybrid.hit5_rate() >= 0.91,
            "hybrid hit@5 {:.2}",
            r.hybrid.hit5_rate()
        );
        assert!(
            r.hybrid.recall5_rate() >= 0.90,
            "recall@5 {:.2}",
            r.hybrid.recall5_rate()
        );
        assert_eq!(r.current_first, 1.0, "current-first rate");
        assert_eq!(r.superseded_labelled, 1.0, "superseded labelling");
        assert_eq!(r.hook_leaks, 0, "superseded facts in the HOOK block");
    }

    /// Calibrates `knowledge::GUARD_CLOSE`. Run on demand:
    /// `FASTEMBED_CACHE_DIR=<models dir> cargo test -p callimachus-core calibrate_guard_close -- --ignored --nocapture`
    #[test]
    #[ignore = "needs the embedding model; run on demand"]
    fn calibrate_guard_close() {
        let Some(embedder) = eval_embedder() else {
            return;
        };
        let scored = score_pairs(&embedder);
        let of = |label: &str| -> Vec<f32> {
            let mut v: Vec<f32> = scored
                .iter()
                .filter(|s| s.label == label)
                .map(|s| s.sim)
                .collect();
            v.sort_by(f32::total_cmp);
            v
        };
        let mut md = format!(
            "## GUARD_CLOSE calibration ({})\n\nCosine of the proposal (query-embedded) against the stored fact (passage), as check_decision scores it.\n\n\
             | label | n | min | p25 | median | p75 | max | mean |\n|---|---|---|---|---|---|---|---|\n",
            corpus().version
        );
        for label in ["restatement", "same_topic", "unrelated", "reversal"] {
            let v = of(label);
            if v.is_empty() {
                continue;
            }
            let mean = v.iter().sum::<f32>() / v.len() as f32;
            md.push_str(&format!(
                "| {label} | {} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} | {mean:.3} |\n",
                v.len(),
                v[0],
                percentile(&v, 25),
                percentile(&v, 50),
                percentile(&v, 75),
                v[v.len() - 1]
            ));
        }
        let (pos, neg) = (of("restatement"), of("same_topic"));
        let (t, ba, lo, hi) = best_threshold(&pos, &neg);
        let current = knowledge::GUARD_CLOSE;
        md.push_str(&format!(
            "\nBest restatement vs same_topic threshold: {t:.3} (balanced accuracy {ba:.3}, \
             in the gap {lo:.3}..{hi:.3}, width {:.3}); GUARD_CLOSE {current:.3} scores {:.3}.\n",
            hi - lo,
            balanced_accuracy(&pos, &neg, current)
        ));
        // Who GUARD_CLOSE calls `close`: restatements and reversals should be, same-topic
        // decisions should not.
        for label in ["restatement", "reversal", "same_topic", "unrelated"] {
            let v = of(label);
            let close = v.iter().filter(|s| **s >= current).count();
            md.push_str(&format!(
                "- GUARD_CLOSE {current} calls {close}/{} {label} pairs close\n",
                v.len()
            ));
        }
        // check_contradiction's floor: a same-topic decision under it is never shown.
        const GUARD_FLOOR: f32 = knowledge::GUARD_FLOOR;
        for label in ["restatement", "same_topic", "reversal", "unrelated"] {
            let v = of(label);
            let through = v.iter().filter(|s| **s >= GUARD_FLOOR).count();
            md.push_str(&format!(
                "- GUARD_FLOOR {GUARD_FLOOR} lets {through}/{} {label} pairs through\n",
                v.len()
            ));
        }
        md.push_str("\nPairs by similarity:\n");
        let mut sorted: Vec<&Scored> = scored.iter().collect();
        sorted.sort_by(|a, b| b.sim.total_cmp(&a.sim));
        for s in sorted {
            md.push_str(&format!(
                "- {:.3} {} {}: \"{}\"\n",
                s.sim, s.label, s.fact, s.proposal
            ));
        }
        println!("{md}");
    }
}
