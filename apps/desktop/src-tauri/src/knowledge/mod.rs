//! Distilled knowledge layer. Slice 1 is the FREE heuristic tier: TODO/action items
//! pulled out of message text with conservative, low-noise markers (markdown unchecked
//! tasks + word-boundaried TODO/FIXME), stored in the `facts` table by the indexer and
//! surfaced via `list_open_todos` (desktop, `cal todos`, MCP). No model, no API key.
//! The LLM tier (decisions / gotchas / summaries, lazy on-demand) reuses the same table.

use crate::agent::Distilled;
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

/// Hard cap on a single extracted TODO so one runaway line can't bloat the list.
const MAX_TODO_LEN: usize = 240;

/// Pull likely TODO/action items out of one message's text. Intentionally
/// conservative — only markdown unchecked tasks (`- [ ]`) and word-boundaried
/// `TODO`/`FIXME` markers — so we get signal, not every "we need to" aside.
pub fn extract_todos(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let candidate = match unchecked_task(line) {
            Some(rest) => Some(rest),
            None => marker_pos(line).map(|i| strip_marker(&line[i..])),
        };
        if let Some(c) = candidate {
            if let Some(todo) = clean(c) {
                out.push(todo);
            }
        }
    }
    out
}

/// Text after a markdown unchecked task bullet, e.g. `- [ ] wire up auth` -> `wire up auth`.
fn unchecked_task(line: &str) -> Option<&str> {
    for p in ["- [ ]", "* [ ]", "- [] ", "* [] "] {
        if let Some(rest) = line.strip_prefix(p) {
            return Some(rest);
        }
    }
    None
}

/// Byte index of a word-boundaried, case-insensitive TODO/FIXME marker (so "mastodon"
/// and "fixmestyle" don't match). None if the line has no marker.
fn marker_pos(line: &str) -> Option<usize> {
    let lower = line.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    for m in ["todo", "fixme"] {
        let mut start = 0;
        while let Some(rel) = lower[start..].find(m) {
            let i = start + rel;
            let before_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
            let after_idx = i + m.len();
            let after_ok = after_idx >= bytes.len() || !bytes[after_idx].is_ascii_alphanumeric();
            if before_ok && after_ok {
                return Some(i);
            }
            start = i + m.len();
        }
    }
    None
}

/// Drop the leading `TODO`/`FIXME` label and its trailing punctuation:
/// `TODO: fix the parser` -> `fix the parser`.
fn strip_marker(frag: &str) -> &str {
    let lower = frag.to_ascii_lowercase();
    let rest = if lower.starts_with("todo") {
        &frag[4..]
    } else if lower.starts_with("fixme") {
        &frag[5..]
    } else {
        frag
    };
    rest.trim_start_matches([':', '-', ' ', '\t', ')', '(', '.', '*'])
}

/// Trim, reject noise/too-short fragments, and length-cap. None = not a usable TODO.
fn clean(s: &str) -> Option<String> {
    let t = s.trim();
    if t.chars().count() < 4 {
        return None;
    }
    // Reject code / JSON / markdown-table / escaped-newline blobs. Some transcripts
    // store command output or whole tables on a single line (literal "\n"), which
    // would otherwise yield a garbage "todo". Prefer precision over recall here.
    if is_noise(t) {
        return None;
    }
    if t.chars().count() > MAX_TODO_LEN {
        let mut capped: String = t.chars().take(MAX_TODO_LEN).collect();
        capped.push('…');
        return Some(capped);
    }
    Some(t.to_string())
}

/// Heuristic junk filter: literal escaped newlines, table pipes, or JSON/code braces
/// mean this "line" is really structured output, not a human action item.
fn is_noise(t: &str) -> bool {
    t.contains("\\n")
        || t.contains('|')
        || t.contains("\",\"")
        || t.contains('{')
        || t.contains('}')
}

/// Cap on heuristic todos kept per thread (matches the indexer).
const MAX_TODOS_PER_THREAD: usize = 25;

/// Wipe all heuristic todos (used when the user turns the knowledge feature off).
pub fn clear_heuristic(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM facts WHERE extractor = 'heuristic'", [])?;
    Ok(())
}

