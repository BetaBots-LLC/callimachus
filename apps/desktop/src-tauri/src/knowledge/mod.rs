//! Distilled knowledge layer. Slice 1 is the FREE heuristic tier: TODO/action items
//! pulled out of message text with conservative, low-noise markers (markdown unchecked
//! tasks + TODO/FIXME markers that open a line), stored in the `facts` table by the indexer
//! and surfaced via `list_open_todos` (desktop, `cal todos`, MCP). No model, no API key.
//! The LLM tier (decisions / gotchas / summaries, lazy on-demand) reuses the same table, as
//! do facts recorded by the user or by agents (`record_fact_with_origin`).

pub mod app_config;

use crate::agent::Distilled;
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

/// Hard cap on a single extracted TODO so one runaway line can't bloat the list.
const MAX_TODO_LEN: usize = 240;

/// Pull likely TODO/action items out of one message's text. Intentionally
/// conservative, so we get signal, not every "we need to" aside:
/// - markdown unchecked tasks (`- [ ] wire up auth`), and
/// - a `TODO` / `FIXME` marker that OPENS a line, optionally after a comment or bullet
///   leader (`// TODO: x`, `- FIXME x`, `# TODO(ari): x`).
///
/// A marker in the middle of a sentence (`set_todo_done(false) and complete_todo …`,
/// `'todo' kind`, `TODO/FIXME markers`) is prose about TODOs, not a TODO, and is ignored.
/// So are fenced code blocks (code under discussion, not the user's action items) and
/// injected context such as a loaded skill body, whose checklists are templates.
pub fn extract_todos(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    if is_injected_context(text) {
        return out;
    }
    let mut in_fence = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || line.is_empty() {
            continue;
        }
        let candidate = unchecked_task(line).or_else(|| leading_marker(line));
        if let Some(todo) = candidate.and_then(clean) {
            out.push(todo);
        }
    }
    out
}