/// Re-derive a single thread's heuristic todos from its already-stored messages,
/// inside the caller's transaction. Shared by the indexer and the backfill.
fn rebuild_thread_todos(tx: &Connection, thread_id: i64, now: i64) -> Result<()> {
    // Keep CURATED todos (done / dismissed / pinned) so closing a TODO survives re-index;
    // only the open, untouched ones are re-derived.
    tx.execute(
        "DELETE FROM facts WHERE thread_id = ?1 AND extractor = 'heuristic'
            AND status = 'open' AND hidden = 0 AND pinned = 0",
        [thread_id],
    )?;
    let mut sel = tx.prepare(
        "SELECT id, text FROM messages WHERE thread_id = ?1 AND role IN ('user', 'assistant')
         ORDER BY seq, id",
    )?;
    let rows: Vec<(i64, String)> = sel
        .query_map([thread_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut ins = tx.prepare(
        "INSERT INTO facts (thread_id, kind, text, source_message_id, status, extractor, created_at)
         VALUES (?1, 'todo', ?2, ?3, 'open', 'heuristic', ?4)",
    )?;
    let mut seen = std::collections::HashSet::new();
    // Seed with kept curated todos so we don't insert an open duplicate of a closed one.
    {
        let mut kept =
            tx.prepare("SELECT text FROM facts WHERE thread_id = ?1 AND extractor = 'heuristic'")?;
        for t in kept
            .query_map([thread_id], |r| r.get::<_, String>(0))?
            .flatten()
        {
            seen.insert(t.to_ascii_lowercase());
        }
    }
    let mut per = 0usize;
    'outer: for (mid, text) in rows {
        if per >= MAX_TODOS_PER_THREAD {
            break;
        }
        for todo in extract_todos(&text) {
            if per >= MAX_TODOS_PER_THREAD {
                break 'outer;
            }
            if seen.insert(todo.to_ascii_lowercase()) {
                ins.execute(rusqlite::params![thread_id, todo, mid, now])?;
                per += 1;
            }
        }
    }
    Ok(())
}

/// Backfill heuristic todos across the whole corpus from already-indexed message text
/// (no file reading). Used when the user opts INTO the feature so todos appear without
/// a full re-index. Runs in BATCHES, taking the DB lock briefly per chunk of threads so
/// the UI stays responsive — never one long lock hold. Safe to run on a background thread.
pub fn backfill_todos(db: &crate::db::Db, now: i64) -> Result<()> {
    const BATCH: usize = 50;
    let lock = || db.0.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"));

    // Clear stale heuristic facts up front (one short lock).
    lock()?.execute("DELETE FROM facts WHERE extractor = 'heuristic'", [])?;

    let thread_ids: Vec<i64> = {
        let conn = lock()?;
        let mut stmt = conn.prepare("SELECT id FROM threads ORDER BY id")?;
        let ids: Vec<i64> = stmt
            .query_map([], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<_>>()?;
        ids
    };

    for chunk in thread_ids.chunks(BATCH) {
        let mut conn = lock()?;
        // The user may have toggled the feature OFF mid-backfill (which cleared the
        // facts in the gap between batches). Re-check under the lock and stop, so we
        // don't re-insert todos the user just turned off.
        if !get_config(&conn)?.enabled {
            return Ok(());
        }
        let tx = conn.transaction()?;
        for &tid in chunk {
            rebuild_thread_todos(&tx, tid, now)?;
        }
        tx.commit()?;
    }
    Ok(())
}

/// An open TODO surfaced to the UI / agents, with the thread it came from.
#[derive(Debug, Serialize)]
pub struct TodoFact {
    pub id: i64,
    #[serde(rename = "threadId")]
    pub thread_id: i64,
    pub text: String,
    pub source: String,
    pub title: Option<String>,
    #[serde(rename = "projectPath")]
    pub project_path: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
}

/// Open TODOs across the corpus, newest first. Optionally scoped to a project-path
/// substring and/or a source kind. Plain SQL — no embedding, works with zero LLM use.
pub fn list_open_todos(
    conn: &Connection,
    query: Option<&str>,
    project: Option<&str>,
    source: Option<&str>,
    limit: i64,
) -> Result<Vec<TodoFact>> {
    let mut sql = String::from(
        "SELECT f.id, f.thread_id, f.text, s.kind, t.title, t.project_path, f.created_at
         FROM facts f
         JOIN threads t ON t.id = f.thread_id
         JOIN sources s ON s.id = t.source_id
         WHERE f.kind = 'todo' AND f.status = 'open' AND f.hidden = 0",
    );
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    // Server-side text search (over the whole corpus, not just the loaded page) so it
    // scales past the page limit for users with thousands of todos.
    if let Some(qy) = query.map(str::trim).filter(|q| !q.is_empty()) {
        let like = format!("%{qy}%");
        args.push(Box::new(like.clone()));
        let a = args.len();
        args.push(Box::new(like));
        let b = args.len();
        sql.push_str(&format!(" AND (f.text LIKE ?{a} OR t.title LIKE ?{b})"));
    }
    if let Some(p) = project.filter(|p| !p.is_empty()) {
        args.push(Box::new(format!("%{p}%")));
        sql.push_str(&format!(" AND t.project_path LIKE ?{}", args.len()));
    }
    if let Some(src) = source.filter(|s| !s.is_empty()) {
        args.push(Box::new(src.to_string()));
        sql.push_str(&format!(" AND s.kind = ?{}", args.len()));
    }
    args.push(Box::new(limit));
    sql.push_str(&format!(
        " ORDER BY f.created_at DESC, f.id DESC LIMIT ?{}",
        args.len()
    ));

    let arg_refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(arg_refs.as_slice(), |r| {
        Ok(TodoFact {
            id: r.get(0)?,
            thread_id: r.get(1)?,
            text: r.get(2)?,
            source: r.get(3)?,
            title: r.get(4)?,
            project_path: r.get(5)?,
            created_at: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// ---------------------------------------------------------------------------
// LLM distillation tier (opt-in). Decisions / gotchas / summary per thread.
// ---------------------------------------------------------------------------

/// Distillation engine config, shared across the app / cal / MCP via `app_config`.
/// `enabled` is the consent flag — nothing distills until the user turns it on.
#[derive(Debug, Serialize)]
pub struct KnowledgeConfig {
    pub enabled: bool,
    pub provider: Option<String>, // None = first available cloud key
    pub model: Option<String>,
    /// Auto-distill new/changed threads in the background (opt-in; uses the engine).
    #[serde(rename = "autoDistill")]
    pub auto_distill: bool,
}

fn config_get(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM app_config WHERE key = ?1", [key], |r| {
            r.get::<_, String>(0)
        })
        .optional()?)
}

/// Read the distillation config (defaults to disabled / unset).
pub fn get_config(conn: &Connection) -> Result<KnowledgeConfig> {
    Ok(KnowledgeConfig {
        enabled: config_get(conn, "knowledge.enabled")?.as_deref() == Some("1"),
        provider: config_get(conn, "knowledge.provider")?.filter(|s| !s.is_empty()),
        model: config_get(conn, "knowledge.model")?.filter(|s| !s.is_empty()),
        auto_distill: config_get(conn, "knowledge.auto_distill")?.as_deref() == Some("1"),
    })
}

/// Toggle background auto-distillation (separate from `set_config` so the consent flag
/// and engine choice aren't disturbed). Only meaningful when distillation is enabled.
pub fn set_auto_distill(conn: &Connection, on: bool) -> Result<()> {
    conn.execute(
        "INSERT INTO app_config (key, value) VALUES ('knowledge.auto_distill', ?1)
         ON CONFLICT(key) DO UPDATE SET value = ?1",
        params![if on { "1" } else { "0" }],
    )?;
    Ok(())
}

/// Persist the distillation config. Enabling it is the user's consent to send thread
/// text to the chosen engine (cloud key) — or to keep it local (Ollama).
pub fn set_config(
    conn: &Connection,
    enabled: bool,
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<()> {
    for (k, v) in [
        ("knowledge.enabled", if enabled { "1" } else { "0" }),
        ("knowledge.provider", provider.unwrap_or("")),
        ("knowledge.model", model.unwrap_or("")),
    ] {
        conn.execute(
            "INSERT INTO app_config (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = ?2",
            params![k, v],
        )?;
    }
    Ok(())
}

/// Replace a thread's LLM-distilled facts with a fresh set and mark it extracted at
/// the current message count. Heuristic todos (extractor='heuristic') are untouched.
pub fn store_distilled(
    conn: &mut Connection,
    thread_id: i64,
    d: &Distilled,
    now: i64,
) -> Result<()> {
    let tx = conn.transaction()?;
    // Replace only the UN-curated LLM facts; pinned / edited / hidden ones are the user's
    // now and survive re-distillation.
    tx.execute(
        "DELETE FROM facts WHERE thread_id = ?1 AND extractor = 'llm'
            AND pinned = 0 AND edited = 0 AND hidden = 0",
        [thread_id],
    )?;
    {
        let mut ins = tx.prepare(
            "INSERT INTO facts (thread_id, kind, text, status, extractor, seq, created_at)
             VALUES (?1, ?2, ?3, 'open', 'llm', ?4, ?5)",
        )?;
        let summary = d.summary.trim();
        if !summary.is_empty() {
            ins.execute(params![thread_id, "summary", summary, 0_i64, now])?;
        }
        for (i, text) in d.decisions.iter().enumerate() {
            let t = text.trim();
            if !t.is_empty() {
                ins.execute(params![thread_id, "decision", t, i as i64, now])?;
            }
        }
        for (i, text) in d.gotchas.iter().enumerate() {
            let t = text.trim();
            if !t.is_empty() {
                ins.execute(params![thread_id, "gotcha", t, i as i64, now])?;
            }
        }
    }
    tx.execute(
        "UPDATE threads SET knowledge_extracted = 1, knowledge_extracted_at = ?2,
            knowledge_msg_count = message_count, knowledge_error = NULL WHERE id = ?1",
        params![thread_id, now],
    )?;
    tx.commit()?;
    Ok(())
}

/// Record a distillation failure so the UI can show it and lazy-on-open won't retry it
/// in a loop (a manual re-distill clears it). Leaves knowledge_extracted = 0.
pub fn set_error(conn: &Connection, thread_id: i64, err: &str) -> Result<()> {
    conn.execute(
        "UPDATE threads SET knowledge_error = ?2 WHERE id = ?1",
        params![thread_id, err],
    )?;
    Ok(())
}

// ---- fact curation (pin / edit / hide) — makes auto-generated memory trustworthy ----

/// Pin or unpin a fact. Pinned facts rank first and survive re-distillation.
pub fn set_fact_pinned(conn: &Connection, fact_id: i64, pinned: bool) -> Result<()> {
    conn.execute(
        "UPDATE facts SET pinned = ?2 WHERE id = ?1",
        params![fact_id, i64::from(pinned)],
    )?;
    Ok(())
}

/// Hide or unhide a fact (soft delete). Hidden facts are never shown but kept as a
/// tombstone so re-distillation's DELETE skips them (they won't be resurrected).
pub fn set_fact_hidden(conn: &Connection, fact_id: i64, hidden: bool) -> Result<()> {
    conn.execute(
        "UPDATE facts SET hidden = ?2 WHERE id = ?1",
        params![fact_id, i64::from(hidden)],
    )?;
    Ok(())
}

/// Mark a TODO done (or reopen it). Done todos drop out of the open lists but persist
/// across re-index (the heuristic re-derive keeps curated/closed rows).
pub fn set_todo_done(conn: &Connection, fact_id: i64, done: bool) -> Result<()> {
    conn.execute(
        "UPDATE facts SET status = ?2 WHERE id = ?1 AND kind = 'todo'",
        params![fact_id, if done { "done" } else { "open" }],
    )?;
    Ok(())
}

/// Edit a fact's text. Marks it edited (survives re-distill) and re-queues it for
/// embedding (drops the stale vector) so cross-thread recall matches the new wording.
pub fn edit_fact(conn: &Connection, fact_id: i64, text: &str) -> Result<()> {
    conn.execute(
        "UPDATE facts SET text = ?2, edited = 1, embedded = 0 WHERE id = ?1",
        params![fact_id, text.trim()],
    )?;
    conn.execute("DELETE FROM vec_facts WHERE fact_id = ?1", [fact_id])?;
    Ok(())
}

/// Clear extraction state so a thread re-distills on next request (the "Re-distill" path).
pub fn mark_for_redistill(conn: &Connection, thread_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE threads SET knowledge_extracted = 0, knowledge_error = NULL WHERE id = ?1",
        [thread_id],
    )?;
    Ok(())
}

/// A single distilled fact for the thread view.
#[derive(Debug, Serialize)]
pub struct KFact {
    pub id: i64,
    pub text: String,
    pub pinned: bool,
}

/// All distilled knowledge for one thread, grouped by kind, with freshness flags.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadKnowledge {
    pub summary: Option<String>,
    pub decisions: Vec<KFact>,
    pub gotchas: Vec<KFact>,
    pub todos: Vec<KFact>,
    pub extracted: bool,
    pub stale: bool,
    pub error: Option<String>,
    pub can_distill: bool,
}

/// Whether a thread should be (re)distilled now: enabled, not already done, no prior
/// error (so we don't loop on a failing API), and either never-done or stale.
pub fn needs_distill(conn: &Connection, thread_id: i64) -> Result<bool> {
    if !get_config(conn)?.enabled {
        return Ok(false);
    }
    let row: Option<(bool, Option<i64>, Option<String>, i64)> = conn
        .query_row(
            "SELECT knowledge_extracted, knowledge_msg_count, knowledge_error, message_count
             FROM threads WHERE id = ?1",
            [thread_id],
            |r| Ok((r.get::<_, i64>(0)? != 0, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((extracted, kmsg, error, mcount)) = row else {
        return Ok(false);
    };
    let stale = extracted && kmsg != Some(mcount);
    Ok(error.is_none() && (!extracted || stale))
}

/// Read the distilled knowledge for one thread (cached; does not run the model).
pub fn get_thread_knowledge(conn: &Connection, thread_id: i64) -> Result<ThreadKnowledge> {
    let (extracted, kmsg, error, mcount): (bool, Option<i64>, Option<String>, i64) = conn
        .query_row(
            "SELECT knowledge_extracted, knowledge_msg_count, knowledge_error, message_count
         FROM threads WHERE id = ?1",
            [thread_id],
            |r| Ok((r.get::<_, i64>(0)? != 0, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;

    let mut stmt = conn.prepare(
        "SELECT id, kind, text, pinned FROM facts
         WHERE thread_id = ?1 AND hidden = 0 ORDER BY pinned DESC, seq, id",
    )?;
    let rows = stmt.query_map([thread_id], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)? != 0,
        ))
    })?;
    let mut summary = None;
    let (mut decisions, mut gotchas, mut todos) = (Vec::new(), Vec::new(), Vec::new());
    for row in rows {
        let (id, kind, text, pinned) = row?;
        match kind.as_str() {
            "summary" => summary = Some(text),
            "decision" => decisions.push(KFact { id, text, pinned }),
            "gotcha" => gotchas.push(KFact { id, text, pinned }),
            "todo" => todos.push(KFact { id, text, pinned }),
            _ => {}
        }
    }
    Ok(ThreadKnowledge {
        summary,
        decisions,
        gotchas,
        todos,
        extracted,
        stale: extracted && kmsg != Some(mcount),
        error,
        can_distill: get_config(conn)?.enabled,
    })
}

/// A semantically-recalled fact (decision/gotcha) with the thread it came from.
#[derive(Debug, Serialize)]
pub struct RecallHit {
    pub id: i64,
    #[serde(rename = "threadId")]
    pub thread_id: i64,
    pub kind: String,
    pub text: String,
    pub source: String,
    pub title: Option<String>,
    #[serde(rename = "projectPath")]
    pub project_path: Option<String>,
    pub similarity: f32,
}

/// Cross-thread semantic recall of distilled facts. `qv` is a PRECOMPUTED query vector
/// (embed it via `embed::embed_query` BEFORE locking the DB so inference never holds the
/// lock). `kind` is 'decision' or 'gotcha'; optionally scope to a project-path substring.
pub fn recall(
    conn: &Connection,
    qv: &[f32],
    kind: &str,
    project: Option<&str>,
    k: usize,
) -> Result<Vec<RecallHit>> {
    // Over-fetch chunks so the kind/project filter (applied AFTER the KNN) still leaves k.
    let knn_k = (k * 5).max(100);
    let mut sql = format!(
        "WITH knn AS MATERIALIZED (
            SELECT fact_id, distance FROM vec_facts
            WHERE embedding MATCH ?1 AND k = {knn_k} ORDER BY distance
         )
         SELECT f.id, f.thread_id, f.kind, f.text, s.kind, t.title, t.project_path,
                MIN(knn.distance) AS d
         FROM knn
         JOIN facts f ON f.id = knn.fact_id
         JOIN threads t ON t.id = f.thread_id
         JOIN sources s ON s.id = t.source_id
         WHERE f.kind = ?2 AND f.hidden = 0"
    );
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = vec![
        Box::new(crate::embed::vec_to_bytes(qv)),
        Box::new(kind.to_string()),
    ];
    if let Some(p) = project.filter(|p| !p.is_empty()) {
        args.push(Box::new(format!("%{p}%")));
        sql.push_str(&format!(" AND t.project_path LIKE ?{}", args.len()));
    }
    args.push(Box::new(k as i64));
    sql.push_str(&format!(" GROUP BY f.id ORDER BY d LIMIT ?{}", args.len()));

    let arg_refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(arg_refs.as_slice(), |r| {
        let dist: f64 = r.get(7)?;
        Ok(RecallHit {
            id: r.get(0)?,
            thread_id: r.get(1)?,
            kind: r.get(2)?,
            text: r.get(3)?,
            source: r.get(4)?,
            title: r.get(5)?,
            project_path: r.get(6)?,
            similarity: (1.0 - dist) as f32,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// ---------------------------------------------------------------------------
// Project Memory — distilled knowledge aggregated across a project's threads.
// ---------------------------------------------------------------------------

/// A thread is "distillable" (worth an LLM pass / counted in coverage) if it's a real
/// top-level thread with enough substance. Keeps batch distill off trivial/subagent rows.
const DISTILLABLE: &str = "is_subagent = 0 AND message_count >= 4";

/// One distilled fact in a project's aggregated memory, with its source thread.
#[derive(Debug, Serialize)]
pub struct MemoryFact {
    pub id: i64,
    #[serde(rename = "threadId")]
    pub thread_id: i64,
    pub text: String,
    pub title: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    pub pinned: bool,
}

/// Durable, aggregated knowledge for one project: decisions + gotchas + open TODOs
/// distilled across all its threads, plus coverage so callers can prompt to distill more.
#[derive(Debug, Serialize)]
pub struct ProjectMemory {
    pub project: String,
    pub decisions: Vec<MemoryFact>,
    pub gotchas: Vec<MemoryFact>,
    #[serde(rename = "openTodos")]
    pub open_todos: Vec<MemoryFact>,
    #[serde(rename = "threadCount")]
    pub thread_count: i64,
    #[serde(rename = "distilledCount")]
    pub distilled_count: i64,
    #[serde(rename = "pendingCount")]
    pub pending_count: i64,
}

/// A project (by path) with its thread + distillation-coverage counts, for the picker.
#[derive(Debug, Serialize)]
pub struct ProjectInfo {
    pub project: String,
    #[serde(rename = "threadCount")]
    pub thread_count: i64,
    #[serde(rename = "distilledCount")]
    pub distilled_count: i64,
    #[serde(rename = "lastActivity")]
    pub last_activity: i64,
}

/// Distinct projects (by `project_path`), newest-active first, with distillation coverage.
pub fn list_projects(conn: &Connection) -> Result<Vec<ProjectInfo>> {
    // Group on the canonical project key (falls back to project_path until backfill runs)
    // so worktrees / symlinks / ~ vs absolute don't split one repo into several projects.
    let mut stmt = conn.prepare(&format!(
        "SELECT COALESCE(project_key, project_path) AS pkey,
                COUNT(*) AS threads,
                SUM(CASE WHEN knowledge_extracted = 1 THEN 1 ELSE 0 END) AS distilled,
                MAX(updated_at) AS last
         FROM threads
         WHERE project_path IS NOT NULL AND project_path != '' AND {DISTILLABLE}
         GROUP BY pkey
         ORDER BY last DESC"
    ))?;
    let rows = stmt.query_map([], |r| {
        Ok(ProjectInfo {
            project: r.get(0)?,
            thread_count: r.get(1)?,
            distilled_count: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
            last_activity: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Distilled facts of one `kind` across a project, newest first, deduped by text, capped.
fn project_facts(
    conn: &Connection,
    kind: &str,
    project: &str,
    open_only: bool,
    limit: usize,
) -> Result<Vec<MemoryFact>> {
    // Match the canonical project key (callers pass it; falls back to project_path until
    // backfill), so all of one repo's threads aggregate together.
    let mut sql = String::from(
        "SELECT f.id, f.thread_id, f.text, t.title, f.created_at, f.pinned
         FROM facts f JOIN threads t ON t.id = f.thread_id
         WHERE f.kind = ?1 AND COALESCE(t.project_key, t.project_path) = ?2 AND f.hidden = 0",
    );
    if open_only {
        sql.push_str(" AND f.status = 'open'");
    }
    // Pinned facts first, then newest. Pinned ones are the user's trusted set.
    sql.push_str(" ORDER BY f.pinned DESC, f.created_at DESC, f.id DESC");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params![kind, project], |r| {
        Ok(MemoryFact {
            id: r.get(0)?,
            thread_id: r.get(1)?,
            text: r.get(2)?,
            title: r.get(3)?,
            created_at: r.get(4)?,
            pinned: r.get::<_, i64>(5)? != 0,
        })
    })?;
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for row in rows {
        let f = row?;
        if seen.insert(f.text.trim().to_lowercase()) {
            out.push(f);
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(out)
}

/// Aggregate a project's distilled memory. `per_kind` caps each list (decisions/gotchas/
/// todos). Coverage counts are over "distillable" threads so the UI can show N/M distilled.
pub fn get_project_memory(
    conn: &Connection,
    project: &str,
    per_kind: usize,
) -> Result<ProjectMemory> {
    let count = |extra: &str| -> Result<i64> {
        Ok(conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM threads
                 WHERE COALESCE(project_key, project_path) = ?1 AND {DISTILLABLE}{extra}"
            ),
            [project],
            |r| r.get(0),
        )?)
    };
    let thread_count = count("")?;
    let distilled_count = count(" AND knowledge_extracted = 1")?;
    Ok(ProjectMemory {
        decisions: project_facts(conn, "decision", project, false, per_kind)?,
        gotchas: project_facts(conn, "gotcha", project, false, per_kind)?,
        open_todos: project_facts(conn, "todo", project, true, per_kind)?,
        thread_count,
        distilled_count,
        pending_count: (thread_count - distilled_count).max(0),
        project: project.to_string(),
    })
}

/// IDs of a project's not-yet-distilled threads, newest first — the batch-distill worklist.
pub fn project_pending_threads(conn: &Connection, project: &str) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id FROM threads
         WHERE COALESCE(project_key, project_path) = ?1 AND knowledge_extracted = 0 AND {DISTILLABLE}
         ORDER BY updated_at DESC"
    ))?;
    let rows = stmt.query_map([project], |r| r.get::<_, i64>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Record an agent- or user-authored fact (kind must be 'decision' or 'gotcha') for a
/// project. Stored in a synthetic per-project "Recorded memory" thread, pinned + ready to
/// embed, so it flows through Project Memory and cross-thread recall like a distilled fact.
/// Returns the new fact id (the caller embeds via embed_pending_facts).
pub fn record_fact(
    conn: &Connection,
    project: &str,
    kind: &str,
    text: &str,
    now: i64,
) -> Result<i64> {
    if !matches!(kind, "decision" | "gotcha") {
        anyhow::bail!("record_fact kind must be 'decision' or 'gotcha'");
    }
    let source_id: i64 =
        conn.query_row("SELECT id FROM sources WHERE kind = 'in_app'", [], |r| {
            r.get(0)
        })?;
    let ext = format!("callimachus-notes:{project}");
    conn.execute(
        "INSERT INTO threads (source_id, external_id, title, project_path, project_key, created_at, updated_at)
         VALUES (?1, ?2, 'Recorded memory', ?3, ?3, ?4, ?4)
         ON CONFLICT(source_id, external_id) DO UPDATE SET updated_at = ?4",
        params![source_id, ext, project, now],
    )?;
    let tid: i64 = conn.query_row(
        "SELECT id FROM threads WHERE source_id = ?1 AND external_id = ?2",
        params![source_id, ext],
        |r| r.get(0),
    )?;
    conn.execute(
        "INSERT INTO facts (thread_id, kind, text, status, extractor, pinned, created_at)
         VALUES (?1, ?2, ?3, 'open', 'agent', 1, ?4)",
        params![tid, kind, text.trim(), now],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Visible (non-hidden) distilled decisions for a project, id + text — for conflict review.
pub fn project_decisions(conn: &Connection, project: &str) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT f.id, f.text FROM facts f JOIN threads t ON t.id = f.thread_id
         WHERE f.kind = 'decision' AND COALESCE(t.project_key, t.project_path) = ?1 AND f.hidden = 0
         ORDER BY f.created_at DESC",
    )?;
    let rows = stmt.query_map([project], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// IDs of threads ANYWHERE that need distilling — never distilled, or changed since their
/// last distill. Newest first, capped. Skips threads that previously errored so a broken
/// one isn't retried forever. The auto-distill worklist.
pub fn pending_threads(conn: &Connection, limit: i64) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id FROM threads
         WHERE {DISTILLABLE} AND knowledge_error IS NULL
           AND (knowledge_extracted = 0 OR knowledge_msg_count != message_count)
         ORDER BY updated_at DESC
         LIMIT ?1"
    ))?;
    let rows = stmt.query_map([limit], |r| r.get::<_, i64>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_markers_and_tasks_only() {
        let text = "\
            Here is what we did.\n\
            TODO: wire up the refresh token flow\n\
            - [ ] add a retry to the uploader\n\
            - [x] already done, ignore me\n\
            We use mastodon for our posts.\n\
            // FIXME handle the empty-array case\n\
            just a normal sentence";
        let todos = extract_todos(text);
        assert!(todos.iter().any(|t| t == "wire up the refresh token flow"));
        assert!(todos.iter().any(|t| t == "add a retry to the uploader"));
        assert!(todos.iter().any(|t| t == "handle the empty-array case"));
        // "mastodon" must NOT trip the word-boundaried todo marker.
        assert!(!todos.iter().any(|t| t.contains("mastodon")));
        // the checked task is not an open todo.
        assert!(!todos.iter().any(|t| t.contains("already done")));
        assert_eq!(todos.len(), 3);
    }

    #[test]
    fn ignores_bare_or_tiny_markers() {
        assert!(extract_todos("TODO").is_empty());
        assert!(extract_todos("TODO: x").is_empty()); // < 4 chars after the marker
        assert!(extract_todos("no markers here at all").is_empty());
    }

    #[test]
    fn rejects_structured_noise() {
        // markdown table cell, escaped-newline blob, and JSON — all junk, not todos.
        assert!(extract_todos("| Windows .exe | TODO (needs a cert) |").is_empty());
        assert!(extract_todos("TODO: run \\nthen \\nmore blob output").is_empty());
        assert!(extract_todos("TODO: {\"key\": \"value\"}").is_empty());
    }

    fn temp_db() -> Connection {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "calli_know_{}_{}.db",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        crate::db::open(&p).unwrap()
    }

    #[test]
    fn distill_config_store_and_invalidation() {
        use crate::agent::Distilled;
        use crate::indexer::{source_id, upsert_thread, ParsedMessage, ParsedThread};

        let mut conn = temp_db();
        let sid = source_id(&conn, "claude_code").unwrap();
        let msg = |r: &str, t: &str| ParsedMessage {
            role: r.into(),
            text: t.into(),
            tool_name: None,
            ts: Some(1),
        };
        let seed = |msgs: Vec<ParsedMessage>| ParsedThread {
            external_id: "k1".into(),
            title: Some("auth".into()),
            created_at: Some(1),
            updated_at: Some(2),
            messages: msgs,
            ..Default::default()
        };
        upsert_thread(
            &mut conn,
            sid,
            &seed(vec![msg("user", "hi"), msg("assistant", "yo")]),
        )
        .unwrap();
        let id: i64 = conn
            .query_row("SELECT id FROM threads WHERE external_id = 'k1'", [], |r| {
                r.get(0)
            })
            .unwrap();

        // Off by default → never distills.
        assert!(!get_config(&conn).unwrap().enabled);
        assert!(!needs_distill(&conn, id).unwrap());

        // Enable (consent) with the local Ollama engine.
        set_config(&conn, true, Some("ollama"), Some("llama3.1")).unwrap();
        let cfg = get_config(&conn).unwrap();
        assert!(cfg.enabled && cfg.provider.as_deref() == Some("ollama"));
        assert!(needs_distill(&conn, id).unwrap()); // enabled + never extracted

        // Store distilled facts (empty items dropped).
        let d = Distilled {
            summary: "did auth".into(),
            decisions: vec!["use jwt".into(), "  ".into()],
            gotchas: vec!["clock skew".into()],
        };
        store_distilled(&mut conn, id, &d, 100).unwrap();
        let k = get_thread_knowledge(&conn, id).unwrap();
        assert_eq!(k.summary.as_deref(), Some("did auth"));
        assert_eq!(k.decisions.len(), 1);
        assert_eq!(k.gotchas.len(), 1);
        assert!(k.extracted && !k.stale && k.can_distill);
        assert!(!needs_distill(&conn, id).unwrap()); // already done

        // Re-index with an added message must invalidate the distilled facts.
        upsert_thread(
            &mut conn,
            sid,
            &seed(vec![
                msg("user", "hi"),
                msg("assistant", "yo"),
                msg("user", "more"),
            ]),
        )
        .unwrap();
        let k2 = get_thread_knowledge(&conn, id).unwrap();
        assert!(!k2.extracted, "changed message count must reset extraction");
        assert!(k2.decisions.is_empty(), "llm facts cleared on invalidation");
        assert!(needs_distill(&conn, id).unwrap());
    }

    #[test]
    fn curation_survives_redistill_and_hides() {
        use crate::agent::Distilled;
        use crate::indexer::{source_id, upsert_thread, ParsedMessage, ParsedThread};

        let mut conn = temp_db();
        let sid = source_id(&conn, "claude_code").unwrap();
        let m = |r: &str, t: &str| ParsedMessage {
            role: r.into(),
            text: t.into(),
            tool_name: None,
            ts: Some(1),
        };
        upsert_thread(
            &mut conn,
            sid,
            &ParsedThread {
                external_id: "pin1".into(),
                title: Some("p".into()),
                project_path: Some("/proj/p".into()),
                created_at: Some(1),
                updated_at: Some(2),
                messages: vec![m("user", "hi"), m("assistant", "yo")],
                ..Default::default()
            },
        )
        .unwrap();
        let tid: i64 = conn
            .query_row("SELECT id FROM threads WHERE external_id='pin1'", [], |r| {
                r.get(0)
            })
            .unwrap();

        let d = |dec: &str| Distilled {
            summary: String::new(),
            decisions: vec![dec.into()],
            gotchas: vec![],
        };
        store_distilled(&mut conn, tid, &d("use sqlite"), 1).unwrap();
        let fid: i64 = conn
            .query_row(
                "SELECT id FROM facts WHERE thread_id=?1 AND kind='decision'",
                [tid],
                |r| r.get(0),
            )
            .unwrap();
        set_fact_pinned(&conn, fid, true).unwrap();

        // Re-distill with a DIFFERENT decision: the pinned one must survive.
        store_distilled(&mut conn, tid, &d("use postgres"), 2).unwrap();
        let texts: Vec<String> = {
            let mut s = conn
                .prepare(
                    "SELECT text FROM facts WHERE thread_id=?1 AND kind='decision' ORDER BY id",
                )
                .unwrap();
            s.query_map([tid], |r| r.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        assert!(
            texts.contains(&"use sqlite".to_string()),
            "pinned decision survived: {texts:?}"
        );
        assert!(
            texts.contains(&"use postgres".to_string()),
            "re-distill added the new decision"
        );

        // Hidden facts don't surface in project memory.
        set_fact_hidden(&conn, fid, true).unwrap();
        let mem = get_project_memory(&conn, "/proj/p", 60).unwrap();
        assert!(
            !mem.decisions.iter().any(|f| f.text == "use sqlite"),
            "hidden fact stays hidden"
        );
    }

    #[test]
    fn recorded_fact_surfaces_in_project_memory() {
        let conn = temp_db();
        let fid = record_fact(&conn, "/proj/r", "decision", "use the read pool", 100).unwrap();
        assert!(fid > 0);
        let mem = get_project_memory(&conn, "/proj/r", 60).unwrap();
        assert!(
            mem.decisions
                .iter()
                .any(|d| d.text == "use the read pool" && d.pinned),
            "recorded decision is pinned + shown in project memory"
        );
        // A second record reuses the one synthetic notes thread for the project.
        record_fact(&conn, "/proj/r", "gotcha", "watch the WAL", 101).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM threads WHERE external_id LIKE 'callimachus-notes:%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "one notes thread per project");
        // Invalid kind is rejected.
        assert!(record_fact(&conn, "/proj/r", "note", "x", 102).is_err());
    }

    #[test]
    fn todo_done_survives_reindex() {
        use crate::indexer::{source_id, upsert_thread, ParsedMessage, ParsedThread};
        let mut conn = temp_db();
        set_config(&conn, true, Some("ollama"), Some("m")).unwrap(); // enable todo extraction
        let sid = source_id(&conn, "claude_code").unwrap();
        let m = |t: &str| ParsedMessage {
            role: "user".into(),
            text: t.into(),
            tool_name: None,
            ts: Some(1),
        };
        let thread = ParsedThread {
            external_id: "td1".into(),
            title: Some("t".into()),
            created_at: Some(1),
            updated_at: Some(2),
            messages: vec![m("TODO: wire auth"), m("- [ ] add tests")],
            ..Default::default()
        };
        upsert_thread(&mut conn, sid, &thread).unwrap();
        let tid: i64 = conn
            .query_row("SELECT id FROM threads WHERE external_id='td1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let todo_id: i64 = conn
            .query_row(
                "SELECT id FROM facts WHERE thread_id=?1 AND kind='todo' AND text LIKE 'wire auth%'",
                [tid],
                |r| r.get(0),
            )
            .unwrap();
        set_todo_done(&conn, todo_id, true).unwrap();

        // Re-index (re-derives heuristic todos): the closed one must persist, not duplicate.
        upsert_thread(&mut conn, sid, &thread).unwrap();
        let status: String = conn
            .query_row("SELECT status FROM facts WHERE id=?1", [todo_id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(status, "done", "todo stayed done across re-index");
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM facts WHERE thread_id=?1 AND kind='todo' AND text LIKE 'wire auth%'",
                [tid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "no duplicate open todo re-added");
    }

    #[test]
    fn recall_finds_nearest_fact_by_kind() {
        use crate::embed::{vec_to_bytes, DIM};
        use crate::indexer::{source_id, upsert_thread, ParsedMessage, ParsedThread};

        let mut conn = temp_db();
        let sid = source_id(&conn, "claude_code").unwrap();
        upsert_thread(
            &mut conn,
            sid,
            &ParsedThread {
                external_id: "r1".into(),
                title: Some("auth".into()),
                messages: vec![ParsedMessage {
                    role: "user".into(),
                    text: "x".into(),
                    tool_name: None,
                    ts: Some(1),
                }],
                ..Default::default()
            },
        )
        .unwrap();
        let tid: i64 = conn
            .query_row("SELECT id FROM threads WHERE external_id = 'r1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        conn.execute(
            "INSERT INTO facts (thread_id, kind, text, status, extractor, created_at)
             VALUES (?1, 'decision', 'use jwt for auth', 'open', 'llm', 1)",
            [tid],
        )
        .unwrap();
        let fid = conn.last_insert_rowid();
        let v = vec![0.1_f32; DIM];
        conn.execute(
            "INSERT INTO vec_facts (fact_id, embedding) VALUES (?1, ?2)",
            rusqlite::params![fid, vec_to_bytes(&v)],
        )
        .unwrap();

        let hits = recall(&conn, &v, "decision", None, 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text, "use jwt for auth");
        assert!(hits[0].similarity > 0.99);
        // Kind filter excludes non-matching kinds.
        assert!(recall(&conn, &v, "gotcha", None, 5).unwrap().is_empty());
    }
}