/// Message text that an agent harness injected rather than the user or model writing it:
/// skill bodies, slash-command expansions, system reminders, and Callimachus's own
/// injected memory / packed threads (extracting from those would feed TODOs back to us).
fn is_injected_context(text: &str) -> bool {
    let t = text.trim_start();
    const PREFIXES: [&str; 8] = [
        "Base directory for this skill",
        "<command-name>",
        "<command-message>",
        "<system-reminder>",
        "<local-command-stdout>",
        "Caveat: The messages below were generated",
        "<reference_thread",
        "<project_memory",
    ];
    PREFIXES.iter().any(|p| t.starts_with(p)) || t.contains(crate::export::MANAGED_START)
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

/// The text after a TODO/FIXME marker that opens `line` (after comment, bullet, quote,
/// numbering or emphasis leaders). The marker must be followed by `:` or `(` in any case,
/// or by a space / dash when written in capitals: `TODO fix x` is a marker, `Todo list
/// for the week` is prose. Anything glued to it (`todos`, `todo_list`, `TODO/FIXME`) is not.
fn leading_marker(line: &str) -> Option<&str> {
    const LEADERS: [&str; 11] = ["<!--", "//", "/*", "--", "#", ";", "*", "-", "+", ">", "_"];
    let mut rest = line;
    loop {
        let before = rest;
        rest = rest.trim_start();
        if let Some(r) = LEADERS.iter().find_map(|l| rest.strip_prefix(l)) {
            rest = r;
        }
        // Ordered-list numbering: `1.` / `2)`.
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 && rest[digits..].starts_with(['.', ')']) {
            rest = &rest[digits + 1..];
        }
        if rest == before {
            break;
        }
    }
    // `get` rather than indexing: the line may start with a multi-byte char.
    let marker = ["TODO", "FIXME"].into_iter().find(|m| {
        rest.get(..m.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(m))
    })?;
    let written_upper = rest.get(..marker.len()) == Some(marker);
    let after = &rest[marker.len()..];
    match after.chars().next() {
        Some(':') | Some('(') => {}
        Some(c) if (c == ' ' || c == '\t' || c == '-') && written_upper => {}
        _ => return None,
    }
    Some(strip_marker_tail(after))
}

/// Drop what follows a TODO marker before its text: an owner tag `(ari)`, separators, and
/// a trailing comment closer. `(ari): fix the parser */` -> `fix the parser`.
fn strip_marker_tail(after: &str) -> &str {
    let mut rest = after;
    if rest.starts_with('(') {
        if let Some(close) = rest.find(')') {
            rest = &rest[close + 1..];
        }
    }
    rest.trim_start_matches([':', '-', ' ', '\t', ')', '.', '*', '_'])
        .trim_end_matches("*/")
        .trim_end_matches("-->")
        .trim_end()
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

/// `facts.created_at` for a heuristic TODO: when its source message was written, not when
/// the thread was (re)indexed, so "newest first" means newest TODO rather than "most
/// recently re-indexed thread". Falls back to `now` for sources without timestamps;
/// tolerates millisecond stamps and never returns a time in the future.
pub fn todo_created_at(message_ts: Option<i64>, now: i64) -> i64 {
    match message_ts {
        Some(ts) if ts > 100_000_000_000 => (ts / 1000).min(now),
        Some(ts) if ts > 0 => ts.min(now),
        _ => now,
    }
}

/// Drop the open, uncurated heuristic TODOs of subagent threads (extracted before
/// extraction skipped them). Curated ones (done / pinned / hidden) are kept. Idempotent and
/// cheap; run once at startup. Returns the number of rows removed.
pub fn purge_subagent_todos(conn: &Connection) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM facts WHERE extractor = 'heuristic' AND kind = 'todo'
            AND status = 'open' AND pinned = 0 AND hidden = 0
            AND thread_id IN (SELECT id FROM threads WHERE is_subagent = 1)",
        [],
    )?)
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
    // Subagent transcripts are the parent session's delegated work (most of the corpus in
    // workflow-heavy repos); their checklists and code comments are not the user's TODOs.
    let is_subagent: bool = tx
        .query_row(
            "SELECT is_subagent FROM threads WHERE id = ?1",
            [thread_id],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .is_some_and(|v| v != 0);
    if is_subagent {
        return Ok(());
    }
    let mut sel = tx.prepare(
        "SELECT id, text, ts FROM messages WHERE thread_id = ?1 AND role IN ('user', 'assistant')
         ORDER BY seq, id",
    )?;
    let rows: Vec<(i64, String, Option<i64>)> = sel
        .query_map([thread_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
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
    'outer: for (mid, text, ts) in rows {
        if per >= MAX_TODOS_PER_THREAD {
            break;
        }
        for todo in extract_todos(&text) {
            if per >= MAX_TODOS_PER_THREAD {
                break 'outer;
            }
            if seen.insert(todo.to_ascii_lowercase()) {
                let created = todo_created_at(ts, now);
                ins.execute(rusqlite::params![thread_id, todo, mid, created])?;
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
        // Subagent threads get no heuristic TODOs (see rebuild_thread_todos).
        let mut stmt = conn.prepare("SELECT id FROM threads WHERE is_subagent = 0 ORDER BY id")?;
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
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
         WHERE f.kind = 'todo' AND f.status = 'open' AND f.hidden = 0
           AND (t.is_subagent = 0 OR f.pinned = 1)",
    );
    // Subagent threads only ever yield uncurated heuristic TODOs, so they are left out, but
    // one the user pinned (an older build extracted them) must stay listed: otherwise
    // `purge_subagent_todos` keeps a row nobody can see, unpin or complete.
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

/// Read the distillation config (defaults to disabled / unset).
pub fn get_config(conn: &Connection) -> Result<KnowledgeConfig> {
    Ok(KnowledgeConfig {
        enabled: app_config::get_flag(conn, "knowledge.enabled")?,
        provider: app_config::get_nonblank(conn, "knowledge.provider")?,
        model: app_config::get_nonblank(conn, "knowledge.model")?,
        auto_distill: app_config::get_flag(conn, "knowledge.auto_distill")?,
    })
}

/// Toggle background auto-distillation (separate from `set_config` so the consent flag
/// and engine choice aren't disturbed). Only meaningful when distillation is enabled.
pub fn set_auto_distill(conn: &Connection, on: bool) -> Result<()> {
    app_config::put_flag(conn, "knowledge.auto_distill", on)
}

/// Whether closing the main window hides the app to the system tray instead of quitting
/// (default off). Read by the Rust window-close handler at startup.
pub fn get_close_to_tray(conn: &Connection) -> Result<bool> {
    app_config::get_flag(conn, "window.close_to_tray")
}

/// Persist the close-to-tray preference.
pub fn set_close_to_tray(conn: &Connection, on: bool) -> Result<()> {
    app_config::put_flag(conn, "window.close_to_tray", on)
}

/// Read a provider's saved custom base URL (unset / blank -> None). Non-secret config; API
/// keys live in the OS keychain. Used for local Ollama pointed at a remote host, and for
/// Ollama Cloud / authenticated proxies (paired with a keychain key).
pub fn get_provider_base_url(conn: &Connection, provider: &str) -> Result<Option<String>> {
    app_config::get_nonblank(conn, &format!("provider.{provider}.base_url"))
}

/// Persist a provider's custom base URL. A blank string clears it (falls back to the default).
pub fn set_provider_base_url(conn: &Connection, provider: &str, url: &str) -> Result<()> {
    app_config::put(conn, &format!("provider.{provider}.base_url"), url.trim())
}

/// Persist the distillation config. Enabling it is the user's consent to send thread
/// text to the chosen engine (cloud key) — or to keep it local (Ollama).
pub fn set_config(
    conn: &Connection,
    enabled: bool,
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<()> {
    app_config::put_flag(conn, "knowledge.enabled", enabled)?;
    app_config::put(conn, "knowledge.provider", provider.unwrap_or(""))?;
    app_config::put(conn, "knowledge.model", model.unwrap_or(""))
}

/// Replace a thread's LLM-distilled facts with a fresh set and mark it extracted at
/// the current message count. Heuristic todos (extractor='heuristic') are untouched.
pub fn store_distilled(
    conn: &mut Connection,
    thread_id: i64,
    d: &Distilled,
    now: i64,
) -> Result<()> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
            knowledge_msg_count = distillable_count, knowledge_error = NULL WHERE id = ?1",
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
            "SELECT knowledge_extracted, knowledge_msg_count, knowledge_error, distillable_count
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
            "SELECT knowledge_extracted, knowledge_msg_count, knowledge_error, distillable_count
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
    /// ADR rationale ("why"), when the decision has one. NULL for gotchas/todos.
    pub rationale: Option<String>,
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
         SELECT f.id, f.thread_id, f.kind, f.text, f.rationale, s.kind, t.title, t.project_path,
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
        // Match how facts are WRITTEN + aggregated (record_fact / project_memory both scope by
        // COALESCE(project_key, project_path)); filtering project_path alone silently drops the
        // canonical-key threads the backfill exists to fix.
        sql.push_str(&format!(
            " AND COALESCE(t.project_key, t.project_path) LIKE ?{}",
            args.len()
        ));
    }
    // Similarity floor: a cosine KNN always returns its k-nearest, so a query with no real match
    // still yields confident-but-wrong hits. Drop neighbors below MIN_SIMILARITY (distance =
    // 1 - cosine, so similarity >= floor ⟺ distance <= 1 - floor) and return an explicit empty.
    const MIN_SIMILARITY: f64 = 0.45;
    args.push(Box::new(1.0_f64 - MIN_SIMILARITY));
    let dist_ph = args.len();
    args.push(Box::new(k as i64));
    let limit_ph = args.len();
    sql.push_str(&format!(
        " GROUP BY f.id HAVING d <= ?{dist_ph} ORDER BY d LIMIT ?{limit_ph}"
    ));

    let arg_refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(arg_refs.as_slice(), |r| {
        let dist: f64 = r.get(8)?;
        Ok(RecallHit {
            id: r.get(0)?,
            thread_id: r.get(1)?,
            kind: r.get(2)?,
            text: r.get(3)?,
            rationale: r.get(4)?,
            source: r.get(5)?,
            title: r.get(6)?,
            project_path: r.get(7)?,
            similarity: (1.0 - dist) as f32,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// One past session relevant to what you're about to do — the "you solved this before"
/// guard. Distinct from `recall` (which returns individual decision/gotcha facts): this
/// rolls the matching facts UP to their thread, so the answer is a short list of prior
/// SESSIONS to revisit, each with the single most-relevant decision/gotcha as the reason.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PriorWork {
    pub thread_id: i64,
    pub title: Option<String>,
    pub project_path: Option<String>,
    pub source: String,
    pub kind: String,    // kind of the best-matching fact ('decision' | 'gotcha')
    pub snippet: String, // the best-matching fact text (why this thread is relevant)
    pub similarity: f32,
    pub matches: i64, // how many decision/gotcha facts in this thread matched
}

/// Find prior sessions semantically similar to `qv` (an embedded task description), by
/// recalling decisions + gotchas and grouping them by thread. `project` optionally scopes
/// to one repo; pass None to search everything. Returns up to `k` threads, best match first.
pub fn find_prior_work(
    conn: &Connection,
    qv: &[f32],
    project: Option<&str>,
    k: usize,
) -> Result<Vec<PriorWork>> {
    let over = (k * 3).max(15);
    let mut hits = recall(conn, qv, "decision", project, over)?;
    hits.extend(recall(conn, qv, "gotcha", project, over)?);

    let mut by_thread: std::collections::HashMap<i64, PriorWork> = std::collections::HashMap::new();
    for h in hits {
        let e = by_thread.entry(h.thread_id).or_insert_with(|| PriorWork {
            thread_id: h.thread_id,
            title: h.title.clone(),
            project_path: h.project_path.clone(),
            source: h.source.clone(),
            kind: h.kind.clone(),
            snippet: h.text.clone(),
            similarity: h.similarity,
            matches: 0,
        });
        e.matches += 1;
        if h.similarity > e.similarity {
            e.similarity = h.similarity;
            e.snippet = h.text.clone();
            e.kind = h.kind.clone();
        }
    }

    // The guard is high-stakes — an agent acts on "you solved this before", so a false positive
    // is worse than an empty answer. Hold it to a stricter floor than general recall; recall()
    // already dropped the clear noise, this keeps only genuinely-related prior sessions.
    const GUARD_FLOOR: f32 = 0.55;
    let mut out: Vec<PriorWork> = by_thread
        .into_values()
        .filter(|w| w.similarity >= GUARD_FLOOR)
        .collect();
    out.sort_by(|a, b| {
        b.similarity
            .partial_cmp(&a.similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(k);
    Ok(out)
}

/// The active contradiction guard: prior DECISIONS semantically close to `qv` (an embedded
/// proposal an agent is about to act on), held to a stricter floor than general recall — a
/// false "you already decided X" is worse than silence. Surface these before the agent
/// re-litigates a settled choice. Returns decision facts (each with its rationale, when
/// recorded) best-match first. `project` optionally scopes to one repo.
pub fn check_contradiction(
    conn: &Connection,
    qv: &[f32],
    project: Option<&str>,
    k: usize,
) -> Result<Vec<RecallHit>> {
    // Same high-stakes floor as find_prior_work: recall() already dropped the clear noise; this
    // keeps only decisions genuinely on the proposal's topic.
    const GUARD_FLOOR: f32 = 0.55;
    let mut hits = recall(conn, qv, "decision", project, (k * 3).max(15))?;
    hits.retain(|h| h.similarity >= GUARD_FLOOR);
    hits.truncate(k);
    Ok(hits)
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
         WHERE f.kind = ?1 AND COALESCE(t.project_key, t.project_path) = ?2 AND f.hidden = 0
           AND (t.is_subagent = 0 OR f.pinned = 1)",
    );
    // Pinned facts on subagent threads stay: the user curated them (see list_open_todos).
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
    let mut mem = get_project_memory_facts(conn, project, per_kind)?;
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
    mem.thread_count = count("")?;
    mem.distilled_count = count(" AND knowledge_extracted = 1")?;
    mem.pending_count = (mem.thread_count - mem.distilled_count).max(0);
    Ok(mem)
}

/// [`get_project_memory`] without the coverage counts, which stay 0: just the facts. For
/// callers that never show "N/M distilled", i.e. everything rendered through
/// `export::agent_memory_md_budgeted` (the SessionStart hook, the compaction carry). The two
/// counts can't use an index (`COALESCE(project_key, project_path)`), so skipping them
/// saves about 0.85 ms on every hook run.
pub fn get_project_memory_facts(
    conn: &Connection,
    project: &str,
    per_kind: usize,
) -> Result<ProjectMemory> {
    Ok(ProjectMemory {
        decisions: project_facts(conn, "decision", project, false, per_kind)?,
        gotchas: project_facts(conn, "gotcha", project, false, per_kind)?,
        open_todos: project_facts(conn, "todo", project, true, per_kind)?,
        thread_count: 0,
        distilled_count: 0,
        pending_count: 0,
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

/// Who wrote a recorded fact. Stored in `facts.extractor` ('user' / 'agent'), next to the
/// machine extractors 'llm' and 'heuristic', so no schema change is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactOrigin {
    /// Typed by the user in the desktop app: trusted, so it is pinned.
    User,
    /// Written back by an agent (MCP `record_decision` / `record_gotcha`, or `cal remember`
    /// run by an agent). Not pinned: an agent's write-back must not permanently outrank
    /// everything else, and a prompt-injected session must not be able to plant a fact
    /// that leads every future context block.
    Agent,
}

impl FactOrigin {
    fn extractor(self) -> &'static str {
        match self {
            FactOrigin::User => "user",
            FactOrigin::Agent => "agent",
        }
    }
}

/// Record an agent-authored fact (kind must be 'decision' or 'gotcha') for a project. Same
/// as `record_fact_with_origin(.., FactOrigin::Agent, ..)`: unpinned, `extractor = 'agent'`.
/// The desktop app's user-typed `remember` should use `FactOrigin::User` instead.
pub fn record_fact(
    conn: &Connection,
    project: &str,
    kind: &str,
    text: &str,
    rationale: Option<&str>,
    now: i64,
) -> Result<i64> {
    record_fact_with_origin(conn, project, kind, text, rationale, FactOrigin::Agent, now)
}

/// Record a fact (kind 'decision' or 'gotcha') for a project. Stored in a synthetic
/// per-project "Recorded memory" thread and left ready to embed, so it flows through
/// Project Memory and cross-thread recall like a distilled fact. User facts are pinned;
/// agent facts rank by recency like distilled ones. Returns the new fact id (the caller
/// embeds via embed_pending_facts).
pub fn record_fact_with_origin(
    conn: &Connection,
    project: &str,
    kind: &str,
    text: &str,
    rationale: Option<&str>,
    origin: FactOrigin,
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
        "INSERT INTO facts (thread_id, kind, text, rationale, status, extractor, pinned, created_at)
         VALUES (?1, ?2, ?3, ?4, 'open', ?5, ?6, ?7)",
        params![
            tid,
            kind,
            text.trim(),
            rationale.map(str::trim),
            origin.extractor(),
            i64::from(origin == FactOrigin::User),
            now
        ],
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

/// IDs of threads ANYWHERE that need distilling: never distilled, invalidated by new turns
/// (the indexer resets `knowledge_extracted` and drops the old facts), or distilled when
/// the message count was recorded differently. Newest first, capped. Skips threads that
/// previously errored so a broken one isn't retried forever.
///
/// A thread that is still marked distilled and has not been touched since
/// (`updated_at <= knowledge_extracted_at`) is NOT re-queued just because its stored count
/// disagrees: that is bookkeeping drift (for example the switch to counting only
/// user/assistant turns), not new content, and re-sending it would re-pay a full pack.
pub fn pending_threads(conn: &Connection, limit: i64) -> Result<Vec<i64>> {
    pending_threads_since(conn, limit, None)
}

/// `pending_threads`, optionally limited to threads updated at or after `since`.
pub fn pending_threads_since(
    conn: &Connection,
    limit: i64,
    since: Option<i64>,
) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id FROM threads
         WHERE {DISTILLABLE} AND knowledge_error IS NULL
           AND (knowledge_extracted = 0
                OR (knowledge_msg_count != distillable_count
                    AND (knowledge_extracted_at IS NULL OR updated_at IS NULL
                         OR updated_at > knowledge_extracted_at)))
           AND (?2 IS NULL OR updated_at >= ?2)
         ORDER BY updated_at DESC
         LIMIT ?1"
    ))?;
    let rows = stmt.query_map(params![limit, since], |r| r.get::<_, i64>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// ---- background auto-distill budget ----
//
// Auto-distill sends each pending thread (packed up to ~12k tokens) to the configured
// engine with nobody watching. Without a budget, turning it on with a cloud key drains the
// whole history in one go. Two limits apply: only recently active threads are eligible,
// and a per-day thread allowance (UTC day, stored in app_config so the app, `cal` and the
// MCP server share it). Manual distills (a project's "Build memory", `cal distill`) are
// user-initiated and are not counted.

/// Default number of threads auto-distill may send to the engine per UTC day.
pub const AUTO_DISTILL_DEFAULT_DAILY_CAP: i64 = 50;

/// Only threads active within this window are auto-distilled; older history stays
/// available to the explicit per-project "Build memory".
pub const AUTO_DISTILL_RECENT_DAYS: i64 = 30;

/// The per-day auto-distill allowance (`knowledge.auto_distill_daily_cap`).
pub fn auto_distill_daily_cap(conn: &Connection) -> Result<i64> {
    Ok(app_config::get(conn, "knowledge.auto_distill_daily_cap")?
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|v| *v >= 0)
        .unwrap_or(AUTO_DISTILL_DEFAULT_DAILY_CAP))
}

/// Set the per-day auto-distill allowance (0 pauses auto-distill without turning it off).
pub fn set_auto_distill_daily_cap(conn: &Connection, cap: i64) -> Result<()> {
    app_config::put(
        conn,
        "knowledge.auto_distill_daily_cap",
        &cap.max(0).to_string(),
    )
}

fn utc_day(now: i64) -> String {
    chrono::DateTime::from_timestamp(now, 0)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// Threads already reserved by auto-distill today.
fn auto_distill_used_today(conn: &Connection, now: i64) -> Result<i64> {
    if app_config::get(conn, "knowledge.auto_distill_day")?.as_deref()
        != Some(utc_day(now).as_str())
    {
        return Ok(0);
    }
    Ok(app_config::get(conn, "knowledge.auto_distill_used")?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0))
}

/// How many more threads auto-distill may send today.
pub fn auto_distill_remaining_today(conn: &Connection, now: i64) -> Result<i64> {
    Ok((auto_distill_daily_cap(conn)? - auto_distill_used_today(conn, now)?).max(0))
}

/// The next auto-distill batch: at most `batch` recently active pending threads, and never
/// more than today's remaining allowance. The returned ids are counted against the
/// allowance immediately, so a cancelled run errs on the side of spending less. Returns an
/// empty list once the day's allowance is used up, which ends the auto-distill drain.
pub fn auto_distill_worklist(conn: &Connection, batch: i64, now: i64) -> Result<Vec<i64>> {
    let take = batch.min(auto_distill_remaining_today(conn, now)?);
    if take <= 0 {
        return Ok(Vec::new());
    }
    let since = now - AUTO_DISTILL_RECENT_DAYS * 86_400;
    let ids = pending_threads_since(conn, take, Some(since))?;
    if !ids.is_empty() {
        let used = auto_distill_used_today(conn, now)? + ids.len() as i64;
        app_config::put(conn, "knowledge.auto_distill_day", &utc_day(now))?;
        app_config::put(conn, "knowledge.auto_distill_used", &used.to_string())?;
    }
    Ok(ids)
}

// ---- cheap keyword prefilter for proactive recall ----

/// Distinctive search terms from a prompt: lowercase words of 4+ chars that aren't
/// stopwords, lightly stemmed, longest first (longer words are more specific), deduped,
/// at most `max`.
pub fn prompt_terms(prompt: &str, max: usize) -> Vec<String> {
    const STOP: &[&str] = &[
        "this", "that", "with", "from", "have", "what", "when", "where", "which", "there", "their",
        "about", "would", "could", "should", "into", "your", "just", "like", "make", "some",
        "then", "than", "them", "they", "will", "been", "were", "does", "done", "doing", "want",
        "need", "please", "help", "also", "more", "only", "very", "here", "using", "work",
        "working", "thing", "things", "know", "look", "take", "give", "much", "many", "each",
        "every", "other", "file", "files", "code", "change", "update", "check", "sure", "think",
        "right", "still", "again", "after", "before", "because", "while", "these", "those",
        "being", "over", "under", "between",
    ];
    let mut terms: Vec<String> = Vec::new();
    for word in prompt
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .map(str::to_lowercase)
    {
        if word.chars().count() < 4 || STOP.contains(&word.as_str()) {
            continue;
        }
        let stem = ["ing", "ed", "es", "s"]
            .iter()
            .find_map(|suf| {
                word.strip_suffix(suf)
                    .filter(|rest| rest.chars().count() >= 4)
            })
            .unwrap_or(word.as_str())
            .to_string();
        if !terms.contains(&stem) {
            terms.push(stem);
        }
    }
    terms.sort_by_key(|t| std::cmp::Reverse(t.chars().count()));
    terms.truncate(max);
    terms
}

/// Whether any embedded, visible decision/gotcha (optionally in `project`, matched the same
/// way `recall` scopes) mentions one of `terms`. A plain LIKE scan over the fact texts,
/// which takes milliseconds, versus loading the embedding model (hundreds of ms and
/// ~400 MB). Proactive recall runs on every prompt, so it checks this first and stays
/// silent when nothing could match.
pub fn has_keyword_candidates(
    conn: &Connection,
    terms: &[String],
    project: Option<&str>,
) -> Result<bool> {
    if terms.is_empty() {
        return Ok(false);
    }
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    let mut likes = Vec::new();
    for t in terms {
        // Escape LIKE wildcards in the term itself.
        let escaped = t
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        args.push(Box::new(format!("%{escaped}%")));
        likes.push(format!("f.text LIKE ?{} ESCAPE '\\'", args.len()));
    }
    let mut sql = format!(
        "SELECT EXISTS (SELECT 1 FROM facts f JOIN threads t ON t.id = f.thread_id
         WHERE f.kind IN ('decision', 'gotcha') AND f.hidden = 0 AND f.embedded = 1
           AND ({})",
        likes.join(" OR ")
    );
    if let Some(p) = project.filter(|p| !p.is_empty()) {
        args.push(Box::new(format!("%{p}%")));
        sql.push_str(&format!(
            " AND COALESCE(t.project_key, t.project_path) LIKE ?{}",
            args.len()
        ));
    }
    sql.push(')');
    let arg_refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    Ok(conn.query_row(&sql, arg_refs.as_slice(), |r| r.get::<_, i64>(0))? != 0)
}

/// The project key for `raw` (a path, a `~/...` path, or an already-canonical key):
/// `indexer::canonical_project`, which resolves a worktree to its main repo and stops the
/// walk at $HOME, falling back to `raw` itself when it is blank. Every surface that scopes
/// to a project (`cal`, the hooks, the MCP tools) goes through this, so they all match the
/// key the indexer stored.
pub fn project_key(raw: &str) -> String {
    crate::indexer::canonical_project(raw).unwrap_or_else(|| raw.to_string())
}

/// [`project_key`] for the process's working directory, or None when it can't be read
/// (deleted out from under us). What `cal` and the MCP server scope to by default.
pub fn cwd_project_key() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    Some(project_key(&cwd.to_string_lossy()))
}

/// Recompute `threads.project_key` where the stored key is out of date with
/// `indexer::canonical_project`: keys computed before worktrees resolved to their main
/// repo, or before the walk up stopped at $HOME (when every non-repo dir under a
/// `~/.git` home collapsed into one "home" project). Only paths that still exist on disk
/// are re-resolved, plus any thread keyed to the home dir itself, so a repo that has since
/// moved keeps its old grouping. One canonicalize per distinct path; cheap. Returns the
/// number of threads re-keyed.
pub fn rekey_project_keys(conn: &Connection) -> Result<usize> {
    let home = dirs::home_dir().map(|h| {
        std::fs::canonicalize(&h)
            .unwrap_or(h)
            .to_string_lossy()
            .trim_end_matches(['/', '\\'])
            .to_string()
    });
    let pairs: Vec<(String, Option<String>)> = {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT project_path, project_key FROM threads
             WHERE project_path IS NOT NULL AND project_path != ''",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    let mut n = 0;
    for (path, key) in pairs {
        let keyed_to_home = key.is_some() && key == home;
        let exists = std::path::Path::new(path.trim()).exists();
        if !exists && !keyed_to_home {
            continue;
        }
        let Some(fresh) = crate::indexer::canonical_project(&path) else {
            continue;
        };
        if Some(&fresh) != key.as_ref() {
            n += conn.execute(
                "UPDATE threads SET project_key = ?1 WHERE project_path = ?2 AND project_key IS ?3",
                params![fresh, path, key],
            )?;
        }
    }
    Ok(n)
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
    fn ignores_markers_that_do_not_open_the_line() {
        // Prose ABOUT todos (the junk the old word-boundary match produced).
        for prose in [
            "set_todo_done(false) and set_fact_hidden exist but are wired only to the GUI",
            "the complete_todo tool marks it done",
            "heuristic TODO/FIXME markers (already conservatively filtered).",
            "facts with the 'todo' kind",
            "Todo list for the week",
            "todos are extracted heuristically",
        ] {
            assert!(extract_todos(prose).is_empty(), "matched prose: {prose}");
        }
        // Real markers, after comment / bullet / emphasis leaders.
        let text = "\
            # TODO wire the cache\n\
            - **TODO:** bold marker here\n\
            todo: buy more disks\n\
            1. FIXME(ari): handle retries */\n\
            <!-- TODO: fix the footer -->";
        let todos = extract_todos(text);
        assert_eq!(
            todos,
            vec![
                "wire the cache",
                "bold marker here",
                "buy more disks",
                "handle retries",
                "fix the footer"
            ]
        );
    }

    #[test]
    fn skips_code_fences_and_injected_context() {
        let text = "```rust\n// TODO: inside code\n```\nTODO: outside the fence";
        assert_eq!(extract_todos(text), vec!["outside the fence"]);
        let skill =
            "Base directory for this skill: /x/skills/plan\n\n- [ ] **[BLOCKS]** fill in the plan";
        assert!(extract_todos(skill).is_empty());
        let reminder = "<system-reminder>\nTODO: never extract this\n</system-reminder>";
        assert!(extract_todos(reminder).is_empty());
        // Multi-byte text at a line start must not panic the marker check.
        assert!(extract_todos("日本語のテキスト").is_empty());
    }

    #[test]
    fn todo_created_at_uses_message_time() {
        assert_eq!(
            todo_created_at(Some(1_700_000_000), 1_800_000_000),
            1_700_000_000
        );
        assert_eq!(
            todo_created_at(Some(1_700_000_000_000), 1_800_000_000),
            1_700_000_000
        );
        assert_eq!(todo_created_at(None, 42), 42);
        assert_eq!(
            todo_created_at(Some(2_000_000_000), 1_800_000_000),
            1_800_000_000
        );
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
    fn facts_only_memory_matches_full_memory_without_counts() {
        let mut conn = temp_db();
        let t = seed_thread(&mut conn, "cov", "/proj/c", false, &["a", "b", "c", "d"]);
        conn.execute(
            "UPDATE threads SET knowledge_extracted = 1 WHERE id = ?1",
            [t],
        )
        .unwrap();
        record_fact(&conn, "/proj/c", "gotcha", "watch the WAL", None, 10).unwrap();
        record_fact(&conn, "/proj/c", "decision", "keep sqlite", None, 11).unwrap();

        let full = get_project_memory(&conn, "/proj/c", 40).unwrap();
        assert_eq!(full.distilled_count, 1);
        assert!(full.thread_count >= 1);
        let facts = get_project_memory_facts(&conn, "/proj/c", 40).unwrap();
        assert_eq!(
            (
                facts.thread_count,
                facts.distilled_count,
                facts.pending_count
            ),
            (0, 0, 0)
        );
        let texts = |m: &ProjectMemory| {
            [&m.decisions, &m.gotchas, &m.open_todos]
                .iter()
                .map(|l| l.iter().map(|f| f.text.clone()).collect::<Vec<_>>())
                .collect::<Vec<_>>()
        };
        assert_eq!(texts(&full), texts(&facts));
    }

    #[test]
    fn recorded_fact_surfaces_in_project_memory() {
        let conn = temp_db();
        let fid = record_fact(
            &conn,
            "/proj/r",
            "decision",
            "use the read pool",
            Some("avoids writer contention"),
            100,
        )
        .unwrap();
        assert!(fid > 0);
        let stored: Option<String> = conn
            .query_row("SELECT rationale FROM facts WHERE id = ?1", [fid], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(stored.as_deref(), Some("avoids writer contention"));
        let mem = get_project_memory(&conn, "/proj/r", 60).unwrap();
        assert!(
            mem.decisions
                .iter()
                .any(|d| d.text == "use the read pool" && !d.pinned),
            "an agent-recorded decision is shown in project memory but NOT auto-pinned"
        );
        let origin: String = conn
            .query_row("SELECT extractor FROM facts WHERE id = ?1", [fid], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(origin, "agent");
        // A user-typed fact (desktop `remember`) is pinned and ranks first.
        record_fact_with_origin(
            &conn,
            "/proj/r",
            "decision",
            "user says: keep sqlite",
            None,
            FactOrigin::User,
            50,
        )
        .unwrap();
        let mem = get_project_memory(&conn, "/proj/r", 60).unwrap();
        assert_eq!(mem.decisions[0].text, "user says: keep sqlite");
        assert!(mem.decisions[0].pinned);
        // A second record reuses the one synthetic notes thread for the project.
        record_fact(&conn, "/proj/r", "gotcha", "watch the WAL", None, 101).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM threads WHERE external_id LIKE 'callimachus-notes:%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "one notes thread per project");
        // Invalid kind is rejected.
        assert!(record_fact(&conn, "/proj/r", "note", "x", None, 102).is_err());
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

    #[test]
    fn check_contradiction_surfaces_near_decision_and_floors_far() {
        use crate::embed::{vec_to_bytes, DIM};
        use crate::indexer::{source_id, upsert_thread, ParsedMessage, ParsedThread};

        let mut conn = temp_db();
        let sid = source_id(&conn, "claude_code").unwrap();
        let add =
            |conn: &mut Connection, ext: &str, text: &str, rationale: Option<&str>, vec: &[f32]| {
                upsert_thread(
                    conn,
                    sid,
                    &ParsedThread {
                        external_id: ext.into(),
                        title: Some(ext.into()),
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
                    .query_row(
                        "SELECT id FROM threads WHERE external_id = ?1",
                        [ext],
                        |r| r.get(0),
                    )
                    .unwrap();
                conn.execute(
                "INSERT INTO facts (thread_id, kind, text, rationale, status, extractor, created_at)
                 VALUES (?1, 'decision', ?2, ?3, 'open', 'llm', 1)",
                rusqlite::params![tid, text, rationale],
            )
            .unwrap();
                let fid = conn.last_insert_rowid();
                conn.execute(
                    "INSERT INTO vec_facts (fact_id, embedding) VALUES (?1, ?2)",
                    rusqlite::params![fid, vec_to_bytes(vec)],
                )
                .unwrap();
            };

        let mut near = vec![0.0_f32; DIM];
        near[0] = 1.0; // cosine 1.0 with the query
        let mut far = vec![0.0_f32; DIM];
        far[1] = 1.0; // cosine 0.0 → below the guard floor
        add(
            &mut conn,
            "near",
            "store auth tokens in the OS keychain",
            Some("never on disk"),
            &near,
        );
        add(&mut conn, "far", "use tabs not spaces", None, &far);

        let hits = check_contradiction(&conn, &near, None, 8).unwrap();
        assert_eq!(hits.len(), 1, "only the on-topic decision passes the floor");
        assert_eq!(hits[0].text, "store auth tokens in the OS keychain");
        assert_eq!(
            hits[0].rationale.as_deref(),
            Some("never on disk"),
            "the guard surfaces the decision's rationale"
        );
        assert!(hits[0].similarity >= 0.55);
    }

    #[test]
    fn recall_floors_noise_and_scopes_by_project_key() {
        use crate::embed::{vec_to_bytes, DIM};
        use crate::indexer::{source_id, upsert_thread, ParsedMessage, ParsedThread};

        let mut conn = temp_db();
        let sid = source_id(&conn, "claude_code").unwrap();
        let add = |conn: &mut rusqlite::Connection, ext: &str, text: &str, vec: &[f32]| -> i64 {
            upsert_thread(
                conn,
                sid,
                &ParsedThread {
                    external_id: ext.into(),
                    title: Some(ext.into()),
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
                .query_row(
                    "SELECT id FROM threads WHERE external_id = ?1",
                    [ext],
                    |r| r.get(0),
                )
                .unwrap();
            conn.execute(
                "INSERT INTO facts (thread_id, kind, text, status, extractor, created_at)
                 VALUES (?1, 'decision', ?2, 'open', 'llm', 1)",
                rusqlite::params![tid, text],
            )
            .unwrap();
            let fid = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO vec_facts (fact_id, embedding) VALUES (?1, ?2)",
                rusqlite::params![fid, vec_to_bytes(vec)],
            )
            .unwrap();
            tid
        };

        let mut near = vec![0.0_f32; DIM];
        near[0] = 1.0; // cosine 1.0 with the query
        let mut far = vec![0.0_f32; DIM];
        far[1] = 1.0; // cosine 0.0 with the query → below the floor
        let near_tid = add(&mut conn, "near", "use jwt for auth", &near);
        add(&mut conn, "far", "unrelated thing", &far);
        let qv = near.clone();

        // Floor: the orthogonal fact is dropped, only the genuine match returns.
        let hits = recall(&conn, &qv, "decision", None, 5).unwrap();
        assert_eq!(hits.len(), 1, "similarity floor drops the orthogonal fact");
        assert_eq!(hits[0].text, "use jwt for auth");

        // Scope by COALESCE(project_key, project_path): set project_key with a NULL project_path
        // (the old project_path-only filter would have missed this).
        conn.execute(
            "UPDATE threads SET project_key = 'myproj', project_path = NULL WHERE id = ?1",
            [near_tid],
        )
        .unwrap();
        assert_eq!(
            recall(&conn, &qv, "decision", Some("myproj"), 5)
                .unwrap()
                .len(),
            1,
            "scoped recall matches on project_key"
        );
        assert!(
            recall(&conn, &qv, "decision", Some("nope"), 5)
                .unwrap()
                .is_empty(),
            "non-matching project is excluded"
        );

        // find_prior_work guard: a query unrelated to everything returns EMPTY, not a false
        // positive; a related query still surfaces the prior session.
        let mut unrelated = vec![0.0_f32; DIM];
        unrelated[2] = 1.0;
        assert!(
            find_prior_work(&conn, &unrelated, None, 5)
                .unwrap()
                .is_empty(),
            "guard returns empty when nothing is related"
        );
        assert_eq!(
            find_prior_work(&conn, &qv, None, 5).unwrap().len(),
            1,
            "guard still surfaces a genuinely-related prior session"
        );
    }

    fn seed_thread(
        conn: &mut Connection,
        ext: &str,
        project: &str,
        subagent: bool,
        texts: &[&str],
    ) -> i64 {
        use crate::indexer::{source_id, upsert_thread, ParsedMessage, ParsedThread};
        let sid = source_id(conn, "claude_code").unwrap();
        upsert_thread(
            conn,
            sid,
            &ParsedThread {
                external_id: ext.into(),
                title: Some(ext.into()),
                project_path: Some(project.into()),
                created_at: Some(1),
                updated_at: Some(2),
                is_subagent: subagent,
                messages: texts
                    .iter()
                    .enumerate()
                    .map(|(i, t)| ParsedMessage {
                        role: if i % 2 == 0 { "user" } else { "assistant" }.into(),
                        text: (*t).into(),
                        tool_name: None,
                        ts: Some(1_000 + i as i64),
                    })
                    .collect(),
                ..Default::default()
            },
        )
        .unwrap();
        conn.query_row(
            "SELECT id FROM threads WHERE external_id = ?1",
            [ext],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn subagent_todos_stay_out_of_memory_and_backfill_uses_message_time() {
        let mut conn = temp_db();
        set_config(&conn, true, Some("ollama"), Some("m")).unwrap();
        let main = seed_thread(
            &mut conn,
            "main",
            "/proj/s",
            false,
            &["TODO: real user task"],
        );
        let sub = seed_thread(
            &mut conn,
            "sub",
            "/proj/s",
            true,
            &["TODO: delegated subtask"],
        );
        // Whatever the indexer did, a rebuild leaves subagent threads with no TODOs and
        // stamps the main thread's TODO with its message time, not index time.
        {
            let tx = conn.transaction().unwrap();
            rebuild_thread_todos(&tx, main, 9_999_999).unwrap();
            rebuild_thread_todos(&tx, sub, 9_999_999).unwrap();
            tx.commit().unwrap();
        }
        let sub_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM facts WHERE thread_id = ?1 AND extractor = 'heuristic'",
                [sub],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sub_count, 0, "no heuristic TODOs for subagent threads");
        let created: i64 = conn
            .query_row(
                "SELECT created_at FROM facts WHERE thread_id = ?1 AND kind = 'todo'",
                [main],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            created, 1_000,
            "created_at is the source message's timestamp"
        );

        // The startup purge clears TODOs an older indexer extracted from subagent threads.
        conn.execute(
            "INSERT INTO facts (thread_id, kind, text, status, extractor, created_at)
             VALUES (?1, 'todo', 'stale subagent todo', 'open', 'heuristic', 5)",
            [sub],
        )
        .unwrap();
        assert_eq!(purge_subagent_todos(&conn).unwrap(), 1);

        // Even a subagent fact that slipped in (older indexer) never reaches project memory.
        conn.execute(
            "INSERT INTO facts (thread_id, kind, text, status, extractor, created_at)
             VALUES (?1, 'gotcha', 'subagent-only gotcha', 'open', 'llm', 5)",
            [sub],
        )
        .unwrap();
        let mem = get_project_memory(&conn, "/proj/s", 40).unwrap();
        assert!(mem.open_todos.iter().any(|t| t.text == "real user task"));
        assert!(!mem.open_todos.iter().any(|t| t.text.contains("delegated")));
        assert!(mem.gotchas.is_empty(), "subagent facts excluded");
        let listed = list_open_todos(&conn, None, Some("/proj/s"), None, 50).unwrap();
        assert!(listed.iter().all(|t| t.thread_id == main));

        // A TODO the user pinned on a subagent thread (older builds extracted them) survives
        // the purge, so it must stay listed where it can be completed or unpinned.
        conn.execute(
            "INSERT INTO facts (thread_id, kind, text, status, extractor, created_at, pinned)
             VALUES (?1, 'todo', 'pinned subagent todo', 'open', 'heuristic', 6, 1)",
            [sub],
        )
        .unwrap();
        assert_eq!(purge_subagent_todos(&conn).unwrap(), 0);
        let listed = list_open_todos(&conn, None, Some("/proj/s"), None, 50).unwrap();
        assert!(listed.iter().any(|t| t.text == "pinned subagent todo"));
        let mem = get_project_memory(&conn, "/proj/s", 40).unwrap();
        assert!(mem
            .open_todos
            .iter()
            .any(|t| t.text == "pinned subagent todo" && t.pinned));
        assert!(
            mem.gotchas.is_empty(),
            "unpinned subagent facts still excluded"
        );
    }

    #[test]
    fn pending_threads_skip_unchanged_count_drift_and_auto_budget_caps() {
        let mut conn = temp_db();
        let four = ["a", "b", "c", "d"];
        let fresh = seed_thread(&mut conn, "fresh", "/proj/d", false, &four);
        let drift = seed_thread(&mut conn, "drift", "/proj/d", false, &four);
        // `drift` was distilled after its last update, but its stored count was recorded
        // under the old definition: bookkeeping drift, not new content.
        conn.execute(
            "UPDATE threads SET knowledge_extracted = 1, knowledge_extracted_at = 100,
                knowledge_msg_count = 99, updated_at = 50 WHERE id = ?1",
            [drift],
        )
        .unwrap();
        let pending = pending_threads(&conn, 100).unwrap();
        assert!(pending.contains(&fresh));
        assert!(!pending.contains(&drift), "unchanged thread is not re-sent");
        // Touched after the distill: it is re-queued.
        conn.execute("UPDATE threads SET updated_at = 200 WHERE id = ?1", [drift])
            .unwrap();
        assert!(pending_threads(&conn, 100).unwrap().contains(&drift));

        // Auto-distill: recent threads only, and a daily allowance that is reserved up front.
        let now = 1_800_000_000;
        conn.execute("UPDATE threads SET updated_at = ?1", [now - 60])
            .unwrap();
        set_auto_distill_daily_cap(&conn, 1).unwrap();
        assert_eq!(auto_distill_remaining_today(&conn, now).unwrap(), 1);
        let first = auto_distill_worklist(&conn, 25, now).unwrap();
        assert_eq!(first.len(), 1, "capped by the daily allowance");
        assert!(auto_distill_worklist(&conn, 25, now).unwrap().is_empty());
        // The next UTC day starts a fresh allowance.
        assert_eq!(
            auto_distill_remaining_today(&conn, now + 86_400).unwrap(),
            1
        );
        // Stale history is left to the explicit per-project build.
        set_auto_distill_daily_cap(&conn, 100).unwrap();
        conn.execute("UPDATE threads SET updated_at = 1", [])
            .unwrap();
        assert!(auto_distill_worklist(&conn, 25, now + 2 * 86_400)
            .unwrap()
            .is_empty());
        assert_eq!(auto_distill_daily_cap(&conn).unwrap(), 100);
    }

    #[test]
    fn keyword_prefilter_matches_project_facts() {
        let mut conn = temp_db();
        let tid = seed_thread(&mut conn, "kw", "/proj/kw", false, &["x"]);
        conn.execute(
            "INSERT INTO facts (thread_id, kind, text, status, extractor, created_at, embedded)
             VALUES (?1, 'gotcha', 'Installing the SessionStart hook rewrites settings.json', 'open', 'llm', 1, 1)",
            [tid],
        )
        .unwrap();
        let terms = prompt_terms(
            "how do we install the claude code hooks into settings json",
            6,
        );
        assert!(terms.contains(&"install".to_string()), "{terms:?}");
        assert!(
            !terms.iter().any(|t| t == "into" || t == "code"),
            "{terms:?}"
        );
        assert!(has_keyword_candidates(&conn, &terms, Some("/proj/kw")).unwrap());
        assert!(!has_keyword_candidates(&conn, &terms, Some("/proj/other")).unwrap());
        let unrelated = prompt_terms("refactor the billing invoices exporter", 6);
        assert!(!has_keyword_candidates(&conn, &unrelated, None).unwrap());
        assert!(!has_keyword_candidates(&conn, &[], None).unwrap());
    }
}
