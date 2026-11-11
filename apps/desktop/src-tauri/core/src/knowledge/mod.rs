//! Distilled knowledge layer. Slice 1 is the FREE heuristic tier: TODO/action items
//! pulled out of message text with conservative, low-noise markers (markdown unchecked
//! tasks + TODO/FIXME markers that open a line), stored in the `facts` table by the indexer
//! and surfaced via `list_open_todos` (desktop, `cal todos`, MCP). No model, no API key.
//! The LLM tier (decisions / gotchas / summaries, lazy on-demand) reuses the same table, as
//! do facts recorded by the user or by agents (`record_fact_with_origin`).

pub mod app_config;
// Recall and check_decision without an answer: no model, or an undistilled scope.
pub mod fallback;
pub mod grounding;
pub mod harvest;
pub mod identity;
pub mod priority;
pub mod repo_memory;
pub mod truth;
// SessionStart memory: facts naming the files the latest sessions edited go first.
pub mod working_set;

pub use working_set::get_project_memory_for_working_set;

pub use fallback::{
    check_or_fallback, recall_or_fallback, ClosestSession, DistillCoverage, RecallOutcome,
};
pub use identity::{
    local_keys_for_portable, new_fact_uid, portable_project_id, rebind_fact_sources,
    refresh_portable_project_id, IMPORT_FACT_SQL,
};
pub use priority::{distill_priority_ready, enqueue_distill_priority};
pub use truth::{
    agent_supersedes_recent, distill_memory_context, fact_date, fact_in_project, fact_truth,
    flag_fact, has_truth_tables, is_session_extractor, link_facts, resolve_fact_flags,
    restore_fact, unlink_facts, unlink_link, FactFlag, FactStatus, FactTruth, FlagKind,
    FlagOutcome, LinkKind, LinkOutcome, LinkRejection, LinkSource, LinkedFact, RefEntry, RefMap,
    SignalSource, AGENT_FLAG_CAP, AGENT_SUPERSEDE_CAP, AGENT_SUPERSEDE_WINDOW_SECS, DECIDED_AT_SQL,
};

use crate::agent::Distilled;
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

/// Hard cap on a single extracted TODO so one runaway line can't bloat the list.
const MAX_TODO_LEN: usize = 240;

/// `facts.extractor` of a TODO taken from a deferred-work list in an assistant message
/// ([`extract_followups`]): only a thread's latest such list, and only one from the last 30
/// days ([`TodoDeriver`]). Re-derived from message text on every index like a `heuristic`
/// one, and curated the same way (done / pinned / hidden / edited rows are kept).
pub const EXTRACTOR_FOLLOWUP: &str = "followup";

/// The extractors whose TODOs the indexer re-derives from message text, as a SQL list. Every
/// re-derive, its curated-row seed and the one-time backfills read both.
pub(crate) const DERIVED_TODO_EXTRACTORS_SQL: &str = "('heuristic', 'followup')";

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
///
/// This is the user-message rule; an assistant message goes through [`message_todos`],
/// which also gates its checklists.
pub fn extract_todos(text: &str) -> Vec<String> {
    scan_todos(text, false)
}

/// The TODOs one message yields, each with the extractor it is stored under: `heuristic`
/// ([`extract_todos`]'s rule) and, in an assistant message, [`EXTRACTOR_FOLLOWUP`]. An
/// assistant's `- [ ]` item must name a specific ([`names_a_specific`]): on the measured index
/// 116 of 129 open TODOs came from assistant messages, and the 76 this gate rejects were
/// how-to checklists written for the user (posting steps, profile fields, recording tips), not
/// this project's work. A TODO / FIXME marker is kept as it is, and so is a user's checklist:
/// the user wrote it as their own list.
pub(crate) fn message_todos(text: &str, assistant: bool) -> Vec<(String, &'static str)> {
    let mut out: Vec<(String, &'static str)> = scan_todos(text, assistant)
        .into_iter()
        .map(|t| (t, "heuristic"))
        .collect();
    if assistant {
        out.extend(
            extract_followups(text)
                .into_iter()
                .map(|t| (t, EXTRACTOR_FOLLOWUP)),
        );
    }
    out
}

/// Whether a TODO names a file, symbol or identifier (`grounding::keep_tokens` finds one, or
/// a word is a ticket id such as `LIN-482` or an issue number such as `#482`, which name this
/// project's work as precisely as a file and which grounding has no rule for): a checklist
/// line that names none is a how-to step, not this project's work.
fn names_a_specific(text: &str) -> bool {
    !grounding::keep_tokens(text).is_empty()
        || text.split_whitespace().any(|w| {
            let w = w.trim_matches(|c: char| !c.is_alphanumeric() && c != '#');
            let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
            if let Some(n) = w.strip_prefix('#') {
                return digits(n);
            }
            // An uppercase letter, then uppercase letters or digits, a dash and digits: not
            // `v1-2` or `utf-8`.
            w.split_once('-').is_some_and(|(key, n)| {
                key.len() >= 2
                    && key.starts_with(|c: char| c.is_ascii_uppercase())
                    && key
                        .bytes()
                        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
                    && digits(n)
            })
        })
}

/// [`extract_todos`], with `gate_tasks` dropping a `- [ ]` item that names no specific.
fn scan_todos(text: &str, gate_tasks: bool) -> Vec<String> {
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
        let todo = match unchecked_task(line) {
            Some(task) => clean(task).filter(|t| !gate_tasks || names_a_specific(t)),
            None => leading_marker(line).and_then(clean),
        };
        if let Some(todo) = todo {
            out.push(todo);
        }
    }
    out
}

/// Most items one message's deferred-work lists yield.
const MAX_FOLLOWUPS_PER_MESSAGE: usize = 12;
/// A heading, bold line or `...:` line longer than this is prose, not a section title.
const MAX_FOLLOWUP_TITLE: usize = 60;
/// Prose lines a deferred-work heading may have before its first bullet ("These are out of
/// scope for this PR:").
const MAX_FOLLOWUP_INTRO: usize = 2;

/// Section titles that introduce deferred work, matched at the start of the title (an `s`
/// may follow: "Follow-ups"). Not "Next steps", even for bullets that name a specific: on the
/// measured index its 55 such bullets were mostly setup instructions for the user (run the
/// migration, regenerate the types, configure a dashboard), about 23 of a 30-item sample.
const FOLLOWUP_TITLES: &[&str] = &[
    "known follow-up",
    "known follow up",
    "follow-up",
    "follow up",
    "followup",
    "not done",
    "not in this pr",
    "deferred",
    "left to do",
    "remaining",
];

/// Words that, anywhere after a deferred-work phrase, say its list is work already done
/// ("Follow-ups, all merged", "Remaining: none, all done"), unless a negation comes first
/// ("Follow-ups that never landed"). A whole-title match instead ("Remaining", "Follow-ups"
/// and nothing after) would keep about 27 of the measured index's 295 items: most of its 79
/// titles add a qualifier ("Remaining cleanup (by risk)", "Deferred, needs a decision"), over
/// deferred work all the same. This rule rejects its one list of completed work (3 items).
const DONE_WORDS: &[&str] = &[
    "shipped",
    "done",
    "fixed",
    "landed",
    "merged",
    "applied",
    "completed",
    "finished",
    "resolved",
    "addressed",
    "implemented",
    "delivered",
];

/// What a singular "follow-up" names as made when it is an adjective: "Follow-up fixes:" over
/// the fixes a review asked for are done, unlike "Follow-ups" or "Remaining fixes", and so is
/// "Follow-up commit `abc1234`:".
const MADE_WORDS: &[&str] = &["fix", "fixes", "change", "changes", "commit", "commits"];

/// A first word after the phrase that names something other than work: "Remaining risks" and
/// "Follow-up questions" list what may go wrong or what to ask, not what is left to do.
const NOT_WORK_WORDS: &[&str] = &["risk", "risks", "question", "questions", "note", "notes"];

/// "none" or "nothing" says nothing is left when it ends the title or one of these words
/// follows it ("Remaining work is none", "Follow-ups: none needed", "nothing left to do"), and
/// not when it only qualifies the items ("Follow-ups (none blocking)", "Remaining (nothing
/// urgent)"), whose list is deferred work all the same.
const NOTHING_LEFT: &[&str] = &[
    "left",
    "remaining",
    "needed",
    "pending",
    "outstanding",
    "else",
    "to",
];

/// Work an agent said it left for later: the bullets under a heading, a bold line or a short
/// `...:` line titled like "Follow-ups", "Known follow-ups, not in this PR", "Not done",
/// "Deferred", "Remaining" or "Left to do", at most [`MAX_FOLLOWUPS_PER_MESSAGE`] per message.
/// On the measured index (1,789 top-level sessions) such lists held 292 items and none of them
/// reached the TODO list, which reads only `- [ ]` tasks and TODO markers. A list ends at the
/// next heading or at an unindented line that is not a bullet, and a title that says its list
/// is done ([`DONE_WORDS`]) opens none; nested bullets are details of their parent and are
/// skipped (but a bullet ending in `:` is a label, not an item, and the bullets nested under
/// it are read as items), as are checked (`[x]`) and struck (`~~`) items. Fenced
/// code and injected context are never read, as in [`extract_todos`].
pub fn extract_followups(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if is_injected_context(text) {
        return out;
    }
    // The open list: the indent of its bullets (set by the first one), how many prose lines
    // came before that, and the indent of the last item read, deeper bullets being its
    // details (None after a label, whose nested bullets are read).
    struct List {
        indent: Option<usize>,
        intro: usize,
        item: Option<usize>,
    }
    let mut list: Option<List> = None;
    let mut in_fence = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            in_fence = !in_fence;
            list = None;
            continue;
        }
        if in_fence || line.is_empty() {
            continue;
        }
        if followup_title(line).is_some() {
            list = Some(List {
                indent: None,
                intro: 0,
                item: None,
            });
            continue;
        }
        let Some(open) = list.as_mut() else {
            continue;
        };
        if line.starts_with('#') {
            list = None;
            continue;
        }
        let indent = raw.len() - raw.trim_start().len();
        let Some(item) = bullet_text(line) else {
            match open.indent {
                // A wrapped bullet continues under its parent.
                Some(i) if indent > i => {}
                Some(_) => list = None,
                None => {
                    open.intro += 1;
                    if open.intro > MAX_FOLLOWUP_INTRO {
                        list = None;
                    }
                }
            }
            continue;
        };
        open.indent.get_or_insert(indent);
        // Under an item its details, and theirs.
        if open.item.is_some_and(|at| indent > at) {
            continue;
        }
        open.item = Some(indent);
        let item = match item.strip_prefix("[ ]").or_else(|| item.strip_prefix("[]")) {
            Some(rest) => rest.trim_start(),
            None if item.starts_with("[x]") || item.starts_with("[X]") => continue,
            None => item,
        };
        if item.starts_with("~~") {
            continue;
        }
        let Some(todo) = clean(item) else { continue };
        // A bullet ending in `:` labels the items nested under it ("**Three questions are
        // still open:**"), at any depth ("- Backend:" under it): on a measured index 5 of 24
        // follow-ups were such labels, their real items skipped as details.
        if todo.trim_end_matches(['*', '_']).ends_with(':') {
            open.item = None;
            continue;
        }
        if !out.iter().any(|o| o.eq_ignore_ascii_case(&todo)) {
            out.push(todo);
        }
        if out.len() >= MAX_FOLLOWUPS_PER_MESSAGE {
            break;
        }
    }
    out
}

/// The text after a list bullet (`- `, `* `, `+ `, `• `, `1. `, `2) `), else None.
fn bullet_text(line: &str) -> Option<&str> {
    for p in ["- ", "* ", "+ ", "\u{2022} "] {
        if let Some(rest) = line.strip_prefix(p) {
            return Some(rest.trim_start());
        }
    }
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if (1..=3).contains(&digits) {
        let rest = &line[digits..];
        if let Some(rest) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return Some(rest.trim_start());
        }
    }
    None
}

/// Whether `line` titles a deferred-work list ([`FOLLOWUP_TITLES`]): a markdown heading, a
/// line in bold, or a short line ending in `:`, whose words after the phrase do not say the
/// work is done ([`DONE_WORDS`], [`MADE_WORDS`], [`NOTHING_LEFT`]) or name something else
/// ([`NOT_WORK_WORDS`]). Some(the phrase it matched), else None.
fn followup_title(line: &str) -> Option<&'static str> {
    let title = if let Some(h) = line.strip_prefix('#') {
        h.trim_start_matches('#').trim()
    } else if let Some(inner) = ["**", "__"]
        .iter()
        .find_map(|m| line.strip_prefix(m).and_then(|r| r.split_once(m)))
    {
        // `**Follow-ups:**` or `**Follow-ups**:`, and nothing after it on the line.
        let (inner, after) = inner;
        if !after.trim().trim_start_matches(':').is_empty() {
            return None;
        }
        inner.trim()
    } else if let Some(t) = line.strip_suffix(':') {
        t.trim()
    } else {
        return None;
    };
    let title = title
        .trim_matches(|c: char| matches!(c, '*' | '_' | ':' | ' '))
        .trim_start_matches(|c: char| c.is_ascii_digit() || matches!(c, '.' | ')' | ' '));
    if title.chars().count() > MAX_FOLLOWUP_TITLE {
        return None;
    }
    let lower = title.to_lowercase();
    let (phrase, plural, rest) = FOLLOWUP_TITLES.iter().copied().find_map(|phrase| {
        let rest = lower.strip_prefix(phrase)?;
        let (plural, rest) = match rest.strip_prefix('s') {
            Some(r) => (true, r),
            None => (false, rest),
        };
        rest.chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric())
            .then_some((phrase, plural, rest))
    })?;
    let words: Vec<&str> = rest
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let done = words.iter().enumerate().any(|(i, w)| {
        let said = DONE_WORDS.contains(w)
            || (matches!(*w, "none" | "nothing")
                && words.get(i + 1).is_none_or(|n| NOTHING_LEFT.contains(n)));
        said && (i == 0 || !matches!(words[i - 1], "not" | "no" | "never"))
    });
    let made = !plural
        && phrase.contains("follow")
        && words.first().is_some_and(|w| MADE_WORDS.contains(w));
    let not_work = words.first().is_some_and(|w| NOT_WORK_WORDS.contains(w));
    (!done && !made && !not_work).then_some(phrase)
}

/// Message text that an agent harness injected rather than the user or model writing it:
/// skill bodies, slash-command expansions, system reminders, and Callimachus's own
/// injected memory / packed threads / distiller memory list / compaction anchor (mining
/// those would feed TODOs and prompts back to us: the anchor quotes the user's own prompts,
/// so a session that pasted one back would replay them as new ones).
pub(crate) fn is_injected_context(text: &str) -> bool {
    let t = text.trim_start();
    const PREFIXES: [&str; 10] = [
        "Base directory for this skill",
        "<command-name>",
        "<command-message>",
        "<system-reminder>",
        "<local-command-stdout>",
        "Caveat: The messages below were generated",
        "<reference_thread",
        "<project_memory",
        "<existing_memory",
        "<session_prompts",
    ];
    PREFIXES.iter().any(|p| t.starts_with(p))
        || t.starts_with(crate::anchor::ANCHOR_HEADING)
        || t.contains(crate::export::MANAGED_START)
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

/// Cap on heuristic todos kept per thread. Follow-ups have no per-thread cap: only a thread's
/// latest deferred-work list yields them (see [`TodoDeriver`]), at most
/// [`MAX_FOLLOWUPS_PER_MESSAGE`] items.
const MAX_TODOS_PER_THREAD: usize = 25;
/// A deferred-work list older than this at derive time yields no follow-ups: a "Remaining"
/// list from months ago says nothing about today, and nothing closes a follow-up but the
/// user. On a measured index only 23 of about 300 follow-ups were from the last 30 days, and
/// without this limit the open list would have grown from 128 to about 350 on upgrade, nine
/// tenths of the new rows older than a month.
const FOLLOWUP_MAX_AGE_SECS: i64 = 30 * 24 * 60 * 60;

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
        &format!(
            "DELETE FROM facts WHERE extractor IN {DERIVED_TODO_EXTRACTORS_SQL} AND kind = 'todo'
                AND status = 'open' AND pinned = 0 AND hidden = 0
                AND thread_id IN (SELECT id FROM threads WHERE is_subagent = 1)"
        ),
        [],
    )?)
}

/// Re-derive a single thread's heuristic and follow-up todos from its already-stored
/// messages, inside the caller's transaction. Shared by the indexer and the backfill.
fn rebuild_thread_todos(tx: &Connection, thread_id: i64, now: i64) -> Result<()> {
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
    // Keep CURATED todos (done / dismissed / pinned) so closing a TODO survives re-index;
    // only the open, untouched ones are re-derived, under the uid each had before.
    let mut derive = TodoDeriver::start(tx, thread_id, true, now)?;
    if is_subagent {
        return Ok(());
    }
    let mut sel = tx.prepare(
        "SELECT id, role, text, ts, seq FROM messages
         WHERE thread_id = ?1 AND role IN ('user', 'assistant')
         ORDER BY seq, id",
    )?;
    let rows: Vec<(i64, String, String, Option<i64>, i64)> = sel
        .query_map([thread_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    // Every message is read: a later deferred-work list replaces the earlier ones.
    for (mid, role, text, ts, seq) in rows {
        derive.message(mid, role == "assistant", &text, ts, seq)?;
    }
    Ok(())
}

/// The insert for one derived TODO: (thread, text, source message, created_at, source seq,
/// uid, extractor). Shared by the indexer and the backfills.
pub(crate) const INSERT_DERIVED_TODO_SQL: &str = "INSERT INTO facts
        (thread_id, kind, text, source_message_id, status, extractor, created_at, source_seq, uid)
     VALUES (?1, 'todo', ?2, ?3, 'open', ?7, ?4, ?5, ?6)";

/// One thread's TODO re-derive, message by message, shared by the indexer (a full replace or
/// an appended tail) and the backfills so they keep one rule: the curated rows it must not
/// duplicate, the uid a TODO found again keeps, the messages whose TODO the user reworded,
/// the heuristic cap, and these rules for follow-ups:
///
/// - **The latest list wins.** A thread's follow-ups are the items of its latest assistant
///   message (in seq order) with a deferred-work list: such a list first removes the
///   thread's open, uncurated follow-ups from earlier messages (on an append they are already
///   in the table), then adds its own items. On a measured index 87 of the 90 messages
///   holding a list were status reports from the middle of a session (a median of 9 user
///   turns followed), a quarter of the items in a second or later list repeated an earlier
///   one in other words, and the old per-thread cap of 25 cut the newest list, the only one
///   that still said what was left. The cost: in a session that reports on several pieces of
///   work one after another, an earlier piece's deferred items leave the list when a later
///   report does not repeat them; they stay in the transcript and in search.
/// - **Identity.** An item the list just removed keeps its uid when that list repeats it. One
///   that leaves a list and comes back in a later one is a new fact, on an append and on a
///   one-pass derive alike: the uid of a removed row outlives no deriver, so carrying it
///   across a gap would make the uid depend on how the messages were batched. A full
///   re-derive keeps the uid of every open row it takes.
/// - **What stays.** A curated follow-up (done, hidden, pinned or edited) is never removed and
///   still blocks an open copy of its text. Heuristic TODOs keep accumulating across messages:
///   one whose text an open, uncurated follow-up holds takes that row over
///   ([`Self::take_over_followup`]), so a later list cannot remove it.
/// - **A hand-curated thread is left alone.** In a thread with a derived TODO the user reworded
///   ([`hand_curated`]), a derive adds no follow-up and removes none: a full re-derive takes
///   only its open, uncurated heuristic rows, its lists are not read, and the age limit
///   removes nothing there. `edit_fact` overwrites the original wording, so a derive knows the
///   reworded item only by its message's seq (which a full replace can renumber), and each
///   rule that guessed which item of a list was the original from the rows that came and went
///   during the derive had a hole, one found in each of three reviews (the siblings blocked
///   for good, the original back on an append, the original back through a row the same
///   derive added and removed). Its heuristic TODOs derive as before follow-ups existed
///   ([`edited_todo_seqs`]). The cost: from the moment a TODO of a session is reworded, later
///   deferred-work lists of that session are not added and its open follow-ups stay until
///   the user closes or hides them; and an open checklist TODO an older build took from one
///   of its deferred-work lists, which the checklist gate now rejects, goes on the re-judge or
///   a full re-derive without coming back as a follow-up, as it would in another thread.
/// - **An age limit.** A list in a message older than [`FOLLOWUP_MAX_AGE_SECS`] at derive
///   time (the message's own time, normalized as [`todo_created_at`] does; a message without
///   one counts as new) is ignored: it adds no follow-up and removes none. So the one-time
///   re-judge adds only the last 30 days' follow-ups, and a later full re-index drops the
///   open, uncurated ones that have aged past the limit (pin, edit or close one to keep it).
pub(crate) struct TodoDeriver<'c> {
    tx: &'c Connection,
    ins: rusqlite::Statement<'c>,
    thread_id: i64,
    now: i64,
    /// The uids of the open rows a full re-derive took, by lowercased text.
    uids: std::collections::HashMap<String, String>,
    edited: std::collections::HashSet<i64>,
    /// [`hand_curated`]: the thread's follow-ups stay as they are.
    frozen: bool,
    seen: std::collections::HashSet<String>,
    heuristic: usize,
}

impl<'c> TodoDeriver<'c> {
    /// `replace`: take the thread's open, uncurated derived TODOs first (a full re-derive; only
    /// the heuristic ones in a hand-curated thread); else keep every row and count the open
    /// heuristic ones toward their cap (an appended tail).
    pub(crate) fn start(
        tx: &'c Connection,
        thread_id: i64,
        replace: bool,
        now: i64,
    ) -> Result<Self> {
        let frozen = hand_curated(tx, thread_id)?;
        let (uids, edited) = if replace {
            (
                take_open_derived_todos(tx, thread_id, frozen)?,
                edited_todo_seqs(tx, thread_id)?,
            )
        } else {
            Default::default()
        };
        // Seed with the rows that stay (curated ones on a replace, all of them on an append)
        // so a closed TODO never comes back as an open copy, under either extractor.
        let mut seen = std::collections::HashSet::new();
        let mut heuristic = 0;
        let mut kept = tx.prepare(&format!(
            "SELECT text, extractor, status FROM facts
             WHERE thread_id = ?1 AND extractor IN {DERIVED_TODO_EXTRACTORS_SQL}"
        ))?;
        let rows = kept.query_map([thread_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for (text, extractor, status) in rows.flatten() {
            seen.insert(text.to_ascii_lowercase());
            // The cap is a per-thread TOTAL of open rows: on an append the existing ones were
            // not deleted, so they still count toward it.
            if !replace && status == "open" && extractor != EXTRACTOR_FOLLOWUP {
                heuristic += 1;
            }
        }
        Ok(TodoDeriver {
            tx,
            ins: tx.prepare(INSERT_DERIVED_TODO_SQL)?,
            thread_id,
            now,
            uids,
            edited,
            frozen,
            seen,
            heuristic,
        })
    }

    /// The re-judge's mode, over a thread's messages already indexed: take follow-ups only
    /// (what the new extractor finds, and no heuristic TODO a re-index would bring back one
    /// past an old cap). A hand-curated thread gets none, as on every derive.
    pub(crate) fn rejudging(mut self) -> Self {
        self.heuristic = MAX_TODOS_PER_THREAD;
        self
    }

    /// Insert the TODOs message `message_id` (at `seq`, written at `ts`) yields.
    pub(crate) fn message(
        &mut self,
        message_id: i64,
        assistant: bool,
        text: &str,
        ts: Option<i64>,
        seq: i64,
    ) -> Result<()> {
        let todos = message_todos(text, assistant);
        // Stamped with the source message's time, not index time, so a re-index of old
        // history doesn't make old TODOs look new. The same time judges a list's age.
        let created_at = todo_created_at(ts, self.now);
        // Only a recent list outside a hand-curated thread yields or removes follow-ups.
        let listed = !self.frozen
            && self.now - created_at <= FOLLOWUP_MAX_AGE_SECS
            && todos.iter().any(|(_, e)| *e == EXTRACTOR_FOLLOWUP);
        let dropped = if listed {
            self.drop_earlier_followups(seq)?
        } else {
            Default::default()
        };
        for (todo, extractor) in todos {
            let followup = extractor == EXTRACTOR_FOLLOWUP;
            if (followup && !listed) || (!followup && self.heuristic >= MAX_TODOS_PER_THREAD) {
                continue;
            }
            let lower = todo.to_ascii_lowercase();
            if self.edited.contains(&seq) && !self.uids.contains_key(&lower) {
                // The user reworded the TODO this message raised (see `edited_todo_seqs`).
                self.seen.insert(lower);
                continue;
            }
            if self.seen.insert(lower.clone()) {
                // The uid its open row had when a full re-derive took it or this list removed
                // it, else a new fact (see **Identity**).
                let uid = self
                    .uids
                    .get(&lower)
                    .or_else(|| dropped.get(&lower))
                    .cloned()
                    .unwrap_or_else(new_fact_uid);
                self.ins.execute(rusqlite::params![
                    self.thread_id,
                    todo,
                    message_id,
                    created_at,
                    seq,
                    uid,
                    extractor
                ])?;
                if !followup {
                    self.heuristic += 1;
                }
            } else if !followup
                && !self.frozen
                && self.take_over_followup(&lower, message_id, created_at, seq)?
            {
                self.heuristic += 1;
            }
        }
        Ok(())
    }

    /// A heuristic TODO (a TODO marker, the user's own checklist line) whose text an open,
    /// uncurated follow-up holds takes that row over, uid and all, as the row the heuristic
    /// would have been: heuristic TODOs accumulate, and without this the seen-dedupe skipped
    /// the marker, so a later list that removed the follow-up took the marker's TODO with it.
    /// A curated row still blocks it, and a hand-curated thread needs none: no list removes
    /// its follow-ups. Returns whether a row was taken over.
    fn take_over_followup(
        &mut self,
        lower: &str,
        message_id: i64,
        created_at: i64,
        seq: i64,
    ) -> Result<bool> {
        let tx = self.tx;
        let open: Vec<(i64, String)> = tx
            .prepare_cached(&format!(
                "SELECT id, text FROM facts
                 WHERE {OPEN_UNCURATED_DERIVED_SQL} AND extractor = '{EXTRACTOR_FOLLOWUP}'"
            ))?
            .query_map([self.thread_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        // Matched in Rust: `seen` lowercases ASCII only, SQLite's `lower()` may do more.
        let Some((id, _)) = open
            .into_iter()
            .find(|(_, text)| text.to_ascii_lowercase() == lower)
        else {
            return Ok(false);
        };
        tx.prepare_cached(
            "UPDATE facts SET extractor = 'heuristic', source_message_id = ?2, source_seq = ?3,
                              created_at = ?4
             WHERE id = ?1",
        )?
        .execute(params![id, message_id, seq, created_at])?;
        Ok(true)
    }

    /// The latest list wins, for a list at `seq`: delete this thread's open, uncurated
    /// follow-ups from earlier messages and let the list add their text again unless a row
    /// that stays holds it. Returns their uids by lowercased text, for an item the list
    /// repeats.
    fn drop_earlier_followups(
        &mut self,
        seq: i64,
    ) -> Result<std::collections::HashMap<String, String>> {
        let tx = self.tx;
        let earlier = format!(
            "{OPEN_UNCURATED_DERIVED_SQL} AND extractor = '{EXTRACTOR_FOLLOWUP}'
             AND COALESCE(source_seq, -1) < ?2"
        );
        let gone: Vec<(String, Option<String>)> = tx
            .prepare_cached(&format!("SELECT text, uid FROM facts WHERE {earlier}"))?
            .query_map(params![self.thread_id, seq], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut dropped = std::collections::HashMap::new();
        if !gone.is_empty() {
            tx.prepare_cached(&format!("DELETE FROM facts WHERE {earlier}"))?
                .execute(params![self.thread_id, seq])?;
            let left: std::collections::HashSet<String> = tx
                .prepare_cached(&format!(
                    "SELECT text FROM facts
                     WHERE thread_id = ?1 AND extractor IN {DERIVED_TODO_EXTRACTORS_SQL}"
                ))?
                .query_map([self.thread_id], |r| {
                    r.get::<_, String>(0).map(|t| t.to_ascii_lowercase())
                })?
                .collect::<rusqlite::Result<_>>()?;
            for (text, uid) in gone {
                let lower = text.to_ascii_lowercase();
                if !left.contains(&lower) {
                    self.seen.remove(&lower);
                }
                if let Some(uid) = uid {
                    dropped.insert(lower, uid);
                }
            }
        }
        Ok(dropped)
    }
}

/// The predicate of a derived TODO a re-derive may replace: open and never curated (not
/// done, hidden, pinned or reworded by the user). Over `facts` with ?1 the thread.
const OPEN_UNCURATED_DERIVED_SQL: &str = "thread_id = ?1 AND kind = 'todo'
        AND extractor IN ('heuristic', 'followup')
        AND status = 'open' AND hidden = 0 AND pinned = 0 AND edited = 0";

/// Delete this thread's open, uncurated derived TODOs (the ones a re-derive replaces; only
/// the heuristic ones with `heuristic_only`, for a hand-curated thread) and return their uids
/// by lowercased text, so the same TODO found again keeps its identity instead of turning
/// into a new fact on every full re-index. A TODO the user reworded (`edit_fact`) is curated:
/// it stays, under its uid, like a done or pinned one.
pub(crate) fn take_open_derived_todos(
    tx: &Connection,
    thread_id: i64,
    heuristic_only: bool,
) -> Result<std::collections::HashMap<String, String>> {
    let only = if heuristic_only {
        " AND extractor = 'heuristic'"
    } else {
        ""
    };
    let mut sel = tx.prepare(&format!(
        "SELECT text, uid FROM facts WHERE {OPEN_UNCURATED_DERIVED_SQL}{only} AND uid IS NOT NULL"
    ))?;
    let uids = sel
        .query_map([thread_id], |r| {
            Ok((r.get::<_, String>(0)?.to_ascii_lowercase(), r.get(1)?))
        })?
        .collect::<rusqlite::Result<std::collections::HashMap<_, _>>>()?;
    tx.execute(
        &format!("DELETE FROM facts WHERE {OPEN_UNCURATED_DERIVED_SQL}{only}"),
        [thread_id],
    )?;
    Ok(uids)
}

/// Whether the user reworded one of this thread's derived TODOs (`edited = 1`, whatever its
/// status, pin or hidden flag): a hand-curated thread, whose follow-ups a derive leaves as
/// they are ([`TodoDeriver`]).
fn hand_curated(tx: &Connection, thread_id: i64) -> Result<bool> {
    Ok(tx
        .prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM facts WHERE thread_id = ?1 AND kind = 'todo'
                AND extractor IN ('heuristic', 'followup') AND edited = 1)",
        )?
        .query_row([thread_id], |r| r.get(0))?)
}

/// The messages (by `source_seq`) whose derived TODO the user reworded. Its original text
/// is gone (`edit_fact` overwrites it), so a re-derive cannot match it by text: it skips the
/// heuristic TODOs it finds again in those messages that no open fact it just took claims,
/// or the original would come back as a second, open copy beside the edited one (no
/// follow-up is derived in such a thread: [`hand_curated`]). The seq can go stale: a full
/// replace that renumbers a thread's messages leaves it where it was, since
/// `rebind_fact_sources` finds a fact by its text, which the edit replaced. That is older
/// than follow-ups and unchanged for heuristic TODOs.
pub(crate) fn edited_todo_seqs(
    tx: &Connection,
    thread_id: i64,
) -> Result<std::collections::HashSet<i64>> {
    let mut stmt = tx.prepare_cached(
        "SELECT source_seq FROM facts WHERE thread_id = ?1
            AND extractor IN ('heuristic', 'followup')
            AND edited = 1 AND source_seq IS NOT NULL",
    )?;
    let seqs = stmt
        .query_map([thread_id], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(seqs)
}

/// Backfill heuristic todos across the whole corpus from already-indexed message text
/// (no file reading), so todos appear without a full re-index. Runs in BATCHES, taking the
/// DB lock briefly per chunk of threads so the UI stays responsive, never one long lock
/// hold: a batch ends after [`BACKFILL_BATCH`] threads or [`BACKFILL_BATCH_TIME`], whichever
/// comes first, since one huge thread can hold many thousands of messages. Before each batch
/// it waits while `pause()` says another writer job is running, the way auto-distill yields
/// to a reindex or an embed build. Safe to run on a background thread. Each thread's rebuild
/// replaces only its open, uncurated todos, so done / pinned / dismissed ones survive (an
/// older build wiped every heuristic row up front, which was harmless only while turning
/// knowledge off had already cleared them).
pub fn backfill_todos(db: &crate::db::Db, now: i64, pause: &dyn Fn() -> bool) -> Result<()> {
    let lock = || crate::db::lock_conn(&db.0);

    let thread_ids: Vec<i64> = {
        let conn = lock();
        // Subagent threads get no heuristic TODOs (see rebuild_thread_todos).
        let mut stmt = conn.prepare("SELECT id FROM threads WHERE is_subagent = 0 ORDER BY id")?;
        let ids: Vec<i64> = stmt
            .query_map([], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<_>>()?;
        ids
    };

    let mut next = 0;
    while next < thread_ids.len() {
        while pause() {
            std::thread::sleep(BACKFILL_PAUSE_POLL);
        }
        let mut conn = lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let started = std::time::Instant::now();
        let end = (next + BACKFILL_BATCH).min(thread_ids.len());
        while next < end {
            rebuild_thread_todos(&tx, thread_ids[next], now)?;
            next += 1;
            if started.elapsed() >= BACKFILL_BATCH_TIME {
                break;
            }
        }
        tx.commit()?;
    }
    Ok(())
}

/// Most threads per backfill batch.
const BACKFILL_BATCH: usize = 50;
/// A backfill batch commits and lets go of the GUI connection once it has run this long, so
/// star, tag and Remember writes queued behind it wait at most about this.
const BACKFILL_BATCH_TIME: std::time::Duration = std::time::Duration::from_millis(200);
/// How often a paused backfill checks whether the writer job it yields to is done.
const BACKFILL_PAUSE_POLL: std::time::Duration = std::time::Duration::from_secs(2);

/// Set once the one-time TODO backfill has run (or was not needed).
const TODOS_BACKFILLED: &str = "knowledge.todos_backfilled";

/// Mark the one-time backfills as not needed: an index with no threads yet gets every TODO at
/// index time, since TODO extraction no longer depends on `knowledge.enabled`, and under
/// today's rules, so the re-judge ([`regate_todos_once`]) has nothing to do either.
pub fn mark_todos_backfilled(conn: &Connection) -> Result<()> {
    app_config::put_flag(conn, TODOS_BACKFILLED, true)?;
    app_config::put(conn, TODOS_REGATED, REGATE_DONE)
}

/// The one-time TODO backfill for indexes from before TODOs were always on. Until then the
/// indexer extracted TODOs only while `knowledge.enabled` was set, and turning it off wiped
/// them, so an index that never had it on holds none. Runs [`backfill_todos`] when
/// `knowledge.todos_backfilled` is unset and sets it after a full pass (an interrupted pass
/// runs again next launch; each thread's rebuild is idempotent). It runs whatever
/// `knowledge.enabled` was: the old on-enable backfill was a fire-and-forget thread with no
/// completion record, so an app quit in the middle of it left later threads without TODOs.
/// Returns whether the backfill ran.
pub fn backfill_todos_once(db: &crate::db::Db, now: i64, pause: &dyn Fn() -> bool) -> Result<bool> {
    let lock = || crate::db::lock_conn(&db.0);
    if app_config::get_flag(&lock(), TODOS_BACKFILLED)? {
        return Ok(false);
    }
    backfill_todos(db, now, pause)?;
    let conn = lock();
    app_config::put_flag(&conn, TODOS_BACKFILLED, true)?;
    // The full rebuild just applied today's rules, the checklist gate and follow-ups included.
    app_config::put(&conn, TODOS_REGATED, REGATE_DONE)?;
    Ok(true)
}

/// app_config: progress of the one-time TODO re-judge ([`regate_todos_once`]): the id of the
/// last thread it finished (it walks them in id order), then [`REGATE_DONE`].
pub(crate) const TODOS_REGATED: &str = "knowledge.todos_regated";
const REGATE_DONE: &str = "done";

/// What a [`regate_todos_once`] pass did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RegateReport {
    /// Threads it walked (this run; a resumed run starts after the last one it finished).
    pub threads: usize,
    /// Open heuristic TODOs it deleted because the checklist gate now rejects them (one that
    /// another message still yields moves there instead).
    pub dropped: usize,
    /// Follow-up TODOs it added from the threads' deferred-work lists, net of the open ones
    /// a later list replaced.
    pub added: usize,
}

/// The one-time re-judge of the TODOs indexed before the assistant checklist gate and
/// [`extract_followups`]: for every top-level thread, in id order, it deletes the open
/// heuristic TODOs the gate now rejects ([`regate_drops`]; one another message still yields
/// moves there) and adds the thread's follow-ups under [`TodoDeriver`]'s rules (its latest
/// deferred-work list only, and only from the last 30 days, so an earlier list's open,
/// uncurated follow-ups go too; none in a hand-curated thread). Only those rows change:
/// a TODO that is done, pinned, hidden or edited, one a user or agent recorded, one from any
/// other extractor, and one whose source message cannot be found are never deleted, and no
/// other heuristic TODO is added. Batched like [`backfill_todos`] (at most [`BACKFILL_BATCH`]
/// threads or [`BACKFILL_BATCH_TIME`] per IMMEDIATE transaction, waiting while `pause()` says
/// a writer job runs); each batch records the last thread it finished under [`TODOS_REGATED`]
/// in the same transaction, so an interrupted pass resumes there and a finished one never
/// runs again. Skipped (None, nothing recorded) on an index
/// without the columns it reads, and when it already ran. For the watcher only (the app's or
/// `cal watch`'s, after the launch catch-up): never a hook or the MCP server, which must not
/// write the index behind the user's back.
pub fn regate_todos_once(
    db: &crate::db::Db,
    now: i64,
    pause: &dyn Fn() -> bool,
) -> Result<Option<RegateReport>> {
    let lock = || crate::db::lock_conn(&db.0);
    let (mut after, thread_ids) = {
        let conn = lock();
        if !regate_columns_present(&conn) {
            return Ok(None);
        }
        let after = match app_config::get(&conn, TODOS_REGATED)? {
            Some(v) if v == REGATE_DONE => return Ok(None),
            Some(v) => v.trim().parse::<i64>().unwrap_or(0),
            None => 0,
        };
        // Subagent threads hold no derived TODOs (`purge_subagent_todos`) and get none.
        let mut stmt =
            conn.prepare("SELECT id FROM threads WHERE is_subagent = 0 AND id > ?1 ORDER BY id")?;
        let ids: Vec<i64> = stmt
            .query_map([after], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<_>>()?;
        (after, ids)
    };
    let mut report = RegateReport::default();
    let mut next = 0;
    while next < thread_ids.len() {
        while pause() {
            std::thread::sleep(BACKFILL_PAUSE_POLL);
        }
        let mut conn = lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let started = std::time::Instant::now();
        let end = (next + BACKFILL_BATCH).min(thread_ids.len());
        while next < end {
            let (dropped, added) = regate_thread(&tx, thread_ids[next], now)?;
            report.dropped += dropped;
            report.added += added;
            after = thread_ids[next];
            next += 1;
            if started.elapsed() >= BACKFILL_BATCH_TIME {
                break;
            }
        }
        app_config::put(&tx, TODOS_REGATED, &after.to_string())?;
        tx.commit()?;
    }
    report.threads = thread_ids.len();
    app_config::put(&lock(), TODOS_REGATED, REGATE_DONE)?;
    Ok(Some(report))
}

/// Whether the index has every column the re-judge reads (`facts` curation from 0015, the
/// uid and source seq from 0034). The app always migrates before its watcher runs, but an
/// index this build has not upgraded must be left alone rather than half judged.
fn regate_columns_present(conn: &Connection) -> bool {
    conn.prepare(
        "SELECT COUNT(*) FROM pragma_table_info('facts')
         WHERE name IN ('pinned', 'hidden', 'edited', 'uid', 'source_seq', 'source_message_id')",
    )
    .and_then(|mut stmt| stmt.query_row([], |r| r.get::<_, i64>(0)))
    .is_ok_and(|n| n == 6)
}

/// One thread's re-judge, inside the caller's transaction: (TODOs deleted, follow-ups added).
fn regate_thread(tx: &Connection, thread_id: i64, now: i64) -> Result<(usize, usize)> {
    let mut dropped = 0;
    for (id, to) in regate_drops(tx, thread_id)? {
        // The predicate again: the row must still be this thread's open, uncurated heuristic
        // TODO when it moves or goes.
        let row = format!("id = ?2 AND extractor = 'heuristic' AND {OPEN_UNCURATED_DERIVED_SQL}");
        match to {
            // Where a full re-derive puts it, under the uid it has.
            Some((message_id, seq, ts)) => {
                tx.execute(
                    &format!(
                        "UPDATE facts SET source_message_id = ?3, source_seq = ?4, created_at = ?5
                         WHERE {row}"
                    ),
                    params![thread_id, id, message_id, seq, todo_created_at(ts, now)],
                )?;
            }
            None => {
                dropped += tx.execute(
                    &format!("DELETE FROM facts WHERE {row}"),
                    params![thread_id, id],
                )?;
            }
        }
    }
    // Counted, not tallied per insert: a later list removes what an earlier one added.
    let followups = |tx: &Connection| -> Result<usize> {
        let n: i64 = tx.query_row(
            "SELECT COUNT(*) FROM facts WHERE thread_id = ?1 AND extractor = 'followup'",
            [thread_id],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    };
    let before = followups(tx)?;
    // An append over the thread's own messages: every row that stays seeds the duplicate
    // check, and only follow-ups are taken (a heuristic TODO missing now stays missing).
    let mut derive = TodoDeriver::start(tx, thread_id, false, now)?.rejudging();
    let mut sel = tx.prepare_cached(
        "SELECT id, text, ts, seq FROM messages
         WHERE thread_id = ?1 AND role = 'assistant' ORDER BY seq, id",
    )?;
    let rows = sel.query_map([thread_id], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<i64>>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })?;
    for row in rows {
        let (mid, text, ts, seq) = row?;
        derive.message(mid, true, &text, ts, seq)?;
    }
    Ok((dropped, followups(tx)?.saturating_sub(before)))
}

/// A message that yields a TODO: (message id, seq, ts).
pub(crate) type Yield = (i64, i64, Option<i64>);

/// The open, uncurated heuristic TODOs of `thread_id` that the assistant checklist gate now
/// rejects: rows the old rule ([`extract_todos`]) derived from their source message, an
/// assistant one, that today's rule for it ([`message_todos`]) no longer yields as a
/// heuristic TODO. A row whose source message is gone (no id, no seq that finds one) or whose
/// message no longer yields its text is left alone: nothing proves the gate rejects it. Each
/// comes with the first user or assistant message of the thread (id, seq, ts) that still
/// yields its text as a heuristic TODO (a later `TODO:` marker, the user's own `- [ ]` line),
/// if any: a full re-derive keeps the TODO there, and the re-judge adds no heuristic TODO
/// back, so deleting it would lose it until the thread is parsed again.
pub(crate) fn regate_drops(tx: &Connection, thread_id: i64) -> Result<Vec<(i64, Option<Yield>)>> {
    let mut sel = tx.prepare_cached(&format!(
        "SELECT id, text, source_message_id, source_seq FROM facts
         WHERE extractor = 'heuristic' AND {OPEN_UNCURATED_DERIVED_SQL}"
    ))?;
    let rows: Vec<(i64, String, Option<i64>, Option<i64>)> = sel
        .query_map([thread_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut by_id =
        tx.prepare_cached("SELECT role, text FROM messages WHERE id = ?1 AND thread_id = ?2")?;
    let mut by_seq = tx.prepare_cached(
        "SELECT role, text FROM messages WHERE thread_id = ?1 AND seq = ?2 ORDER BY id LIMIT 1",
    )?;
    let mut rejected = Vec::new();
    for (id, text, message_id, seq) in rows {
        let found: Option<(String, String)> = match message_id {
            Some(m) => by_id
                .query_row(params![m, thread_id], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?,
            None => None,
        };
        let found = match (found, seq) {
            (Some(f), _) => Some(f),
            (None, Some(s)) => by_seq
                .query_row(params![thread_id, s], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?,
            (None, None) => None,
        };
        let Some((role, message)) = found else {
            continue;
        };
        if role != "assistant" {
            continue;
        }
        let same = |t: &String| t.eq_ignore_ascii_case(&text);
        if extract_todos(&message).iter().any(same) && !scan_todos(&message, true).iter().any(same)
        {
            rejected.push((id, text));
        }
    }
    if rejected.is_empty() {
        return Ok(Vec::new());
    }
    // (id, seq, ts, heuristic TODOs: `message_todos`' heuristic half) of each message that
    // yields one, read only for a thread with a rejected row (the measured index's re-judge
    // rejected 76 TODOs across its 1,789 sessions).
    let mut yields = Vec::new();
    let mut sel = tx.prepare_cached(
        "SELECT id, role, text, ts, seq FROM messages
         WHERE thread_id = ?1 AND role IN ('user', 'assistant') ORDER BY seq, id",
    )?;
    let rows = sel.query_map([thread_id], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<i64>>(3)?,
            r.get::<_, i64>(4)?,
        ))
    })?;
    for row in rows {
        let (mid, role, text, ts, seq) = row?;
        let todos = scan_todos(&text, role == "assistant");
        if !todos.is_empty() {
            yields.push((mid, seq, ts, todos));
        }
    }
    Ok(rejected
        .into_iter()
        .map(|(id, text)| {
            let to = yields
                .iter()
                .find(|(.., todos)| todos.iter().any(|t| t.eq_ignore_ascii_case(&text)))
                .map(|&(mid, seq, ts, _)| (mid, seq, ts));
            (id, to)
        })
        .collect())
}

/// An open TODO surfaced to the UI / agents, with the thread it came from.
#[derive(Debug, Serialize, schemars::JsonSchema)]
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
    /// Who wrote it: 'heuristic', 'followup' (an item from an agent's deferred-work list,
    /// [`EXTRACTOR_FOLLOWUP`]), 'llm', 'user', 'agent', or an importer
    /// ([`harvest::EXTRACTOR_CLAUDE_MEMORY`], [`harvest::EXTRACTOR_CLAUDE_COMPACT`],
    /// [`EXTRACTOR_PLAYBOOK`]).
    pub extractor: String,
    /// Stable key of an imported fact (`facts.origin_ref`), else None.
    #[serde(rename = "originRef")]
    pub origin_ref: Option<String>,
}

/// `facts.origin_ref` as a select expression: the column, or NULL on an index older than
/// migration 0032. Read-only sidecars and pooled readers never migrate (see
/// `db::has_index`), and naming a missing column fails the whole query, so a newer `cal`
/// or hook on an index the app has not upgraded yet would otherwise lose every fact list.
pub(crate) fn origin_ref_sql(conn: &Connection) -> &'static str {
    let has = conn
        .prepare_cached("SELECT 1 FROM pragma_table_info('facts') WHERE name = 'origin_ref'")
        .and_then(|mut stmt| stmt.exists([]))
        .unwrap_or(false);
    if has {
        "f.origin_ref"
    } else {
        "NULL"
    }
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
    let mut sql = format!(
        "SELECT f.id, f.thread_id, f.text, s.kind, t.title, t.project_path, f.created_at,
                f.extractor, {}
         FROM facts f
         JOIN threads t ON t.id = f.thread_id
         JOIN sources s ON s.id = t.source_id
         WHERE f.kind = 'todo' AND f.status = 'open' AND f.hidden = 0
           AND (t.is_subagent = 0 OR f.pinned = 1)",
        origin_ref_sql(conn)
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
        // A project fragment's `_` and `%` are literal, as in every project scope.
        args.push(Box::new(crate::scope::like_contains(p)));
        sql.push_str(&format!(
            " AND t.project_path LIKE ?{} ESCAPE '\\'",
            args.len()
        ));
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
            extractor: r.get(7)?,
            origin_ref: r.get(8)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// ---------------------------------------------------------------------------
// LLM distillation tier (opt-in). Decisions / gotchas / summary per thread.
// ---------------------------------------------------------------------------

/// Distillation engine config, shared across the app / cal / MCP via `app_config`.
/// `enabled` is the consent flag for LLM distillation only: nothing distills until the user
/// turns it on. Heuristic TODOs, Remember and project memory work with it off.
#[derive(Debug, Serialize)]
pub struct KnowledgeConfig {
    pub enabled: bool,
    /// None = the first saved API key (the legacy default existing users consented to; never
    /// reinterpreted as a CLI engine).
    pub provider: Option<String>,
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
/// text to the chosen engine (a CLI subscription or cloud key), or to keep it local (Ollama).
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

/// One engine the memory setup card offers. `kind` is `cli` (a logged-in agent CLI),
/// `ollama` (a local server) or `key` (a cloud provider with a saved API key).
#[derive(Debug, Clone, Serialize)]
pub struct MemoryEngine {
    pub id: String,
    pub label: String,
    pub kind: &'static str,
    pub detected: bool,
    /// The CLI's path or "not found", the Ollama server state, or "API key saved".
    pub detail: String,
}

/// What the memory setup card shows: the current config, the engines it can turn on, the
/// one to preselect, and the auto-distill budget the consent copy quotes.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemorySetup {
    pub enabled: bool,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub engines: Vec<MemoryEngine>,
    pub recommended: Option<String>,
    pub eligible_threads: usize,
    pub daily_cap: i64,
}

/// The engine to preselect: the first detected of claude-cli, codex-cli, local Ollama, then
/// the first saved key. A CLI needs no key and runs on the subscription the user already
/// has; Ollama keeps text on the machine; a saved key bills per token, so it comes last.
pub fn recommended_engine(engines: &[MemoryEngine]) -> Option<String> {
    let rank = |e: &MemoryEngine| match (e.id.as_str(), e.kind) {
        ("claude-cli", _) => 0,
        ("codex-cli", _) => 1,
        (_, "ollama") => 2,
        _ => 3,
    };
    engines
        .iter()
        .enumerate()
        .filter(|(_, e)| e.detected)
        .min_by_key(|(i, e)| (rank(e), *i))
        .map(|(_, e)| e.id.clone())
}

/// Turn on LLM memory with an explicit engine: the one-click consent from the memory setup
/// card. Stores the provider by name (never None, so the choice the user saw is the one
/// that runs) and turns background auto-distill on with it.
pub fn enable_memory(conn: &Connection, provider: &str, model: Option<&str>) -> Result<()> {
    set_config(conn, true, Some(provider), model)?;
    set_auto_distill(conn, true)
}

/// Replace a thread's LLM-distilled facts with a fresh set and mark it extracted at
/// the current message count. Heuristic todos (extractor='heuristic') are untouched.
///
/// `refs` is the [`RefMap`] of the [`distill_memory_context`] block the model was shown
/// (`RefMap::default()` when it was shown none). In one transaction:
/// - the thread's uncurated LLM facts are replaced, an identical text keeping its uid; but some
///   are kept (never deleted), and no new fact is inserted with their text: a fact another
///   thread's fact actively supersedes (history, whoever declared it, so a re-distill cannot
///   bring the old decision back as current), one another source's link names (the user's,
///   an agent's, a review's, a repo import's, or a link two sessions distilled: deleting
///   either side would silently void it, and a reworded copy cannot be matched to it), and one
///   carrying a flag or a resolution that this re-distill does not replace (a user's or an
///   agent's flag, a retirement from another session, one the user kept). `d<N>` / `g<N>`
///   naming a kept text resolves to the kept fact; a reworded one is a near-duplicate, which
///   beats losing the user's link;
/// - the thread's previous distilled links (except incoming ones: a listed item it said
///   supersedes its own, which the kept target holds) and unresolved distilled signals are
///   removed (a re-distill is idempotent; a flag the user resolved stays resolved), then
///   `d.links`, `d.reaffirms` and `d.retires` are applied with the per-thread caps and
///   validation of [`truth::apply_distilled_truth`].
pub fn store_distilled(
    conn: &mut Connection,
    thread_id: i64,
    d: &Distilled,
    refs: &RefMap,
    now: i64,
) -> Result<()> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let truth = truth::has_truth_tables(&tx);
    // Facts of this thread that must outlive the re-distill (see the doc): superseded by
    // another thread's fact through any active link, including one this thread's own distill
    // declared (the incoming link the cleanup below keeps, else re-distilling the older
    // session of a backlog pair without repeating the link would revive its decision); named
    // by an active link this re-distill does not replace; or carrying a signal it does not
    // replace, other than a reaffirmation (another session's confirmation is not worth a
    // duplicate).
    let kept: Vec<(i64, String, String, String)> = if truth {
        let mut sel = tx.prepare(&format!(
            "SELECT f.id, f.kind, f.text, f.uid FROM facts f
             WHERE f.thread_id = ?1 AND f.extractor = 'llm'
               AND f.pinned = 0 AND f.edited = 0 AND f.hidden = 0 AND f.uid IS NOT NULL
               AND (EXISTS (SELECT 1 FROM fact_links lk JOIN facts nf ON nf.uid = lk.from_uid
                    WHERE lk.to_uid = f.uid AND lk.kind = 'supersedes'
                      AND {} AND lk.dismissed_at IS NULL AND nf.thread_id != f.thread_id)
                 OR EXISTS (SELECT 1 FROM fact_links lk
                    WHERE lk.from_uid = f.uid AND lk.dismissed_at IS NULL
                      AND NOT (lk.source = 'distill' AND lk.thread_id IS ?1))
                 OR EXISTS (SELECT 1 FROM fact_links lk
                    WHERE lk.to_uid = f.uid AND lk.dismissed_at IS NULL
                      AND NOT (lk.source = 'distill' AND lk.thread_id IS ?1))
                 OR EXISTS (SELECT 1 FROM fact_signals sg
                    WHERE sg.fact_uid = f.uid AND sg.kind != 'reaffirmed'
                      AND NOT (sg.source = 'distill' AND sg.thread_id IS ?1
                               AND sg.resolved_at IS NULL)))",
            truth::NEWER_STANDS_NF_SQL
        ))?;
        let rows = sel
            .query_map([thread_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        rows
    } else {
        Vec::new()
    };
    let kept_uid: std::collections::HashMap<(String, String), String> = kept
        .iter()
        .map(|(_, kind, text, uid)| ((kind.clone(), distilled_key(text)), uid.clone()))
        .collect();
    // The same fact distilled again keeps its uid: a copy already published to repo memory
    // (or another machine) must still match it, not come back as a duplicate. Keyed by kind
    // and trimmed, lowercased text.
    let mut uids: std::collections::HashMap<(String, String), String> = {
        let mut sel = tx.prepare(
            "SELECT kind, text, uid FROM facts WHERE thread_id = ?1 AND extractor = 'llm'
                AND pinned = 0 AND edited = 0 AND hidden = 0 AND uid IS NOT NULL",
        )?;
        let rows = sel
            .query_map([thread_id], |r| {
                Ok((
                    (
                        r.get::<_, String>(0)?,
                        distilled_key(&r.get::<_, String>(1)?),
                    ),
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        rows
    };
    let mut uid_for = |kind: &str, text: &str| {
        uids.remove(&(kind.to_string(), distilled_key(text)))
            .unwrap_or_else(new_fact_uid)
    };
    if truth {
        // A link the user undid stays: it is what keeps this re-distill from declaring it again.
        // So does an incoming one (another thread's fact supersedes this thread's own): its
        // target is kept above as history, and the model leaving the link out this time (an
        // LLM does not always repeat itself) must not bring the replaced decision back. Run
        // while this thread's facts still exist, so the own side is recognized.
        tx.execute(
            "DELETE FROM fact_links WHERE thread_id = ?1 AND source = 'distill'
               AND dismissed_at IS NULL
               AND NOT (kind = 'supersedes'
                    AND EXISTS (SELECT 1 FROM facts m
                                WHERE m.uid = fact_links.to_uid AND m.thread_id = ?1)
                    AND EXISTS (SELECT 1 FROM facts o
                                WHERE o.uid = fact_links.from_uid AND o.thread_id != ?1))",
            [thread_id],
        )?;
        tx.execute(
            "DELETE FROM fact_signals WHERE thread_id = ?1 AND source = 'distill'
               AND (kind = 'reaffirmed' OR resolved_at IS NULL)",
            [thread_id],
        )?;
    }
    // Replace only the UN-curated LLM facts; pinned / edited / hidden ones are the user's
    // now and survive re-distillation, and superseded ones are kept as history.
    let kept_ids: Vec<i64> = kept.iter().map(|k| k.0).collect();
    tx.execute(
        "DELETE FROM facts WHERE thread_id = ?1 AND extractor = 'llm'
            AND pinned = 0 AND edited = 0 AND hidden = 0
            AND id NOT IN (SELECT value FROM json_each(?2))",
        params![thread_id, serde_json::to_string(&kept_ids)?],
    )?;
    // `d1`, `g2`... -> the uid of the fact that holds that item now.
    let mut own: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    {
        let mut ins = tx.prepare(
            "INSERT INTO facts (thread_id, kind, text, status, extractor, seq, created_at, uid)
             VALUES (?1, ?2, ?3, 'open', 'llm', ?4, ?5, ?6)",
        )?;
        let summary = d.summary.trim();
        if !summary.is_empty() {
            ins.execute(params![
                thread_id,
                "summary",
                summary,
                0_i64,
                now,
                uid_for("summary", summary)
            ])?;
        }
        for (kind, prefix, items) in [("decision", 'd', &d.decisions), ("gotcha", 'g', &d.gotchas)]
        {
            for (i, text) in items.iter().enumerate() {
                let t = text.trim();
                if t.is_empty() {
                    continue;
                }
                let r = format!("{prefix}{}", i + 1);
                if let Some(uid) = kept_uid.get(&(kind.to_string(), distilled_key(t))) {
                    own.insert(r, uid.clone());
                    continue;
                }
                let uid = uid_for(kind, t);
                ins.execute(params![thread_id, kind, t, i as i64, now, uid])?;
                own.insert(r, uid);
            }
        }
    }
    truth::apply_distilled_truth(&tx, thread_id, d, &own, refs, now)?;
    tx.execute(
        "UPDATE threads SET knowledge_extracted = 1, knowledge_extracted_at = ?2,
            knowledge_msg_count = distillable_count, knowledge_error = NULL WHERE id = ?1",
        params![thread_id, now],
    )?;
    tx.commit()?;
    Ok(())
}

/// How [`store_distilled`] recognizes a fact distilled again: trimmed, lowercased text.
fn distilled_key(text: &str) -> String {
    text.trim().to_lowercase()
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
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct KFact {
    pub id: i64,
    pub text: String,
    pub pinned: bool,
    /// See [`FactStatus`].
    pub status: FactStatus,
    /// When it was decided (epoch seconds): its session's start for a distilled fact.
    #[serde(rename = "decidedAt")]
    pub decided_at: i64,
}

/// All distilled knowledge for one thread, grouped by kind, with freshness flags.
#[derive(Debug, Serialize, schemars::JsonSchema)]
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

    let mut stmt = conn.prepare(&format!(
        "SELECT f.id, f.kind, f.text, f.pinned, COALESCE(f.uid, ''), {DECIDED_AT_SQL}
         FROM facts f JOIN threads t ON t.id = f.thread_id
         WHERE f.thread_id = ?1 AND f.hidden = 0 ORDER BY f.pinned DESC, f.seq, f.id"
    ))?;
    let rows = stmt
        .query_map([thread_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)? != 0,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let uids: Vec<&str> = rows
        .iter()
        .filter(|r| matches!(r.1.as_str(), "decision" | "gotcha"))
        .map(|r| r.4.as_str())
        .collect();
    let truth = fact_truth(conn, &uids)?;
    let mut summary = None;
    let (mut decisions, mut gotchas, mut todos) = (Vec::new(), Vec::new(), Vec::new());
    for (id, kind, text, pinned, uid, decided_at) in rows {
        let status = truth.get(&uid).map(|t| t.status).unwrap_or_default();
        let fact = KFact {
            id,
            text,
            pinned,
            status,
            decided_at,
        };
        match kind.as_str() {
            "summary" => summary = Some(fact.text),
            "decision" => decisions.push(fact),
            "gotcha" => gotchas.push(fact),
            "todo" => todos.push(fact),
            _ => {}
        }
    }
    Ok(ThreadKnowledge {
        summary,
        decisions,
        gotchas,
        todos,
        extracted,
        // Distilled at some point (the count is kept) and then changed: the indexer only
        // clears `knowledge_extracted`, so the old facts above still show, marked stale.
        stale: kmsg.is_some() && (!extracted || kmsg != Some(mcount)),
        error,
        can_distill: get_config(conn)?.enabled,
    })
}

/// A semantically-recalled fact (decision/gotcha) with the thread it came from, when it was
/// decided and where it stands ([`FactStatus`]: a superseded hit names what replaced it).
#[derive(Debug, Default, Serialize, schemars::JsonSchema)]
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
    /// See [`TodoFact::extractor`].
    #[schemars(
        description = "Who wrote it: llm, heuristic, user, agent, or an importer such as claude-memory."
    )]
    pub extractor: String,
    #[serde(rename = "originRef")]
    pub origin_ref: Option<String>,
    #[serde(rename = "decidedAt")]
    pub decided_at: i64,
    pub status: FactStatus,
    #[serde(rename = "supersededBy", skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<LinkedFact>,
    #[serde(rename = "disputedBy", skip_serializing_if = "Vec::is_empty")]
    pub disputed_by: Vec<LinkedFact>,
    /// Ids of the facts it actively supersedes.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub replaces: Vec<i64>,
    /// 1 + the other sessions that reaffirmed it.
    pub support: i64,
    #[serde(rename = "lastConfirmed", skip_serializing_if = "Option::is_none")]
    pub last_confirmed: Option<i64>,
    /// Set by the contradiction guard only.
    #[serde(rename = "match", skip_serializing_if = "Option::is_none")]
    pub match_strength: Option<GuardMatch>,
}

impl RecallHit {
    /// Fill the derived fields from `t`.
    fn apply_truth(&mut self, t: FactTruth) {
        self.status = t.status;
        self.superseded_by = t.superseded_by;
        self.disputed_by = t.disputed_by;
        self.replaces = t.replaces.into_iter().map(|f| f.id).collect();
        self.support = t.support;
        self.last_confirmed = t.last_confirmed;
    }
}

/// How close a contradiction-guard hit is to the proposal: `close` at or above
/// [`GUARD_CLOSE`] similarity, else `related`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum GuardMatch {
    Close,
    Related,
}

/// Similarity at which a guard hit counts as `close` (the proposal restates or reverses that
/// decision) rather than merely `related`. Calibrated by `eval::tests::calibrate_guard_close`
/// on memory_truth_v1 (bge-small-en-v1.5, proposal query-embedded against the stored fact as
/// check_decision scores it): restatements 0.719-0.935 (median 0.875, n 16), reversals
/// 0.772-0.875 (n 6), same-topic decisions 0.539-0.715 (median 0.657, n 16), unrelated
/// 0.448-0.578. 0.75 calls 15/16 restatements, 6/6 reversals and 0/16 same-topic pairs close
/// (balanced accuracy 0.969). The best separator, 0.717 (1.000), sits in a 0.003 gap set by one
/// pair on each side, which is sampling noise at n 16; and the eval's own guard run has a
/// same-topic decision at 0.749 (refresh token lifetime against an access token proposal),
/// which 0.717 would call close. A false "close" is the costlier error, so it stays 0.75.
pub const GUARD_CLOSE: f32 = 0.75;

/// Similarity a guard hit below [`GUARD_CLOSE`] needs to be shown at all, as `related`
/// (`find_prior_work`, `check_contradiction`). The old floor, 0.55, let nearly every proposal
/// through. Calibrated by `eval::tests::real_guard_calibration` on a clone of the author's
/// index (bge-small-en-v1.5, 2026-11-09): 891 decisions of one project checked in another
/// (12 projects with 15+ decisions, each against the 3 largest others), where nothing decided
/// them. Their best cosine there had a median of 0.662, p90 0.714, p95 0.729 and max 0.795
/// (28 pairs whose texts were identical in both projects left out); 99.7% cleared 0.55, and
/// check_decision answered 96.2% of them with a list (6.46 hits on average), so agents learned
/// to ignore it. 0.72 sits at that p90: 6.5% now get a related hit (0.08 on average). On the
/// eval pairs it passes 15/16 restatements, 6/6 reversals, 0/16 same-topic and 0/10 unrelated
/// (same-topic max 0.715): a related hit is a near-restatement, not a topic match. No
/// `fact_links` supersedes existed on that index to measure positives against.
pub const GUARD_RELATED: f32 = 0.72;

/// A `related` hit more than this below the best hit is dropped. Without a close hit every
/// related one is within it (they all sit in [0.72, 0.75)); next to a close one it keeps only
/// near-ties. memory_truth's "access tokens expire after 15 minutes" has its restatement at
/// 0.948 and a refresh-token decision at 0.749: context the close answer does not need.
pub const GUARD_RELATED_BAND: f32 = 0.05;

/// Most `related` hits one guard answer keeps; every close hit stays.
pub const GUARD_RELATED_MAX: usize = 2;

/// The guard's rule over candidates sorted best first: keep every close hit (at or above
/// [`GUARD_CLOSE`]); keep a related one only at or above `related` and within `band` of the
/// best, at most `max` of them. Parameterized so the calibration can sweep it; the guards
/// call [`guard_keep`].
pub(crate) fn guard_keep_with(sims: &[f32], related: f32, band: f32, max: usize) -> Vec<bool> {
    let best = sims.iter().copied().fold(f32::MIN, f32::max);
    let mut related_kept = 0;
    sims.iter()
        .map(|&s| {
            if s >= GUARD_CLOSE {
                return true;
            }
            let keep = s >= related && s >= best - band && related_kept < max;
            related_kept += usize::from(keep);
            keep
        })
        .collect()
}

/// [`guard_keep_with`] at the calibrated [`GUARD_RELATED`], [`GUARD_RELATED_BAND`] and
/// [`GUARD_RELATED_MAX`]. `sims` must be sorted best first.
pub(crate) fn guard_keep(sims: &[f32]) -> Vec<bool> {
    guard_keep_with(sims, GUARD_RELATED, GUARD_RELATED_BAND, GUARD_RELATED_MAX)
}

/// What check_decision and `cal check` say first when no hit restates or reverses the
/// proposal ([`guard_no_decision`]), before any related one.
pub const NO_DECISION_YET: &str = "No decision on this yet.";

/// Whether a guard answer holds no decision on the proposal: no close hit (a restatement or a
/// reversal of it), only related ones or nothing. check_decision and `cal check` then lead
/// with "No decision on this yet." so a related hit is never read as a settled answer.
pub fn guard_no_decision(hits: &[RecallHit]) -> bool {
    !hits
        .iter()
        .any(|h| h.match_strength == Some(GuardMatch::Close))
}

/// Cross-thread semantic recall of distilled facts. `qv` is a PRECOMPUTED query vector
/// (embed it via `embed::embed_query` BEFORE locking the DB so inference never holds the
/// lock). `kind` is 'decision' or 'gotcha'; optionally scope to a project-path substring.
/// Superseded facts are left out (in SQL, so `k` counts live hits) unless
/// `include_superseded`; flagged and disputed ones stay, labelled by `status`.
pub fn recall(
    conn: &Connection,
    qv: &[f32],
    kind: &str,
    project: Option<&str>,
    k: usize,
    include_superseded: bool,
) -> Result<Vec<RecallHit>> {
    // Over-fetch chunks so the kind/project filter (applied AFTER the KNN) still leaves k.
    let knn_k = (k * 5).max(100);
    let origin_ref = origin_ref_sql(conn);
    let live = if !include_superseded && truth::has_truth_tables(conn) {
        format!(" AND NOT {}", truth::SUPERSEDED_SQL)
    } else {
        String::new()
    };
    let mut sql = format!(
        "WITH knn AS MATERIALIZED (
            SELECT fact_id, distance FROM vec_facts
            WHERE embedding MATCH ?1 AND k = {knn_k} ORDER BY distance
         )
         SELECT f.id, f.thread_id, f.kind, f.text, f.rationale, s.kind, t.title, t.project_path,
                MIN(knn.distance) AS d, f.extractor, {origin_ref}, {DECIDED_AT_SQL},
                COALESCE(f.uid, '')
         FROM knn
         JOIN facts f ON f.id = knn.fact_id
         JOIN threads t ON t.id = f.thread_id
         JOIN sources s ON s.id = t.source_id
         WHERE f.kind = ?2 AND f.hidden = 0{live}"
    );
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = vec![
        Box::new(crate::embed::vec_to_bytes(qv)),
        Box::new(kind.to_string()),
    ];
    // Match how facts are WRITTEN + aggregated (record_fact / project_memory both scope by
    // COALESCE(project_key, project_path)); filtering project_path alone silently drops the
    // canonical-key threads the backfill exists to fix.
    if let Some(cond) = crate::scope::ProjectScope::fragment(project).sql("t", &mut args) {
        sql.push_str(&format!(" AND {cond}"));
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
    let rows = stmt
        .query_map(arg_refs.as_slice(), |r| {
            let dist: f64 = r.get(8)?;
            Ok((
                RecallHit {
                    id: r.get(0)?,
                    thread_id: r.get(1)?,
                    kind: r.get(2)?,
                    text: r.get(3)?,
                    rationale: r.get(4)?,
                    source: r.get(5)?,
                    title: r.get(6)?,
                    project_path: r.get(7)?,
                    similarity: (1.0 - dist) as f32,
                    extractor: r.get(9)?,
                    origin_ref: r.get(10)?,
                    decided_at: r.get(11)?,
                    support: 1,
                    ..Default::default()
                },
                r.get::<_, String>(12)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let uids: Vec<&str> = rows.iter().map(|(_, u)| u.as_str()).collect();
    let mut truth = fact_truth(conn, &uids)?;
    Ok(rows
        .into_iter()
        .map(|(mut h, uid)| {
            if let Some(t) = truth.remove(&uid) {
                h.apply_truth(t);
            }
            h
        })
        .collect())
}

/// A decision or gotcha matching a search, for the Search view's "Decisions and gotchas"
/// group.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FactHit {
    pub id: i64,
    pub kind: String,
    pub text: String,
    /// The fact's canonical project key ("" when its thread has none).
    pub project: String,
    pub thread_id: i64,
    /// The message the fact was extracted from, so a hit can open the thread there.
    pub source_message_id: Option<i64>,
    pub title: Option<String>,
    pub pinned: bool,
    pub extractor: String,
    /// When the fact was recorded (unix seconds), so Search's date presets apply to facts.
    pub created_at: i64,
}

/// Query words the keyword match uses at most; more only slow the scan.
const FACT_SEARCH_MAX_WORDS: usize = 8;

/// Columns of a [`FactHit`] over `facts f JOIN threads t`, in `fact_hit_row` order.
const FACT_HIT_COLS: &str = "f.id, f.kind, f.text, COALESCE(t.project_key, t.project_path, ''),
     f.thread_id, f.source_message_id, t.title, f.pinned, f.extractor, f.created_at";

fn fact_hit_row(r: &rusqlite::Row) -> rusqlite::Result<FactHit> {
    Ok(FactHit {
        id: r.get(0)?,
        kind: r.get(1)?,
        text: r.get(2)?,
        project: r.get(3)?,
        thread_id: r.get(4)?,
        source_message_id: r.get(5)?,
        title: r.get(6)?,
        pinned: r.get::<_, i64>(7)? != 0,
        extractor: r.get(8)?,
        created_at: r.get(9)?,
    })
}

/// Visible decisions and gotchas whose text holds every term of `query` (case-insensitive;
/// phrases, stopwords and punctuation as the search box parses them, see
/// `search::like_terms`), pinned first then newest, optionally in one project (its canonical
/// key, exact). Same visibility rule as the Project page: hidden facts never, subagent facts
/// only when pinned. `semantic` adds the [`recall`] hits for a query vector, ahead of the
/// keyword ones and deduped by id; it still answers a query of stopwords alone.
pub fn search_facts(
    conn: &Connection,
    query: &str,
    project: Option<&str>,
    limit: usize,
    semantic: Option<&[f32]>,
) -> Result<Vec<FactHit>> {
    if query.trim().is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let words: Vec<String> = crate::search::like_terms(query)
        .into_iter()
        .take(FACT_SEARCH_MAX_WORDS)
        .map(|w| w.to_lowercase())
        .collect();
    let project = project.map(str::trim).filter(|p| !p.is_empty());
    let mut out: Vec<FactHit> = Vec::new();
    if let Some(qv) = semantic {
        let mut hits: Vec<RecallHit> = Vec::new();
        for kind in ["decision", "gotcha"] {
            // Semantic hits are a bonus on top of the keyword match, which still answers if
            // the vector table can't be read. Superseded facts stay findable here, as the
            // keyword match finds them: Search is where history is looked up.
            if let Ok(found) = recall(conn, qv, kind, project, limit, true) {
                hits.extend(found);
            }
        }
        hits.sort_by(|a, b| b.similarity.total_cmp(&a.similarity));
        let mut by_id = conn.prepare_cached(&format!(
            "SELECT {FACT_HIT_COLS} FROM facts f JOIN threads t ON t.id = f.thread_id
             WHERE f.id = ?1 AND (t.is_subagent = 0 OR f.pinned = 1)"
        ))?;
        for h in hits {
            let Some(hit) = by_id.query_row([h.id], fact_hit_row).optional()? else {
                continue;
            };
            // `recall` scopes by substring; keep only the exact project.
            if project.is_some_and(|p| hit.project != p) || out.iter().any(|o| o.id == hit.id) {
                continue;
            }
            out.push(hit);
            if out.len() >= limit {
                return Ok(out);
            }
        }
    }
    if words.is_empty() {
        return Ok(out);
    }
    let (sql, args) = fact_search_sql(&words, project, limit + out.len());
    let arg_refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    for hit in stmt.query_map(arg_refs.as_slice(), fact_hit_row)? {
        let hit = hit?;
        if !out.iter().any(|o| o.id == hit.id) {
            out.push(hit);
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(out)
}

/// The keyword arm of [`search_facts`]: every term a LIKE, off the kind index (about 2k
/// decisions and gotchas on a 21k-thread index, each thread row by its key), sorted.
fn fact_search_sql(
    words: &[String],
    project: Option<&str>,
    limit: usize,
) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
    let mut sql = format!(
        "SELECT {FACT_HIT_COLS} FROM facts f JOIN threads t ON t.id = f.thread_id
         WHERE f.kind IN ('decision', 'gotcha') AND f.hidden = 0
           AND (t.is_subagent = 0 OR f.pinned = 1)"
    );
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    for w in words {
        args.push(Box::new(crate::scope::like_contains(w)));
        sql.push_str(&format!(" AND f.text LIKE ?{} ESCAPE '\\'", args.len()));
    }
    if let Some(p) = project {
        args.push(Box::new(p.to_string()));
        sql.push_str(&format!(
            " AND COALESCE(t.project_key, t.project_path) = ?{}",
            args.len()
        ));
    }
    args.push(Box::new(limit as i64));
    sql.push_str(&format!(
        " ORDER BY f.pinned DESC, f.created_at DESC, f.id DESC LIMIT ?{}",
        args.len()
    ));
    (sql, args)
}

/// One past session relevant to what you're about to do — the "you solved this before"
/// guard. Distinct from `recall` (which returns individual decision/gotcha facts): this
/// rolls the matching facts UP to their thread, so the answer is a short list of prior
/// SESSIONS to revisit, each with the single most-relevant decision/gotcha as the reason.
#[derive(Debug, Serialize, schemars::JsonSchema)]
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
    // The guard is high-stakes: an agent acts on "you solved this before", so a false positive
    // is worse than an empty answer. Every close session stays; merely related ones only as
    // the guard rule allows (`guard_keep`).
    let mut out = prior_work_candidates(conn, qv, project, k)?;
    let sims: Vec<f32> = out.iter().map(|w| w.similarity).collect();
    let mut keep = guard_keep(&sims).into_iter();
    out.retain(|_| keep.next().unwrap_or(false));
    out.truncate(k);
    Ok(out)
}

/// Every thread [`find_prior_work`] weighs, best first, before the guard rule: one entry per
/// thread with its best-matching decision or gotcha. Separate so the calibration can measure
/// the rule against the candidates it sees.
pub(crate) fn prior_work_candidates(
    conn: &Connection,
    qv: &[f32],
    project: Option<&str>,
    k: usize,
) -> Result<Vec<PriorWork>> {
    let over = (k * 3).max(15);
    // A superseded fact is no reason to revisit its session: the newer one is.
    let mut hits = recall(conn, qv, "decision", project, over, false)?;
    hits.extend(recall(conn, qv, "gotcha", project, over, false)?);

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

    let mut out: Vec<PriorWork> = by_thread.into_values().collect();
    out.sort_by(|a, b| {
        b.similarity
            .total_cmp(&a.similarity)
            .then(a.thread_id.cmp(&b.thread_id))
    });
    Ok(out)
}

/// The active contradiction guard: prior DECISIONS semantically close to `qv` (an embedded
/// proposal an agent is about to act on). A false "you already decided X" is worse than
/// silence, so every `close` hit (a restatement or reversal) stays and at most
/// [`GUARD_RELATED_MAX`] `related` ones do ([`guard_keep`]); no close hit means no decision on
/// it yet ([`guard_no_decision`]). Surface these before the agent re-litigates a settled
/// choice. Returns decision facts (each with its rationale, when recorded, its date and
/// status, and `match`). `project` optionally scopes to one repo.
///
/// Superseded decisions are always included, each naming what replaced it: they are the most
/// useful answer to "should we go back to X?". Order: live hits (`current`, `disputed`) first,
/// then `flagged`, then `superseded`, by similarity within each group.
pub fn check_contradiction(
    conn: &Connection,
    qv: &[f32],
    project: Option<&str>,
    k: usize,
) -> Result<Vec<RecallHit>> {
    let mut hits = guard_candidates(conn, qv, project, k)?;
    let sims: Vec<f32> = hits.iter().map(|h| h.similarity).collect();
    let mut keep = guard_keep(&sims).into_iter();
    hits.retain(|_| keep.next().unwrap_or(false));
    for h in &mut hits {
        h.match_strength = Some(if h.similarity >= GUARD_CLOSE {
            GuardMatch::Close
        } else {
            GuardMatch::Related
        });
    }
    let group = |s: FactStatus| match s {
        FactStatus::Current | FactStatus::Disputed => 0,
        FactStatus::Flagged => 1,
        FactStatus::Superseded => 2,
    };
    hits.sort_by(|a, b| {
        group(a.status).cmp(&group(b.status)).then(
            b.similarity
                .partial_cmp(&a.similarity)
                .unwrap_or(std::cmp::Ordering::Equal),
        )
    });
    hits.truncate(k);
    Ok(hits)
}

/// Every decision [`check_contradiction`] weighs, best first, before the guard rule
/// (superseded ones included). Separate so the calibration can measure the rule against the
/// candidates it sees.
pub(crate) fn guard_candidates(
    conn: &Connection,
    qv: &[f32],
    project: Option<&str>,
    k: usize,
) -> Result<Vec<RecallHit>> {
    recall(conn, qv, "decision", project, (k * 3).max(15), true)
}

// ---------------------------------------------------------------------------
// Project Memory — distilled knowledge aggregated across a project's threads.
// ---------------------------------------------------------------------------

/// A thread is "distillable" (worth an LLM pass / counted in coverage) if it's a real
/// top-level thread with enough substance. Keeps batch distill off trivial/subagent rows.
pub(crate) const DISTILLABLE: &str = "is_subagent = 0 AND message_count >= 4";

/// One distilled fact in a project's aggregated memory, with its source thread, when it was
/// decided and where it stands. The fields after `originRef` are additions (0035): empty and
/// None ones are left out of the JSON.
#[derive(Debug, Default, Serialize, schemars::JsonSchema)]
pub struct MemoryFact {
    pub id: i64,
    #[serde(rename = "threadId")]
    pub thread_id: i64,
    pub text: String,
    pub title: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    pub pinned: bool,
    /// The user rewrote its text: curated like a pinned fact, so an unresolved flag leaves it
    /// in memory. With `pinned` it says why a flagged fact is still in memory, which the
    /// Projects page used to infer from the live lists. Left out of the JSON when false.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub edited: bool,
    /// Who wrote it: `llm`, `heuristic`, `user`, `agent`, or a harvest origin such as
    /// `claude-memory`. The SessionStart hook and the managed AGENTS.md / CLAUDE.md block
    /// drop `claude-memory` facts, which Claude Code already loads from its own auto memory,
    /// and so does `.callimachus/memory.md`, a repo file people commit; MCP results and the
    /// Projects tab keep them. `repo`: imported from the repo's committed
    /// `.callimachus/memory/` files (`originRef` names the file), shown everywhere.
    pub extractor: String,
    #[serde(rename = "originRef")]
    pub origin_ref: Option<String>,
    /// `decision`, `gotcha` or `todo`.
    pub kind: String,
    /// When it was decided (epoch seconds, [`DECIDED_AT_SQL`]): a distilled fact's session
    /// start, else its own time.
    #[serde(rename = "decidedAt")]
    pub decided_at: i64,
    /// The source kind of its thread (`claude_code`, `codex`; `in_app` for recorded and
    /// imported facts, which live on the project's "Recorded memory" thread).
    pub source: String,
    pub status: FactStatus,
    /// The ids of the facts it actively supersedes, newest first: the same list of fact ids
    /// as `replaces` on a recall or check hit, so the key means one thing on every surface.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub replaces: Vec<i64>,
    /// `decidedAt` of the newest fact it actively supersedes (the `replaces <date>` clause in
    /// a memory line). A date, never a fact id: the ids are in `replaces`.
    #[serde(rename = "replacesDecidedAt", skip_serializing_if = "Option::is_none")]
    pub replaces_decided_at: Option<i64>,
    /// The facts it is disputed with (active `contradicts` links), newest first.
    #[serde(rename = "disputedBy", skip_serializing_if = "Vec::is_empty")]
    pub disputed_by: Vec<LinkedFact>,
    /// 1 + the other sessions that reaffirmed it.
    pub support: i64,
    /// The newest start among those sessions.
    #[serde(rename = "lastConfirmed", skip_serializing_if = "Option::is_none")]
    pub last_confirmed: Option<i64>,
    /// What replaced it (superseded facts only).
    #[serde(rename = "supersededBy", skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<LinkedFact>,
    /// Its unresolved stale / wrong flags (flagged facts only).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<FactFlag>,
}

impl MemoryFact {
    /// Fill the derived fields from `t`.
    fn apply_truth(&mut self, t: FactTruth) {
        self.status = t.status;
        self.replaces_decided_at = t.replaces.first().map(|f| f.decided_at);
        self.replaces = t.replaces.iter().map(|f| f.id).collect();
        self.disputed_by = t.disputed_by;
        self.support = t.support;
        self.last_confirmed = t.last_confirmed;
        self.superseded_by = t.superseded_by;
        self.flags = t.flags;
    }
}

/// Durable, aggregated knowledge for one project: decisions + gotchas + open TODOs
/// distilled across all its threads, plus coverage so callers can prompt to distill more.
///
/// The three lists hold live facts only: never a superseded one, and a flagged one only when
/// it is pinned (the user's trusted fact stays injected, labelled `flagged`). Disputed facts
/// stay, labelled. `superseded` and `flagged` are filled only for a [`MemoryQuery`] with
/// `history` (the Projects page): decisions and gotchas together, by the same order.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ProjectMemory {
    pub project: String,
    pub decisions: Vec<MemoryFact>,
    pub gotchas: Vec<MemoryFact>,
    #[serde(rename = "openTodos")]
    pub open_todos: Vec<MemoryFact>,
    /// Replaced facts, kept as history (`history` queries only).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub superseded: Vec<MemoryFact>,
    /// Facts with an unresolved stale / wrong flag, pinned ones included (`history` only).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub flagged: Vec<MemoryFact>,
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

/// Which facts [`project_facts`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slice {
    /// What memory injects: not superseded, and not flagged unless pinned or edited (the
    /// user curated it; only they settle a flag on it).
    Live,
    /// Superseded (history).
    Superseded,
    /// Unresolved stale / wrong flag and not superseded, pinned ones included.
    Flagged,
}

/// Distilled facts of one `kind` (None: decisions and gotchas) across a project, pinned first, then by session time (the later
/// of when each was decided and last reaffirmed), deduped by text, capped. The `slice` filter
/// runs in SQL so `limit` counts only facts of that slice. `exclude` leaves one extractor's
/// facts out in SQL, before the cap and the dedupe, so a project with many of them
/// (harvested auto memories sort by file mtime, so they are often the newest) can neither
/// push the other facts out of the window nor shadow an identical fact from another
/// extractor. Each fact comes with its uid; the derived fields are left at their defaults for
/// [`get_project_memory_with`] to fill in one batch. `ids`, a JSON array of fact ids, keeps
/// only those facts (the working set's candidates, [`working_set`]): the slice filter and
/// the order then run on them alone. `playbooks` false leaves unpinned playbook gotchas out
/// in SQL, like `exclude`; true keeps them and, once the list is capped, moves the unpinned
/// ones after the other facts, so the list holds the same facts as without the move
/// ([`MemoryQuery::playbooks`]).
#[allow(clippy::too_many_arguments)]
fn project_facts(
    conn: &Connection,
    kind: Option<&str>,
    project: &str,
    open_only: bool,
    limit: usize,
    exclude: Option<&str>,
    borrowed: Option<&str>,
    slice: Slice,
    ids: Option<&str>,
    playbooks: bool,
) -> Result<Vec<(MemoryFact, String)>> {
    let truth = truth::has_truth_tables(conn);
    if !truth && slice != Slice::Live {
        return Ok(Vec::new());
    }
    let sql = project_facts_sql(
        conn,
        truth,
        kind.is_some(),
        borrowed.is_some(),
        open_only,
        exclude.is_some(),
        slice,
        ids.is_some(),
        playbooks,
    );
    // `?1` is the kind, unused (bound to '') for decisions and gotchas together.
    let kind = kind.unwrap_or("");
    let mut params: Vec<&dyn rusqlite::ToSql> = vec![&kind, &project];
    if let Some(b) = &borrowed {
        params.push(b);
    }
    if let Some(x) = &exclude {
        params.push(x);
    }
    if let Some(i) = &ids {
        params.push(i);
    }
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params.as_slice(), |r| {
        Ok((
            MemoryFact {
                id: r.get(0)?,
                thread_id: r.get(1)?,
                text: r.get(2)?,
                title: r.get(3)?,
                created_at: r.get(4)?,
                pinned: r.get::<_, i64>(5)? != 0,
                extractor: r.get(6)?,
                origin_ref: r.get(7)?,
                kind: r.get(8)?,
                source: r.get(10)?,
                decided_at: r.get(11)?,
                edited: r.get::<_, i64>(12)? != 0,
                support: 1,
                ..Default::default()
            },
            r.get::<_, String>(9)?,
        ))
    })?;
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for row in rows {
        let f = row?;
        if seen.insert(f.0.text.trim().to_lowercase()) {
            out.push(f);
            if out.len() >= limit {
                break;
            }
        }
    }
    if playbooks {
        // After the cap and the dedupe, not in the ORDER BY: sorted last in SQL, a repo's
        // other gotchas (often more than the cap) would push every playbook out of the list
        // `cal memory --json` printed before. This moves them and keeps the same facts.
        let (mut kept, last): (Vec<_>, Vec<_>) = out
            .into_iter()
            .partition(|(f, _)| f.pinned || f.extractor != EXTRACTOR_PLAYBOOK);
        kept.extend(last);
        out = kept;
    }
    Ok(out)
}

/// The SQL of [`project_facts`]. Parameters: `?1` the kind (`''` when `kind` is false: then
/// decisions and gotchas), `?2` the project key, then the borrowed uids (JSON) when
/// `borrowed`, then the excluded extractor when `exclude`, then the fact ids (JSON) when
/// `ids`. `truth`: the index has 0035's tables (see [`truth::has_truth_tables`]);
/// without them every fact reads as live. `playbooks`: keep unpinned playbook gotchas
/// ([`MemoryQuery::playbooks`]).
#[allow(clippy::too_many_arguments)]
fn project_facts_sql(
    conn: &Connection,
    truth: bool,
    kind: bool,
    borrowed: bool,
    open_only: bool,
    exclude: bool,
    slice: Slice,
    ids: bool,
    playbooks: bool,
) -> String {
    // Match the canonical project key (callers pass it; falls back to project_path until
    // backfill), so all of one repo's threads aggregate together. `borrowed` (a JSON array of
    // uids, see [`repo_memory::BORROWED`]) adds the repo memory facts this project's files
    // share with another clone of the same repo, which hold them under that clone's key.
    let scope = if borrowed {
        "(COALESCE(t.project_key, t.project_path) = ?2
              OR f.uid IN (SELECT value FROM json_each(?3)))"
    } else {
        "COALESCE(t.project_key, t.project_path) = ?2"
    };
    let kind_sql = if kind {
        "f.kind = ?1"
    } else {
        "f.kind IN ('decision', 'gotcha') AND ?1 = ''"
    };
    // In SQL, so the caller's cap counts only facts of the slice.
    let filter = match (truth, slice) {
        (false, _) => String::new(),
        (true, Slice::Live) => format!(
            " AND NOT {} AND (f.pinned = 1 OR f.edited = 1 OR NOT {})",
            truth::SUPERSEDED_SQL,
            truth::FLAGGED_SQL
        ),
        (true, Slice::Superseded) => format!(" AND {}", truth::SUPERSEDED_SQL),
        (true, Slice::Flagged) => format!(
            " AND {} AND NOT {}",
            truth::FLAGGED_SQL,
            truth::SUPERSEDED_SQL
        ),
    };
    // Pinned facts on subagent threads stay: the user curated them (see list_open_todos).
    let mut sql = format!(
        "SELECT f.id, f.thread_id, f.text, t.title, f.created_at, f.pinned, f.extractor, {},
                f.kind, COALESCE(f.uid, ''), s.kind, {DECIDED_AT_SQL}, f.edited
         FROM facts f JOIN threads t ON t.id = f.thread_id
         JOIN sources s ON s.id = t.source_id
         WHERE {kind_sql} AND {scope} AND f.hidden = 0
           AND (t.is_subagent = 0 OR f.pinned = 1){filter}",
        origin_ref_sql(conn)
    );
    if open_only {
        sql.push_str(" AND f.status = 'open'");
    }
    if exclude {
        sql.push_str(&format!(
            " AND f.extractor != ?{}",
            3 + usize::from(borrowed)
        ));
    }
    if ids {
        sql.push_str(&format!(
            " AND f.id IN (SELECT value FROM json_each(?{}))",
            3 + usize::from(borrowed) + usize::from(exclude)
        ));
    }
    // Before the cap and the dedupe, like `exclude`: playbooks are filed under the session
    // that made the fix, so they are among a repo's newest gotchas and would fill the window.
    if !playbooks {
        sql.push_str(&format!(
            " AND (f.extractor != '{EXTRACTOR_PLAYBOOK}' OR f.pinned = 1)"
        ));
    }
    // Pinned facts first (the user's trusted set), then by session time, so a backlog
    // distilled newest-first still ranks a reversal above the decision it reversed.
    // [`project_facts`] moves kept unpinned playbooks last after its cap, not here.
    sql.push_str(&format!(
        " ORDER BY f.pinned DESC, {} DESC, f.id DESC",
        truth::recency_sql(truth)
    ));
    sql
}

/// What a project memory read returns ([`get_project_memory_with`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct MemoryQuery<'a> {
    /// Cap on each list.
    pub per_kind: usize,
    /// Leave this extractor's facts out (in SQL): [`harvest::EXTRACTOR_CLAUDE_MEMORY`] for the
    /// surfaces Claude Code reads next to its own auto memory.
    pub exclude: Option<&'a str>,
    /// Also fill [`ProjectMemory::superseded`] and [`ProjectMemory::flagged`] (the Projects
    /// page). Injection surfaces leave it off.
    pub history: bool,
    /// Count the project's distillable and distilled threads (about 0.85 ms; the hook skips
    /// it).
    pub coverage: bool,
    /// Keep unpinned failure playbooks ([`EXTRACTOR_PLAYBOOK`]), after every other fact of
    /// their kind (moved there after the cap, so a capped list keeps the newest facts, as
    /// before): lists a person reads and curates (the Projects page, `cal memory`). Off
    /// (the default, and every `get_project_memory*` shorthand) for memory handed to an agent
    /// or written to a file, where only a pinned one stays. On a large index they grew from 8
    /// to 38 in one re-mine, took all 8 of the hook's gotcha slots in one repo, and none held:
    /// most named a commit made within a day after the error's last sighting.
    pub playbooks: bool,
}

/// A project's memory as `q` asks: the live lists always, the history lists and coverage
/// counts when asked. The other `get_project_memory*` functions are shorthands for this.
pub fn get_project_memory_with(
    conn: &Connection,
    project: &str,
    q: &MemoryQuery,
) -> Result<ProjectMemory> {
    // Read once for the kinds: one `app_config` row, empty for nearly every project.
    let borrowed = repo_memory::borrowed_uids(conn, project);
    let borrowed = (!borrowed.is_empty())
        .then(|| serde_json::to_string(&borrowed))
        .transpose()?;
    let b = borrowed.as_deref();
    let (per_kind, exclude, playbooks) = (q.per_kind, q.exclude, q.playbooks);
    let live = |kind: &str| {
        project_facts(
            conn,
            Some(kind),
            project,
            false,
            per_kind,
            exclude,
            b,
            Slice::Live,
            None,
            playbooks,
        )
    };
    let history = |slice| {
        if q.history {
            project_facts(
                conn, None, project, false, per_kind, exclude, b, slice, None, playbooks,
            )
        } else {
            Ok(Vec::new())
        }
    };
    let lists = [
        live("decision")?,
        live("gotcha")?,
        project_facts(
            conn,
            Some("todo"),
            project,
            true,
            per_kind,
            exclude,
            None,
            Slice::Live,
            None,
            playbooks,
        )?,
        history(Slice::Superseded)?,
        history(Slice::Flagged)?,
    ];
    // One batch of status lookups for every list (TODOs carry no links).
    let uids: Vec<&str> = lists
        .iter()
        .flatten()
        .filter(|(f, _)| f.kind != "todo")
        .map(|(_, u)| u.as_str())
        .collect();
    let truths = fact_truth(conn, &uids)?;
    let [decisions, gotchas, open_todos, superseded, flagged] = lists.map(|list| {
        list.into_iter()
            .map(|(mut f, uid)| {
                if let Some(t) = truths.get(&uid) {
                    f.apply_truth(t.clone());
                }
                f
            })
            .collect::<Vec<_>>()
    });
    let mut mem = ProjectMemory {
        decisions,
        gotchas,
        open_todos,
        superseded,
        flagged,
        thread_count: 0,
        distilled_count: 0,
        pending_count: 0,
        project: project.to_string(),
    };
    if q.coverage {
        (mem.thread_count, mem.distilled_count) = project_coverage(conn, project)?;
        mem.pending_count = (mem.thread_count - mem.distilled_count).max(0);
    }
    Ok(mem)
}

/// Aggregate a project's distilled memory. `per_kind` caps each list (decisions/gotchas/
/// todos). Coverage counts are over "distillable" threads so the UI can show N/M distilled.
pub fn get_project_memory(
    conn: &Connection,
    project: &str,
    per_kind: usize,
) -> Result<ProjectMemory> {
    project_memory(conn, project, per_kind, None)
}

/// [`get_project_memory`] without one extractor's facts (see [`project_facts`] for why the
/// filter runs in SQL). For surfaces Claude Code reads next to its own auto memory, and
/// files people commit: [`harvest::EXTRACTOR_CLAUDE_MEMORY`].
pub fn get_project_memory_excluding(
    conn: &Connection,
    project: &str,
    per_kind: usize,
    extractor: &str,
) -> Result<ProjectMemory> {
    project_memory(conn, project, per_kind, Some(extractor))
}

fn project_memory(
    conn: &Connection,
    project: &str,
    per_kind: usize,
    exclude: Option<&str>,
) -> Result<ProjectMemory> {
    get_project_memory_with(
        conn,
        project,
        &MemoryQuery {
            per_kind,
            exclude,
            coverage: true,
            ..Default::default()
        },
    )
}

/// A project's distillable thread count and how many of them are distilled.
pub fn project_coverage(conn: &Connection, project: &str) -> Result<(i64, i64)> {
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
    Ok((count("")?, count(" AND knowledge_extracted = 1")?))
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
    project_memory_facts(conn, project, per_kind, None)
}

/// [`get_project_memory_facts`] without one extractor's facts, filtered in SQL (see
/// [`get_project_memory_excluding`]).
pub fn get_project_memory_facts_excluding(
    conn: &Connection,
    project: &str,
    per_kind: usize,
    extractor: &str,
) -> Result<ProjectMemory> {
    project_memory_facts(conn, project, per_kind, Some(extractor))
}

fn project_memory_facts(
    conn: &Connection,
    project: &str,
    per_kind: usize,
    exclude: Option<&str>,
) -> Result<ProjectMemory> {
    get_project_memory_with(
        conn,
        project,
        &MemoryQuery {
            per_kind,
            exclude,
            ..Default::default()
        },
    )
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
    Ok(record_fact_with_origin(
        conn,
        project,
        kind,
        text,
        rationale,
        FactOrigin::Agent,
        None,
        now,
    )?
    .id)
}

/// What [`record_fact_with_origin`] wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordedFact {
    /// The new fact's id (the caller embeds it via embed_pending_facts).
    pub id: i64,
    /// The `replaces` link's outcome, when one was asked for: stored as `supersedes`, or
    /// downgraded to `contradicts` (an agent's target was pinned or edited, or
    /// [`Self::capped`]), or rejected.
    pub link: Option<LinkOutcome>,
    /// The project already took [`AGENT_SUPERSEDE_CAP`] agent replacements within the window,
    /// so this one was asked as `contradicts` (disputed, for the user to settle) instead.
    pub capped: bool,
}

/// Record a fact (kind 'decision' or 'gotcha') for a project. Stored in a synthetic
/// per-project "Recorded memory" thread and left ready to embed, so it flows through
/// Project Memory and cross-thread recall like a distilled fact. User facts are pinned;
/// agent facts rank by recency like distilled ones.
///
/// `replaces` (a fact id): the new fact supersedes it, linked with source `user` or `agent`
/// by origin through [`link_facts`], so an agent's link to a pinned or edited fact is stored
/// as `contradicts`, and so is an agent's link past [`AGENT_SUPERSEDE_CAP`]. The fact is recorded whatever the link's outcome (a refused link is
/// reported, not an error), but an error while linking records nothing: the caller sees a
/// failure only when nothing was written, so an agent that retries cannot duplicate it.
#[allow(clippy::too_many_arguments)]
pub fn record_fact_with_origin(
    conn: &Connection,
    project: &str,
    kind: &str,
    text: &str,
    rationale: Option<&str>,
    origin: FactOrigin,
    replaces: Option<i64>,
    now: i64,
) -> Result<RecordedFact> {
    if !matches!(kind, "decision" | "gotcha") {
        anyhow::bail!("record_fact kind must be 'decision' or 'gotcha'");
    }
    truth::atomic(conn, |conn| {
        record_fact_once(conn, project, kind, text, rationale, origin, replaces, now)
    })
}

#[allow(clippy::too_many_arguments)]
fn record_fact_once(
    conn: &Connection,
    project: &str,
    kind: &str,
    text: &str,
    rationale: Option<&str>,
    origin: FactOrigin,
    replaces: Option<i64>,
    now: i64,
) -> Result<RecordedFact> {
    let tid = notes_thread_id(conn, project, now)?;
    conn.execute(
        "INSERT INTO facts
            (thread_id, kind, text, rationale, status, extractor, pinned, created_at, uid)
         VALUES (?1, ?2, ?3, ?4, 'open', ?5, ?6, ?7, ?8)",
        params![
            tid,
            kind,
            text.trim(),
            rationale.map(str::trim),
            origin.extractor(),
            i64::from(origin == FactOrigin::User),
            now,
            new_fact_uid()
        ],
    )?;
    let id = conn.last_insert_rowid();
    let mut capped = false;
    let link = match replaces {
        Some(old) => {
            let source = match origin {
                FactOrigin::User => LinkSource::User,
                FactOrigin::Agent => LinkSource::Agent,
            };
            // Counted inside this transaction, so two writers cannot both take the last slot.
            capped = origin == FactOrigin::Agent
                && truth::agent_supersedes_recent(conn, project, now)? >= AGENT_SUPERSEDE_CAP;
            let kind = if capped {
                LinkKind::Contradicts
            } else {
                LinkKind::Supersedes
            };
            Some(link_facts(conn, id, old, kind, source, None, now)?)
        }
        None => None,
    };
    // A refused link wrote nothing, so the cap did not change what was stored.
    let capped = capped && link.is_some_and(|l| l.link_id().is_some());
    Ok(RecordedFact { id, link, capped })
}

/// `facts.extractor` of a failure playbook: the gotcha `issues::refresh_clusters` records when a
/// recurring error was fixed by a linked commit. `origin_ref` is
/// `playbook:<signature>@<project key>` ([`crate::issues::playbook_ref`]), one fact per signature
/// and project, so a fix in another repo never moves this repo's fact.
///
/// An unpinned one takes no slot in memory handed to an agent or written to a file
/// ([`MemoryQuery::playbooks`]): the recorded fix is a commit right after the error's latest
/// sighting, which is not evidence the error stopped (on a large index 35 of 38 named one
/// within a week after it, and 3 were seen again). The lists a person reads keep them, and
/// the error's own readers (`issues::playbooks_for`, `recurring_issues`, topic recall) find them.
pub const EXTRACTOR_PLAYBOOK: &str = "playbook";

/// Record or refresh a playbook gotcha, one per `origin_ref` (see [`EXTRACTOR_PLAYBOOK`]),
/// filed under `thread_id` (the session that made the fix, so it cites it and lands in that
/// project's memory). A new one is unpinned. An existing one is updated in place, keeping its
/// uid, pin and hidden flag, and re-embedded when its text changed; one whose text the user
/// edited is left as it is. Returns whether anything was written.
pub fn upsert_playbook(
    conn: &Connection,
    origin_ref: &str,
    thread_id: i64,
    text: &str,
    now: i64,
) -> Result<bool> {
    let existing: Option<(i64, String, i64, i64)> = conn
        .query_row(
            "SELECT id, text, thread_id, edited FROM facts WHERE origin_ref = ?1
             ORDER BY id LIMIT 1",
            [origin_ref],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    match existing {
        None => {
            conn.execute(
                "INSERT INTO facts (thread_id, kind, text, status, extractor, origin_ref,
                                    created_at, uid)
                 VALUES (?1, 'gotcha', ?2, 'open', ?3, ?4, ?5, ?6)",
                params![
                    thread_id,
                    text,
                    EXTRACTOR_PLAYBOOK,
                    origin_ref,
                    now,
                    new_fact_uid()
                ],
            )?;
            Ok(true)
        }
        Some((_, _, _, edited)) if edited != 0 => Ok(false),
        Some((_, old_text, old_thread, _)) if old_text == text && old_thread == thread_id => {
            Ok(false)
        }
        Some((id, old_text, _, _)) => {
            if old_text == text {
                conn.execute(
                    "UPDATE facts SET thread_id = ?2 WHERE id = ?1",
                    params![id, thread_id],
                )?;
            } else {
                conn.execute(
                    "UPDATE facts SET thread_id = ?2, text = ?3, embedded = 0 WHERE id = ?1",
                    params![id, thread_id, text],
                )?;
                // The old vector describes the old text; the embed pass writes a new one.
                conn.execute("DELETE FROM vec_facts WHERE fact_id = ?1", [id])?;
            }
            Ok(true)
        }
    }
}

/// The project's synthetic "Recorded memory" thread (created on first use, `updated_at`
/// bumped on each write), which holds facts that came from no transcript of their own:
/// recorded ones and imported ones ([`harvest`]).
pub(crate) fn notes_thread_id(conn: &Connection, project: &str, now: i64) -> Result<i64> {
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
    Ok(conn.query_row(
        "SELECT id FROM threads WHERE source_id = ?1 AND external_id = ?2",
        params![source_id, ext],
        |r| r.get(0),
    )?)
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

/// A project's visible decisions that are not superseded, with when each was decided, newest
/// first: what conflict review compares (a pair a supersede already settled is not a
/// conflict any more).
pub fn project_decisions_dated(
    conn: &Connection,
    project: &str,
) -> Result<Vec<(i64, String, i64)>> {
    let live = if truth::has_truth_tables(conn) {
        format!(" AND NOT {}", truth::SUPERSEDED_SQL)
    } else {
        String::new()
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT f.id, f.text, {DECIDED_AT_SQL} AS decided
         FROM facts f JOIN threads t ON t.id = f.thread_id
         WHERE f.kind = 'decision' AND COALESCE(t.project_key, t.project_path) = ?1
           AND f.hidden = 0{live}
         ORDER BY decided DESC, f.id DESC"
    ))?;
    let rows = stmt.query_map([project], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// IDs of threads ANYWHERE that need distilling: never distilled, invalidated by new turns
/// (the indexer resets `knowledge_extracted`; the old facts keep serving until the re-distill
/// replaces them), or distilled when
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
    pending_threads_window(conn, limit, since, None)
}

/// The WHERE clause `pending_threads` documents, shared with the auto-distill idle deadline
/// so both agree on which threads are waiting.
fn pending_predicate() -> String {
    format!(
        "{DISTILLABLE} AND knowledge_error IS NULL
           AND (knowledge_extracted = 0
                OR (knowledge_msg_count != distillable_count
                    AND (knowledge_extracted_at IS NULL OR updated_at IS NULL
                         OR updated_at > knowledge_extracted_at)))"
    )
}

/// `pending_threads_since`, also skipping threads updated at or after `idle_before` (a
/// session still in progress). A NULL updated_at counts as idle.
fn pending_threads_window(
    conn: &Connection,
    limit: i64,
    since: Option<i64>,
    idle_before: Option<i64>,
) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id FROM threads
         WHERE {}
           AND (?2 IS NULL OR updated_at >= ?2)
           AND (?3 IS NULL OR updated_at IS NULL OR updated_at < ?3)
         ORDER BY updated_at DESC
         LIMIT ?1",
        pending_predicate()
    ))?;
    let rows = stmt.query_map(params![limit, since, idle_before], |r| r.get::<_, i64>(0))?;
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

/// Auto-distill waits until a thread has been idle this long (by its last message time).
/// A live session changes every turn: distilling it mid-flight re-packs and re-sends the
/// whole thread for each kick, and the result is stale a turn later. Its earlier facts keep
/// serving in the meantime, because the indexer only marks them stale.
pub const AUTO_DISTILL_IDLE_SECS: i64 = 30 * 60;

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

/// The next auto-distill batch: at most `batch` recently active pending threads that have
/// gone idle ([`AUTO_DISTILL_IDLE_SECS`]), and never
/// more than today's remaining allowance. Sessions in the priority queue (they just ended,
/// see [`priority`]) come first, whatever their age or idle time. The returned ids are
/// counted against the
/// allowance immediately, so a cancelled run errs on the side of spending less. Returns an
/// empty list once the day's allowance is used up, which ends the auto-distill drain.
pub fn auto_distill_worklist(conn: &Connection, batch: i64, now: i64) -> Result<Vec<i64>> {
    let take = batch.min(auto_distill_remaining_today(conn, now)?);
    if take <= 0 {
        return Ok(Vec::new());
    }
    let mut ids = priority::take_priority(conn, take, now)?;
    let room = take - ids.len() as i64;
    if room > 0 {
        let since = now - AUTO_DISTILL_RECENT_DAYS * 86_400;
        // `take` rows, not `room`: up to all of the queued ones can be in this list too.
        let regular =
            pending_threads_window(conn, take, Some(since), Some(now - AUTO_DISTILL_IDLE_SECS))?;
        let fresh: Vec<i64> = regular.into_iter().filter(|id| !ids.contains(id)).collect();
        ids.extend(fresh.into_iter().take(room as usize));
    }
    if !ids.is_empty() {
        let used = auto_distill_used_today(conn, now)? + ids.len() as i64;
        app_config::put(conn, "knowledge.auto_distill_day", &utc_day(now))?;
        app_config::put(conn, "knowledge.auto_distill_used", &used.to_string())?;
    }
    Ok(ids)
}

/// How many threads the auto-distill worklist would take at `now` with no daily cap: the
/// same recent-and-idle filter, so Settings does not count live sessions it will skip.
pub fn auto_distill_eligible_count(conn: &Connection, now: i64) -> Result<usize> {
    let since = now - AUTO_DISTILL_RECENT_DAYS * 86_400;
    let regular = pending_threads_window(
        conn,
        i64::MAX,
        Some(since),
        Some(now - AUTO_DISTILL_IDLE_SECS),
    )?;
    // Plus queued sessions the recent-and-idle filter leaves out (read-only: Settings
    // asks on a pooled reader).
    let queued = priority::ready_priority(conn, now)?
        .into_iter()
        .filter(|id| !regular.contains(id))
        .count();
    Ok(regular.len() + queued)
}

/// Margin past the idle edge before the deadline kick, so a clock tick of skew cannot land
/// it one second early and find the thread still live.
const AUTO_DISTILL_IDLE_SLACK_SECS: i64 = 5;

/// When the earliest pending thread the worklist skipped as live becomes idle (unix
/// seconds), or None when no pending thread is waiting on the idle filter. No other kick
/// follows a session going quiet (the watcher's top-up embed lands about a minute after the
/// last write, still inside the window), so without this the day's last session waited for
/// some unrelated later kick. A thread written again in the meantime only moves its own
/// edge later, so a kick at this deadline is never late; when it finds nothing, the caller
/// asks again.
pub fn auto_distill_next_idle_at(conn: &Connection, now: i64) -> Result<Option<i64>> {
    let earliest: Option<i64> = conn.query_row(
        &format!(
            "SELECT MIN(updated_at) FROM threads
             WHERE {} AND updated_at >= ?1",
            pending_predicate()
        ),
        [now - AUTO_DISTILL_IDLE_SECS],
        |r| r.get(0),
    )?;
    Ok(earliest.map(|t| t + AUTO_DISTILL_IDLE_SECS + AUTO_DISTILL_IDLE_SLACK_SECS))
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
        // LIKE wildcards in the term itself are literal.
        args.push(Box::new(crate::scope::like_contains(t)));
        likes.push(format!("f.text LIKE ?{} ESCAPE '\\'", args.len()));
    }
    let mut sql = format!(
        "SELECT EXISTS (SELECT 1 FROM facts f JOIN threads t ON t.id = f.thread_id
         WHERE f.kind IN ('decision', 'gotcha') AND f.hidden = 0 AND f.embedded = 1
           AND ({})",
        likes.join(" OR ")
    );
    if let Some(cond) = crate::scope::ProjectScope::fragment(project).sql("t", &mut args) {
        sql.push_str(&format!(" AND {cond}"));
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

    /// A flagged fact carries `edited`, so the Projects page can say whether a pin or an edit
    /// keeps it in memory (or neither, and it is out) without inferring it from the live lists.
    #[test]
    fn memory_facts_say_whether_the_user_edited_them() {
        use truth::{flag_fact, FlagKind, SignalSource};
        let conn = temp_db();
        const P: &str = "/p/edited";
        let rec = |text: &str| record_fact(&conn, P, "decision", text, None, 1).unwrap();
        let (plain, pinned, edited) = (rec("plain one"), rec("pinned one"), rec("edited one"));
        set_fact_pinned(&conn, pinned, true).unwrap();
        edit_fact(&conn, edited, "edited one, reworded").unwrap();
        for id in [plain, pinned, edited] {
            flag_fact(
                &conn,
                id,
                FlagKind::Stale,
                SignalSource::User,
                None,
                None,
                2,
            )
            .unwrap();
        }
        let q = MemoryQuery {
            per_kind: 10,
            history: true,
            ..Default::default()
        };
        let mem = get_project_memory_with(&conn, P, &q).unwrap();
        let bits = |l: &[MemoryFact]| -> Vec<(i64, bool, bool)> {
            let mut v: Vec<_> = l.iter().map(|f| (f.id, f.pinned, f.edited)).collect();
            v.sort();
            v
        };
        assert_eq!(
            bits(&mem.flagged),
            vec![
                (plain, false, false),
                (pinned, true, false),
                (edited, false, true)
            ]
        );
        assert_eq!(
            bits(&mem.decisions),
            vec![(pinned, true, false), (edited, false, true)]
        );
        // `edited` is left out of the JSON when false, like the other additions.
        let json = |id: i64| {
            let f = mem.flagged.iter().find(|f| f.id == id).unwrap();
            serde_json::to_value(f).unwrap()
        };
        assert_eq!(json(edited)["edited"], serde_json::Value::Bool(true));
        assert!(json(plain).get("edited").is_none());
    }

    /// Unpinned playbooks take no slot in memory handed to an agent ([`MemoryQuery::playbooks`]):
    /// ten newer than every distilled gotcha leave the default read as it was, the cap
    /// counting only what survives the filter, and a pinned one stays, first. The lists a
    /// person reads keep the same facts as before, the unpinned playbooks moved after the
    /// other gotchas.
    #[test]
    fn unpinned_playbooks_take_no_slot_unless_the_reader_asks() {
        use crate::export::digest_test_support as dts;
        const P: &str = "/r/app";
        let mut conn = dts::temp_db("playbook_slots");
        let gotchas = ["Gotcha three", "Gotcha two", "Gotcha one"];
        for (i, text) in gotchas.iter().enumerate() {
            let age = 70_000 + 10_000 * i as i64;
            let t = dts::session(&mut conn, &format!("pb-g{i}"), P, "g", age, &["go"]);
            dts::fact(&conn, t, "gotcha", text, "llm");
        }
        let fixer = dts::session(&mut conn, "pb-fix", P, "fix", 600, &["fix it"]);
        let now = chrono::Utc::now().timestamp();
        let playbook = |i: i64| {
            format!("error[E0308]: mismatched types ({i}): seen 2 times; fixed in abc{i:04}")
        };
        for i in 0..10 {
            let origin = format!("playbook:sig{i}@{P}");
            upsert_playbook(&conn, &origin, fixer, &playbook(i), now + i).unwrap();
        }
        let pinned = "error[E0061]: wrong number of arguments: seen 3 times; fixed in def0000";
        upsert_playbook(&conn, "playbook:pinned@/r/app", fixer, pinned, now - 50).unwrap();
        conn.execute(
            "UPDATE facts SET pinned = 1 WHERE origin_ref = 'playbook:pinned@/r/app'",
            [],
        )
        .unwrap();

        let read = |per_kind: usize, playbooks: bool| {
            let q = MemoryQuery {
                per_kind,
                playbooks,
                ..Default::default()
            };
            get_project_memory_with(&conn, P, &q).unwrap().gotchas
        };
        let texts = |l: &[MemoryFact]| l.iter().map(|f| f.text.clone()).collect::<Vec<_>>();
        let expected: Vec<String> = std::iter::once(pinned)
            .chain(gotchas)
            .map(str::to_string)
            .collect();
        assert_eq!(texts(&read(4, false)), expected);
        assert_eq!(texts(&read(3, false)), expected[..3].to_vec());
        // Every shorthand the agent surfaces read keeps the default.
        assert_eq!(
            texts(&get_project_memory(&conn, P, 40).unwrap().gotchas),
            expected
        );
        assert_eq!(
            texts(&get_project_memory_facts(&conn, P, 40).unwrap().gotchas),
            expected
        );

        let all = read(20, true);
        assert_eq!(all.len(), 14);
        assert_eq!(texts(&all[..4]), expected);
        assert!(all[4..]
            .iter()
            .all(|f| f.extractor == EXTRACTOR_PLAYBOOK && !f.pinned));
        assert_eq!(all[4].text, playbook(9), "newest first among them");
        // Moved after the cap, not sorted last in SQL: with a cap below the count of the
        // other gotchas, the list keeps the newest facts as `cal memory --json` printed
        // them before, instead of losing every playbook to the older gotchas.
        assert_eq!(
            texts(&read(3, true)),
            [pinned.to_string(), playbook(9), playbook(8)]
        );
        let twelve = read(12, true);
        assert_eq!(texts(&twelve[..2]), [pinned, "Gotcha three"]);
        assert_eq!(
            texts(&twelve[2..]),
            (0..10).rev().map(playbook).collect::<Vec<_>>()
        );
    }

    /// The live-only memory read (the hook's hot path) checks supersession by an index probe
    /// per fact, never a scan of `fact_links` or `fact_signals`, for every shape of the query
    /// (the working set's, both kinds over a list of ids, included).
    #[test]
    fn live_memory_filter_probes_the_truth_indexes() {
        let conn = temp_db();
        for (kind, borrowed, exclude, ids) in [
            (true, false, false, false),
            (true, false, true, false),
            (true, true, true, false),
            (false, false, true, true),
            (false, true, false, true),
        ] {
            let sql = project_facts_sql(
                &conn,
                true,
                kind,
                borrowed,
                false,
                exclude,
                Slice::Live,
                ids,
                false,
            );
            let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let n = stmt.parameter_count();
            let args: Vec<String> = (0..n).map(|_| "x".to_string()).collect();
            let plan = stmt
                .query_map(rusqlite::params_from_iter(args.iter()), |r| {
                    r.get::<_, String>(3)
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
                .join(" | ");
            assert!(plan.contains("idx_fact_links_to"), "{plan}");
            assert!(plan.contains("idx_fact_signals_fact"), "{plan}");
            assert!(!plan.contains("SCAN lk"), "{plan}");
            assert!(!plan.contains("SCAN sg"), "{plan}");
            assert!(!plan.contains("SCAN nf"), "{plan}");
            if !borrowed {
                assert!(!plan.contains("SCAN f "), "{plan}");
            }
        }
    }

    #[test]
    fn search_facts_matches_every_word_pinned_first_in_one_project() {
        use crate::export::digest_test_support as dts;
        let mut conn = dts::temp_db("search_facts");
        let app = dts::session(&mut conn, "f1", "/r/app", "app", 600, &["go"]);
        let web = dts::session(&mut conn, "f2", "/r/app-web", "web", 600, &["go"]);
        let ins = |tid: i64, kind: &str, text: &str, at: i64, pinned: i64, hidden: i64| {
            conn.execute(
                "INSERT INTO facts (thread_id, kind, text, extractor, created_at, pinned, hidden)
                 VALUES (?1, ?2, ?3, 'llm', ?4, ?5, ?6)",
                params![tid, kind, text, at, pinned, hidden],
            )
            .unwrap();
            conn.last_insert_rowid()
        };
        let newer = ins(
            app,
            "decision",
            "Use Postgres for the Auth store",
            300,
            0,
            0,
        );
        let pinned = ins(
            app,
            "gotcha",
            "auth tokens expire; refresh in Postgres job",
            100,
            1,
            0,
        );
        ins(app, "decision", "Use Redis for auth sessions", 400, 0, 0);
        ins(app, "decision", "postgres auth (hidden)", 500, 0, 1);
        ins(app, "todo", "postgres auth todo", 500, 0, 0);
        let other = ins(web, "decision", "Postgres auth for web", 600, 0, 0);

        let ids = |hits: Vec<FactHit>| hits.into_iter().map(|h| h.id).collect::<Vec<_>>();
        // Every word, any case; pinned first, then newest; hidden facts and TODOs never.
        let hits = search_facts(&conn, "POSTGRES auth", Some("/r/app"), 10, None).unwrap();
        assert_eq!(hits[0].project, "/r/app");
        assert_eq!(ids(hits), [pinned, newer]);
        // The project is matched exactly: `/r/app` is not `/r/app-web`.
        assert_eq!(
            ids(search_facts(&conn, "postgres auth", None, 10, None).unwrap()),
            [pinned, other, newer]
        );
        assert_eq!(
            ids(search_facts(&conn, "postgres auth", None, 1, None).unwrap()),
            [pinned]
        );
        // LIKE wildcards in the query are literal.
        assert!(search_facts(&conn, "postgres%auth", None, 10, None)
            .unwrap()
            .is_empty());
        assert!(search_facts(&conn, "postgres_auth", None, 10, None)
            .unwrap()
            .is_empty());
        assert!(search_facts(&conn, "   ", None, 10, None)
            .unwrap()
            .is_empty());
        // Parsed like the search box: stopwords and punctuation drop out, a quoted phrase
        // must appear whole, and a query of stopwords alone finds nothing by keyword.
        assert_eq!(
            ids(search_facts(&conn, "how did we use postgres?", Some("/r/app"), 10, None).unwrap()),
            [newer]
        );
        assert_eq!(
            ids(search_facts(&conn, "\"refresh in postgres\"", None, 10, None).unwrap()),
            [pinned]
        );
        assert!(search_facts(&conn, "\"postgres refresh\"", None, 10, None)
            .unwrap()
            .is_empty());
        assert!(search_facts(&conn, "how did we", None, 10, None)
            .unwrap()
            .is_empty());

        // Off the kind index with each thread by its key, never a scan of facts or threads.
        for project in [None, Some("/r/app")] {
            let (sql, args) = fact_search_sql(&["postgres".into(), "auth".into()], project, 10);
            let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
            let plan = conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap()
                .query_map(refs.as_slice(), |r| r.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
                .join(" | ");
            assert!(
                plan.contains("SEARCH f USING INDEX idx_facts_kind (kind=?)"),
                "{plan}"
            );
            assert!(
                plan.contains("SEARCH t USING INTEGER PRIMARY KEY"),
                "{plan}"
            );
            assert!(!plan.contains("SCAN"), "{plan}");
        }
    }

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

    /// The app's own fences are never mined: the compaction anchor (its heading or a bare
    /// `<session_prompts>` block) quotes the user's prompts, and the distiller's
    /// `<existing_memory>` list quotes stored facts.
    #[test]
    fn the_anchor_and_the_distiller_memory_list_are_injected_context() {
        let anchor = format!(
            "{}\n<session_prompts>\n[2026-11-08 09:14] TODO: wire up auth\n</session_prompts>",
            crate::anchor::ANCHOR_HEADING
        );
        let bare = "<session_prompts omitted=\"3\">\n- [ ] add the hook test\n</session_prompts>";
        let memory = "<existing_memory session_started=\"2026-10-12\">\nD1 (2026-10-01): TODO: \
                      use JSON\n</existing_memory>";
        for text in [anchor.as_str(), bare, memory] {
            assert!(is_injected_context(text), "{text}");
            assert!(extract_todos(text).is_empty(), "{text}");
        }
        assert!(!is_injected_context(
            "TODO: a real prompt about <session_prompts"
        ));
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
        crate::db::test_support::TestIndex::new("know").into_connection()
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
            ..Default::default()
        };
        store_distilled(&mut conn, id, &d, &RefMap::default(), 100).unwrap();
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
        assert!(k2.stale, "shown as stale, not as never distilled");
        assert_eq!(
            k2.decisions.len(),
            1,
            "the old facts keep serving until the re-distill lands"
        );
        assert!(needs_distill(&conn, id).unwrap());

        // The re-distill replaces them in one transaction.
        let d2 = Distilled {
            summary: "did auth, then more".into(),
            decisions: vec!["use jwt with refresh".into()],
            gotchas: Vec::new(),
            ..Default::default()
        };
        store_distilled(&mut conn, id, &d2, &RefMap::default(), 200).unwrap();
        let k3 = get_thread_knowledge(&conn, id).unwrap();
        assert!(k3.extracted && !k3.stale);
        assert_eq!(k3.decisions.len(), 1);
        assert_eq!(k3.decisions[0].text, "use jwt with refresh");
        assert!(k3.gotchas.is_empty(), "replaced, not merged");
    }

    #[test]
    fn a_reworded_todo_survives_the_backfill_without_its_original() {
        use crate::indexer::{source_id, upsert_thread, ParsedMessage, ParsedThread};
        let mut conn = temp_db();
        let sid = source_id(&conn, "claude_code").unwrap();
        upsert_thread(
            &mut conn,
            sid,
            &ParsedThread {
                external_id: "edited".into(),
                messages: vec![ParsedMessage {
                    role: "user".into(),
                    text: "TODO: ship the docs\nTODO: tag the release".into(),
                    tool_name: None,
                    ts: Some(1),
                }],
                ..Default::default()
            },
        )
        .unwrap();
        let todos = |conn: &Connection| -> Vec<(String, String)> {
            conn.prepare("SELECT text, uid FROM facts WHERE kind = 'todo' ORDER BY text")
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        let before = todos(&conn);
        let docs: i64 = conn
            .query_row(
                "SELECT id FROM facts WHERE text = 'ship the docs'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        edit_fact(&conn, docs, "ship the docs site").unwrap();
        let db = crate::db::Db(std::sync::Mutex::new(conn));
        backfill_todos(&db, 5, &|| false).unwrap();
        let after = todos(&db.0.lock().unwrap());
        assert_eq!(
            after.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(),
            vec!["ship the docs site", "tag the release"]
        );
        assert_eq!(after[1].1, before[1].1, "the untouched TODO keeps its uid");
    }

    #[test]
    fn a_fact_distilled_again_keeps_its_uid() {
        use crate::agent::Distilled;
        use crate::indexer::{source_id, upsert_thread, ParsedMessage, ParsedThread};
        let mut conn = temp_db();
        let sid = source_id(&conn, "claude_code").unwrap();
        upsert_thread(
            &mut conn,
            sid,
            &ParsedThread {
                external_id: "uid1".into(),
                messages: vec![ParsedMessage {
                    role: "user".into(),
                    text: "hi".into(),
                    tool_name: None,
                    ts: Some(1),
                }],
                ..Default::default()
            },
        )
        .unwrap();
        let tid: i64 = conn
            .query_row("SELECT id FROM threads WHERE external_id='uid1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let uid_of = |conn: &Connection, kind: &str| -> Vec<(String, String)> {
            conn.prepare(
                "SELECT text, uid FROM facts WHERE thread_id = ?1 AND kind = ?2 ORDER BY seq",
            )
            .unwrap()
            .query_map(params![tid, kind], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
        };
        let first = Distilled {
            summary: "Built the exporter.".into(),
            decisions: vec!["Use SQLite".into(), "Ship weekly".into()],
            gotchas: vec!["WAL needs a checkpoint".into()],
            ..Default::default()
        };
        store_distilled(&mut conn, tid, &first, &RefMap::default(), 1).unwrap();
        let (decisions, gotchas, summary) = (
            uid_of(&conn, "decision"),
            uid_of(&conn, "gotcha"),
            uid_of(&conn, "summary"),
        );
        // The thread grew: a re-distill finds the same facts (spelled a little differently)
        // plus a new one.
        let again = Distilled {
            summary: "Built the exporter and its tests.".into(),
            decisions: vec!["use sqlite ".into(), "Pin the toolchain".into()],
            gotchas: vec!["WAL needs a checkpoint".into()],
            ..Default::default()
        };
        store_distilled(&mut conn, tid, &again, &RefMap::default(), 2).unwrap();
        let decisions2 = uid_of(&conn, "decision");
        assert_eq!(decisions2[0].1, decisions[0].1, "same decision, same uid");
        assert_ne!(
            decisions2[1].1, decisions[1].1,
            "a new decision gets its own"
        );
        assert_eq!(uid_of(&conn, "gotcha")[0].1, gotchas[0].1);
        assert_ne!(
            uid_of(&conn, "summary")[0].1,
            summary[0].1,
            "a changed summary is new"
        );
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
            ..Default::default()
        };
        store_distilled(&mut conn, tid, &d("use sqlite"), &RefMap::default(), 1).unwrap();
        let fid: i64 = conn
            .query_row(
                "SELECT id FROM facts WHERE thread_id=?1 AND kind='decision'",
                [tid],
                |r| r.get(0),
            )
            .unwrap();
        set_fact_pinned(&conn, fid, true).unwrap();

        // Re-distill with a DIFFERENT decision: the pinned one must survive.
        store_distilled(&mut conn, tid, &d("use postgres"), &RefMap::default(), 2).unwrap();
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
            None,
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

        let hits = recall(&conn, &v, "decision", None, 5, false).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text, "use jwt for auth");
        assert!(hits[0].similarity > 0.99);
        // Kind filter excludes non-matching kinds.
        assert!(recall(&conn, &v, "gotcha", None, 5, false)
            .unwrap()
            .is_empty());
    }

    /// Every close hit stays; a related one only from GUARD_RELATED, within the band of the
    /// best hit, two at most; nothing below the floor.
    #[test]
    fn guard_keeps_close_hits_and_few_near_related_ones() {
        let (c, r) = (GUARD_CLOSE, GUARD_RELATED);
        assert_eq!(guard_keep(&[0.95, 0.9, 0.8, c]), vec![true; 4]);
        // Next to a strong close hit, a related one outside the band is noise.
        assert_eq!(guard_keep(&[0.95, c - 0.01]), vec![true, false]);
        assert_eq!(
            guard_keep(&[c + 0.01, c - 0.01]),
            vec![true, true],
            "a near tie stays"
        );
        assert_eq!(
            guard_keep(&[r + 0.02, r + 0.01, r + 0.005, r - 0.01]),
            vec![true, true, false, false]
        );
        assert!(guard_keep(&[r - 0.001, 0.6]).iter().all(|k| !k));
        assert!(guard_keep(&[]).is_empty());
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
        let hits = recall(&conn, &qv, "decision", None, 5, false).unwrap();
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
            recall(&conn, &qv, "decision", Some("myproj"), 5, false)
                .unwrap()
                .len(),
            1,
            "scoped recall matches on project_key"
        );
        assert!(
            recall(&conn, &qv, "decision", Some("nope"), 5, false)
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

    fn heuristic_todos(conn: &Connection, thread_id: i64) -> Vec<(String, String)> {
        let mut stmt = conn
            .prepare(
                "SELECT text, status FROM facts WHERE thread_id = ?1 AND extractor = 'heuristic'
                 ORDER BY text",
            )
            .unwrap();
        stmt.query_map([thread_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// TODO extraction no longer waits for the distillation opt-in, and turning
    /// distillation off keeps the TODOs (it used to wipe every heuristic row).
    #[test]
    fn heuristic_todos_need_no_opt_in_and_survive_disable() {
        let mut conn = temp_db();
        assert!(!get_config(&conn).unwrap().enabled);
        let t = seed_thread(&mut conn, "h1", "/proj/h", false, &["TODO: ship the card"]);
        assert_eq!(
            heuristic_todos(&conn, t),
            vec![("ship the card".to_string(), "open".to_string())],
            "extracted at index time with distillation off"
        );
        set_config(&conn, true, Some("ollama"), Some("m")).unwrap();
        set_config(&conn, false, None, None).unwrap();
        assert_eq!(heuristic_todos(&conn, t).len(), 1, "disable keeps TODOs");
        assert_eq!(
            list_open_todos(&conn, None, None, None, 10).unwrap().len(),
            1
        );
    }

    /// The upgrade backfill runs once, whatever knowledge.enabled was, keeps curated TODOs,
    /// and waits while another writer job runs.
    #[test]
    fn todo_backfill_runs_once_and_keeps_curated_todos() {
        let mut conn = temp_db();
        let t = seed_thread(
            &mut conn,
            "b1",
            "/proj/b",
            false,
            &["TODO: first task", "TODO: second task"],
        );
        // An index from before TODOs were always on: none extracted, except one the user
        // closed back when they had knowledge on.
        conn.execute(
            "DELETE FROM facts WHERE thread_id = ?1 AND text = 'first task'",
            [t],
        )
        .unwrap();
        conn.execute(
            "UPDATE facts SET status = 'done' WHERE thread_id = ?1 AND text = 'second task'",
            [t],
        )
        .unwrap();
        let db = crate::db::Db(std::sync::Mutex::new(conn));
        let polls = std::cell::Cell::new(0);
        let pause = || {
            polls.set(polls.get() + 1);
            false
        };
        assert!(
            backfill_todos_once(&db, 5_000, &pause).unwrap(),
            "first launch runs it"
        );
        assert!(polls.get() >= 1, "asks before its batch");
        {
            let conn = db.0.lock().unwrap();
            assert_eq!(
                heuristic_todos(&conn, t),
                vec![
                    ("first task".to_string(), "open".to_string()),
                    ("second task".to_string(), "done".to_string()),
                ],
                "backfilled, and the closed TODO stays closed without a duplicate"
            );
            assert!(app_config::get_flag(&conn, TODOS_BACKFILLED).unwrap());
        }
        assert!(
            !backfill_todos_once(&db, 5_000, &|| false).unwrap(),
            "later launches skip it"
        );

        // An index that had knowledge on runs it too: the old on-enable pass kept no record
        // of finishing.
        let mut conn = temp_db();
        let t = seed_thread(&mut conn, "b2", "/proj/c", false, &["TODO: later task"]);
        conn.execute("DELETE FROM facts WHERE thread_id = ?1", [t])
            .unwrap();
        set_config(&conn, true, Some("claude-cli"), None).unwrap();
        let db = crate::db::Db(std::sync::Mutex::new(conn));
        assert!(backfill_todos_once(&db, 5_000, &|| false).unwrap());
        assert_eq!(heuristic_todos(&db.0.lock().unwrap(), t).len(), 1);
        assert!(app_config::get_flag(&db.0.lock().unwrap(), TODOS_BACKFILLED).unwrap());
    }

    /// An assistant's checklist line counts only when it names a file, symbol or identifier;
    /// a TODO marker and a user's own checklist are kept as they were.
    #[test]
    fn assistant_checklists_must_name_a_specific() {
        let text = "\
            - [ ] Post the clip on Tuesday morning\n\
            - [ ] Add captions before you upload\n\
            - [ ] wire `retry_backoff` into the uploader\n\
            - [ ] move the check into src/sync/queue.rs\n\
            - [ ] rename fetchUser to loadUser\n\
            - [ ] Close LIN-482 once the fix ships\n\
            - [ ] Bump PROJ-1234\n\
            - [ ] Fix issue #482 before the release\n\
            - [ ] Save the notes as utf-8\n\
            - [ ] Pin the plugin at v1-2 for now\n\
            TODO: ask the designer about the footer";
        let found = |assistant| -> Vec<String> {
            message_todos(text, assistant)
                .into_iter()
                .filter(|(_, e)| *e == "heuristic")
                .map(|(t, _)| t)
                .collect()
        };
        assert_eq!(
            found(true),
            vec![
                "wire `retry_backoff` into the uploader",
                "move the check into src/sync/queue.rs",
                "rename fetchUser to loadUser",
                // A ticket or an issue names this project's work; a version or an encoding
                // that only looks like one does not.
                "Close LIN-482 once the fix ships",
                "Bump PROJ-1234",
                "Fix issue #482 before the release",
                "ask the designer about the footer",
            ]
        );
        assert_eq!(found(false).len(), 11, "a user's checklist is theirs");
        assert_eq!(
            extract_todos(text).len(),
            11,
            "extract_todos is the user rule"
        );
    }

    #[test]
    fn followups_come_from_deferred_work_lists_only() {
        let text = "\
            ## Summary\n\
            - shipped the parser\n\
            \n\
            ## Known follow-ups, not in this PR\n\
            - cap the retry queue in `jobs.rs`\n\
            - [ ] add a Windows path test\n\
            - [x] already handled\n\
            - ~~dropped idea~~\n\
            - make the export lazy\n  \
              which needs the cursor API\n  \
              - a nested detail\n\
            That is all for now.\n\
            - not a follow-up any more\n\
            \n\
            **Deferred:**\n\
            1. port the hook to Cursor\n\
            2) document the budget\n\
            \n\
            Next steps:\n\
            - restart the app and check Settings\n\
            - bump `MAX_BATCH` to 64\n\
            \n\
            ### Remaining\n\
            * the `cal why` gutter\n\
            ### Changes\n\
            * not deferred\n\
            ```\n\
            ## Follow-ups\n\
            - inside a fence\n\
            ```\n\
            #### Following the plan\n\
            - not a follow-up heading";
        assert_eq!(
            extract_followups(text),
            vec![
                "cap the retry queue in `jobs.rs`",
                "add a Windows path test",
                "make the export lazy",
                "port the hook to Cursor",
                "document the budget",
                // Not "Next steps", even with a specific: those were setup instructions.
                "the `cal why` gutter",
            ]
        );
        // At most 12 per message, and nothing from injected context or a long title.
        let many: String = std::iter::once("## Follow-ups\n".to_string())
            .chain((0..20).map(|i| format!("- follow-up number {i}\n")))
            .collect();
        assert_eq!(extract_followups(&many).len(), MAX_FOLLOWUPS_PER_MESSAGE);
        let skill = format!("Base directory for this skill: /x\n\n{many}");
        assert!(extract_followups(&skill).is_empty());
        let long =
            "## Remaining questions about how the old scheduler handled clock drift on resume\n\
                    - one";
        assert!(extract_followups(long).is_empty());
        // A title that says its list is done introduces a summary of work, not deferred work;
        // the same words negated, or after a phrase that is not an adjective, do not. Nor
        // does one over risks or questions, or one that says nothing is left.
        for done in [
            "**Follow-ups, all merged:**",
            "## Follow-up fixes",
            "Follow-up changes:",
            "### Remaining: none, all done",
            "**Deferred items (now merged)**",
            "Follow-up commit `abc1234`:",
            "**Follow-up fix:**",
            "## Remaining risks",
            "Follow-up questions:",
            "## Remaining work is none",
            "**Follow-ups: nothing left to do**",
            "## Follow-ups: none needed",
        ] {
            assert_eq!(followup_title(done), None, "{done}");
            let list = format!("{done}\n- moved the retry cap into the scheduler");
            assert!(extract_followups(&list).is_empty(), "{done}");
        }
        for open in [
            "## Follow-ups (not merged yet)",
            "## Remaining fixes",
            "**Follow-ups (fixes for later):**",
            "### Deferred (needs a decision)",
            "Not done yet, by risk:",
            // "none" / "nothing" qualifying the items, not saying none are left.
            "**Follow-ups (none blocking):**",
            "## Remaining (nothing urgent)",
            "Remaining: nothing blocking, but two nice-to-haves:",
        ] {
            assert!(followup_title(open).is_some(), "{open}");
        }
        // Only an assistant's message yields them.
        assert!(message_todos(&many, false).is_empty());
        assert!(message_todos(&many, true)
            .iter()
            .all(|(_, e)| *e == EXTRACTOR_FOLLOWUP));
    }

    /// A bullet ending in `:` is a label: the bullets nested directly under it are the items,
    /// while those under an ordinary item stay its details, and a label with nothing nested
    /// yields nothing.
    #[test]
    fn a_label_bullet_yields_the_items_nested_under_it() {
        let text = "\
            ## Follow-ups\n\
            - cap the retry queue in `jobs.rs`\n  \
              - a detail of the cap\n\
            - **Two questions are still open:**\n  \
              - port the hook to Cursor\n  \
              - document the budget\n    \
                - a detail of the budget";
        assert_eq!(
            extract_followups(text),
            [
                "cap the retry queue in `jobs.rs`",
                "port the hook to Cursor",
                "document the budget",
            ]
        );
        let bare = "## Remaining\n- Still open:\n- keep the log quiet\n- Also open:";
        assert_eq!(extract_followups(bare), ["keep the log quiet"]);
        // A label under a label is one too, not an item that hides the real ones.
        let nested = "\
            ## Follow-ups\n\
            - **Open:**\n  \
              - Backend:\n    \
                - add the retry\n      \
                  - a detail of the retry\n  \
              - Frontend:\n    \
                - add the toast";
        assert_eq!(
            extract_followups(nested),
            ["add the retry", "add the toast"]
        );
    }

    fn todo_rows(conn: &Connection, thread_id: i64) -> Vec<(String, String, String)> {
        conn.prepare(
            "SELECT text, extractor, status FROM facts WHERE thread_id = ?1 AND kind = 'todo'
             ORDER BY text",
        )
        .unwrap()
        .query_map([thread_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    }

    /// Follow-ups are derived at index time, keep their uid through a full re-index, and a
    /// closed one stays closed without an open copy.
    #[test]
    fn followups_are_indexed_and_survive_a_reindex_curated() {
        let mut conn = temp_db();
        let text = "## Follow-ups\n- cap the retry queue\n- add a Windows path test";
        // Written an hour ago: the indexer judges a list's age against the clock.
        let now = chrono::Utc::now().timestamp();
        let t = seed_timed(
            &mut conn,
            "f1",
            "/proj/f",
            false,
            &[("go", Some(now - 3_600)), (text, Some(now - 3_599))],
        );
        assert_eq!(
            todo_rows(&conn, t),
            vec![
                (
                    "add a Windows path test".into(),
                    "followup".into(),
                    "open".into()
                ),
                (
                    "cap the retry queue".into(),
                    "followup".into(),
                    "open".into()
                ),
            ]
        );
        let uid = |conn: &Connection, text: &str| -> String {
            conn.query_row("SELECT uid FROM facts WHERE text = ?1", [text], |r| {
                r.get(0)
            })
            .unwrap()
        };
        let before = uid(&conn, "cap the retry queue");
        conn.execute(
            "UPDATE facts SET status = 'done' WHERE text = 'add a Windows path test'",
            [],
        )
        .unwrap();
        // The listing carries the extractor, under the schema it always had.
        let listed = list_open_todos(&conn, None, Some("/proj/f"), None, 10).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].extractor, EXTRACTOR_FOLLOWUP);
        {
            let tx = conn.transaction().unwrap();
            rebuild_thread_todos(&tx, t, now).unwrap();
            tx.commit().unwrap();
        }
        assert_eq!(uid(&conn, "cap the retry queue"), before);
        assert_eq!(
            todo_rows(&conn, t),
            vec![
                (
                    "add a Windows path test".into(),
                    "followup".into(),
                    "done".into()
                ),
                (
                    "cap the retry queue".into(),
                    "followup".into(),
                    "open".into()
                ),
            ]
        );
    }

    /// The follow-up texts of a thread, in text order.
    fn followup_texts(conn: &Connection, thread_id: i64) -> Vec<String> {
        todo_rows(conn, thread_id)
            .into_iter()
            .filter(|r| r.1 == EXTRACTOR_FOLLOWUP)
            .map(|r| r.0)
            .collect()
    }

    /// (text, ts) for each message, the first written at `ts`, one second apart.
    fn timed<'a>(texts: &[&'a str], ts: i64) -> Vec<(&'a str, Option<i64>)> {
        texts
            .iter()
            .enumerate()
            .map(|(i, t)| (*t, Some(ts + i as i64)))
            .collect()
    }

    /// A thread's follow-ups are its latest deferred-work list's items, on a full derive and
    /// on an appended tail (where the earlier list's rows are already in the table and must
    /// go); an item both lists hold keeps its uid; heuristic TODOs keep accumulating.
    #[test]
    fn only_the_latest_follow_up_list_counts() {
        let now = chrono::Utc::now().timestamp();
        let l1 = "TODO: tidy the first marker\n## Follow-ups\n- port the hook to Cursor\n\
                  - keep the log quiet";
        let l2 = "TODO: tidy the second marker\n## Remaining\n- document the budget\n\
                  - cap the retry queue";
        let l3 = "TODO: tidy the third marker\n**Deferred:**\n- cap the retry queue\n\
                  - add a Windows path test";
        let msgs = timed(&["go", l1, "and?", l2, "next?", l3], now - 3_600);
        let latest = ["add a Windows path test", "cap the retry queue"];
        let markers = [
            "tidy the first marker",
            "tidy the second marker",
            "tidy the third marker",
        ];
        let marker_texts = |conn: &Connection, t: i64| -> Vec<String> {
            heuristic_todos(conn, t).into_iter().map(|r| r.0).collect()
        };
        let uid = |conn: &Connection, t: i64| -> String {
            conn.query_row(
                "SELECT uid FROM facts WHERE thread_id = ?1 AND text = 'cap the retry queue'",
                [t],
                |r| r.get(0),
            )
            .unwrap()
        };

        // A full derive of the three lists.
        let mut conn = temp_db();
        let full = seed_timed(&mut conn, "l1", "/proj/l", false, &msgs);
        assert_eq!(followup_texts(&conn, full), latest);
        assert_eq!(marker_texts(&conn, full), markers);

        // Two lists indexed, then the third appended.
        let tail = seed_timed(&mut conn, "l2", "/proj/l", false, &msgs[..4]);
        assert_eq!(
            followup_texts(&conn, tail),
            ["cap the retry queue", "document the budget"]
        );
        let shared = uid(&conn, tail);
        let first_message = |conn: &Connection| -> i64 {
            conn.query_row(
                "SELECT id FROM messages WHERE thread_id = ?1 AND seq = 0",
                [tail],
                |r| r.get(0),
            )
            .unwrap()
        };
        let first = first_message(&conn);
        seed_timed(&mut conn, "l2", "/proj/l", false, &msgs);
        assert_eq!(first_message(&conn), first, "appended, not replaced");
        assert_eq!(followup_texts(&conn, tail), latest);
        assert_eq!(uid(&conn, tail), shared, "the item both lists hold");
        assert_eq!(marker_texts(&conn, tail), markers);

        // A full re-derive of either keeps the same rows, and the uid.
        {
            let tx = conn.transaction().unwrap();
            rebuild_thread_todos(&tx, full, now).unwrap();
            rebuild_thread_todos(&tx, tail, now).unwrap();
            tx.commit().unwrap();
        }
        for t in [full, tail] {
            assert_eq!(followup_texts(&conn, t), latest);
            assert_eq!(marker_texts(&conn, t), markers);
        }
        assert_eq!(uid(&conn, tail), shared);
    }

    /// A done, pinned or hidden follow-up from an earlier list survives a later list and is
    /// not copied by it, on an appended tail and on a full re-derive, while the open one the
    /// later list leaves out goes. (This pinned an edited one too, and a sibling of it the
    /// later list left out staying; an edited row now freezes the thread's follow-ups:
    /// `a_reworded_todo_freezes_its_threads_follow_ups`.)
    #[test]
    fn curated_follow_ups_survive_a_later_list() {
        let now = chrono::Utc::now().timestamp();
        let items = "- close the done one\n- pin the pinned one\n- hide the hidden one";
        let l1 =
            format!("TODO: tidy the first marker\n## Follow-ups\n{items}\n- drop the open one");
        let l2 = format!("TODO: tidy the second marker\n## Remaining\n{items}\n- add the new one");
        let msgs = timed(&["go", l1.as_str(), "and?", l2.as_str()], now - 3_600);
        let mut conn = temp_db();
        let t = seed_timed(&mut conn, "c1", "/proj/c", false, &msgs[..2]);
        for sql in [
            "UPDATE facts SET status = 'done' WHERE text = 'close the done one'",
            "UPDATE facts SET pinned = 1 WHERE text = 'pin the pinned one'",
            "UPDATE facts SET hidden = 1 WHERE text = 'hide the hidden one'",
        ] {
            assert_eq!(conn.execute(sql, []).unwrap(), 1, "{sql}");
        }
        // (text, status, pinned, hidden, edited) of each follow-up.
        type Row = (String, String, i64, i64, i64);
        let rows = |conn: &Connection| -> Vec<Row> {
            conn.prepare(
                "SELECT text, status, pinned, hidden, edited FROM facts
                 WHERE thread_id = ?1 AND extractor = 'followup' ORDER BY text",
            )
            .unwrap()
            .query_map([t], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
        };
        let row = |text: &str, status: &str, pinned, hidden, edited| -> Row {
            (text.into(), status.into(), pinned, hidden, edited)
        };
        let expected = vec![
            row("add the new one", "open", 0, 0, 0),
            row("close the done one", "done", 0, 0, 0),
            row("hide the hidden one", "open", 0, 1, 0),
            row("pin the pinned one", "open", 1, 0, 0),
        ];
        seed_timed(&mut conn, "c1", "/proj/c", false, &msgs);
        assert_eq!(rows(&conn), expected, "appended");
        {
            let tx = conn.transaction().unwrap();
            rebuild_thread_todos(&tx, t, now).unwrap();
            tx.commit().unwrap();
        }
        assert_eq!(rows(&conn), expected, "a full re-derive");
        assert_eq!(
            heuristic_todos(&conn, t),
            [
                ("tidy the first marker".to_string(), "open".to_string()),
                ("tidy the second marker".to_string(), "open".to_string()),
            ]
        );
    }

    /// Two recent lists both hold an item and the user rewords it in the latest list's row: a
    /// full re-derive and the re-judge each leave exactly the rows that were there, the edited
    /// row and its sibling, and no open copy of the original wording (which a row the derive
    /// itself added at the earlier list and then removed used to bring back).
    #[test]
    fn a_reworded_follow_up_is_never_copied_back() {
        let now = chrono::Utc::now().timestamp();
        let msgs = timed(
            &[
                "go",
                "## Follow-ups\n- cap the retry queue",
                "and?",
                "## Remaining\n- cap the retry queue\n- add a Windows path test",
            ],
            now - 3_600,
        );
        let mut conn = temp_db();
        let t = seed_timed(&mut conn, "w1", "/proj/w", false, &msgs);
        let cap: i64 = conn
            .query_row(
                "SELECT id FROM facts WHERE thread_id = ?1 AND text = 'cap the retry queue'",
                [t],
                |r| r.get(0),
            )
            .unwrap();
        edit_fact(&conn, cap, "cap the queue at 50").unwrap();
        // (text, status, edited, source seq, uid) of each TODO.
        type Row = (String, String, i64, i64, String);
        let rows = |conn: &Connection| -> Vec<Row> {
            conn.prepare(
                "SELECT text, status, edited, source_seq, uid FROM facts
                 WHERE thread_id = ?1 AND kind = 'todo' ORDER BY text",
            )
            .unwrap()
            .query_map([t], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
        };
        let edited = rows(&conn);
        assert_eq!(
            edited
                .iter()
                .map(|r| (r.0.as_str(), r.2, r.3))
                .collect::<Vec<_>>(),
            [
                ("add a Windows path test", 0, 3),
                ("cap the queue at 50", 1, 3)
            ]
        );
        {
            let tx = conn.transaction().unwrap();
            rebuild_thread_todos(&tx, t, now).unwrap();
            tx.commit().unwrap();
        }
        assert_eq!(rows(&conn), edited, "a full re-derive");
        let db = crate::db::Db(std::sync::Mutex::new(conn));
        let report = regate_todos_once(&db, now, &|| false).unwrap().unwrap();
        assert_eq!((report.dropped, report.added), (0, 0), "{report:?}");
        assert_eq!(rows(&db.0.lock().unwrap()), edited, "the re-judge");
    }

    /// Once the user rewords a derived TODO of a thread, a follow-up or a heuristic one, its
    /// follow-ups stay as they are: a later list arriving by append adds nothing and removes
    /// nothing, and neither a full re-derive past the age limit nor the re-judge touches them.
    /// Its heuristic TODOs derive as before follow-ups existed: a marker in the appended
    /// message is added, and a reworded one's original never comes back.
    #[test]
    fn a_reworded_todo_freezes_its_threads_follow_ups() {
        let now = chrono::Utc::now().timestamp();
        let msgs = timed(
            &[
                "go",
                "TODO: tidy the first marker\n## Follow-ups\n- cap the retry queue\n\
                 - keep the log quiet",
                "and?",
                "TODO: tidy the second marker\n## Remaining\n- add a Windows path test",
            ],
            now - 3_600,
        );
        let row = |text: &str, extractor: &str| -> (String, String, String) {
            (text.into(), extractor.into(), "open".into())
        };
        let mut conn = temp_db();
        let mut frozen = Vec::new();
        for (ext, from, to) in [
            ("z1", "cap the retry queue", "cap the queue at 50"),
            ("z2", "tidy the first marker", "tidy the first marker twice"),
        ] {
            let t = seed_timed(&mut conn, ext, "/proj/z", false, &msgs[..2]);
            let id: i64 = conn
                .query_row(
                    "SELECT id FROM facts WHERE thread_id = ?1 AND text = ?2",
                    params![t, from],
                    |r| r.get(0),
                )
                .unwrap();
            edit_fact(&conn, id, to).unwrap();
            seed_timed(&mut conn, ext, "/proj/z", false, &msgs);
            let mut expected = vec![
                row("cap the retry queue", "followup"),
                row("keep the log quiet", "followup"),
                row("tidy the first marker", "heuristic"),
                row("tidy the second marker", "heuristic"),
            ];
            for r in &mut expected {
                if r.0 == from {
                    r.0 = to.into();
                }
            }
            expected.sort();
            assert_eq!(todo_rows(&conn, t), expected, "{ext}: appended");
            frozen.push((t, expected));
        }
        {
            let tx = conn.transaction().unwrap();
            for (t, _) in &frozen {
                rebuild_thread_todos(&tx, *t, now + FOLLOWUP_MAX_AGE_SECS + 86_400).unwrap();
            }
            tx.commit().unwrap();
        }
        for (t, expected) in &frozen {
            assert_eq!(&todo_rows(&conn, *t), expected, "a full re-derive");
        }
        let db = crate::db::Db(std::sync::Mutex::new(conn));
        let report = regate_todos_once(&db, now, &|| false).unwrap().unwrap();
        assert_eq!((report.dropped, report.added), (0, 0), "{report:?}");
        for (t, expected) in &frozen {
            assert_eq!(
                &todo_rows(&db.0.lock().unwrap(), *t),
                expected,
                "the re-judge"
            );
        }
    }

    /// A heuristic TODO that repeats an open follow-up (an assistant's TODO marker, the user's
    /// own checklist line) takes the follow-up's row over, uid and all, so a later list that
    /// leaves the follow-up out does not take the TODO with it: heuristic TODOs accumulate, on
    /// appends, on a one-pass derive and on a re-derive.
    #[test]
    fn a_heuristic_todo_outlives_the_follow_up_it_repeats() {
        let now = chrono::Utc::now().timestamp();
        let msgs = timed(
            &[
                "go",
                "## Follow-ups\n- cap the retry queue\n- keep the log quiet",
                "- [ ] keep the log quiet",
                "TODO: cap the retry queue",
                "next?",
                "## Remaining\n- add a Windows path test",
            ],
            now - 3_600,
        );
        let row = |text: &str, extractor: &str| -> (String, String, String) {
            (text.into(), extractor.into(), "open".into())
        };
        let expected = vec![
            row("add a Windows path test", "followup"),
            row("cap the retry queue", "heuristic"),
            row("keep the log quiet", "heuristic"),
        ];
        // (text, uid, source seq) of the two repeated items.
        let repeated = |conn: &Connection, t: i64| -> Vec<(String, String, i64)> {
            conn.prepare(
                "SELECT text, uid, source_seq FROM facts WHERE thread_id = ?1
                   AND text IN ('cap the retry queue', 'keep the log quiet') ORDER BY text",
            )
            .unwrap()
            .query_map([t], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
        };
        let mut conn = temp_db();
        let tail = seed_timed(&mut conn, "h1", "/proj/h", false, &msgs[..2]);
        let uids: Vec<String> = repeated(&conn, tail).into_iter().map(|r| r.1).collect();
        seed_timed(&mut conn, "h1", "/proj/h", false, &msgs[..4]);
        assert_eq!(
            todo_rows(&conn, tail),
            [
                row("cap the retry queue", "heuristic"),
                row("keep the log quiet", "heuristic")
            ],
            "taken over"
        );
        let taken = vec![
            ("cap the retry queue".to_string(), uids[0].clone(), 3),
            ("keep the log quiet".to_string(), uids[1].clone(), 2),
        ];
        assert_eq!(
            repeated(&conn, tail),
            taken,
            "the uid, at the heuristic's message"
        );
        seed_timed(&mut conn, "h1", "/proj/h", false, &msgs);
        assert_eq!(todo_rows(&conn, tail), expected, "appended");
        let full = seed_timed(&mut conn, "h2", "/proj/h", false, &msgs);
        assert_eq!(todo_rows(&conn, full), expected, "one pass");
        {
            let tx = conn.transaction().unwrap();
            rebuild_thread_todos(&tx, tail, now).unwrap();
            rebuild_thread_todos(&tx, full, now).unwrap();
            tx.commit().unwrap();
        }
        for t in [tail, full] {
            assert_eq!(todo_rows(&conn, t), expected, "a full re-derive");
        }
        assert_eq!(repeated(&conn, tail), taken);
    }

    /// An item that leaves the latest list and comes back in a later one is a new fact
    /// however the messages were batched (three appends or one), and a full re-derive keeps
    /// the uid it has now. One that two lists in a row hold keeps its uid
    /// (`only_the_latest_follow_up_list_counts`).
    #[test]
    fn a_follow_up_that_leaves_the_list_comes_back_as_a_new_fact() {
        let now = chrono::Utc::now().timestamp();
        let msgs = timed(
            &[
                "go",
                "## Follow-ups\n- cap the retry queue",
                "and?",
                "## Remaining\n- document the budget",
                "next?",
                "**Deferred:**\n- cap the retry queue",
            ],
            now - 3_600,
        );
        let uid = |conn: &Connection, t: i64| -> String {
            conn.query_row(
                "SELECT uid FROM facts WHERE thread_id = ?1 AND text = 'cap the retry queue'",
                [t],
                |r| r.get(0),
            )
            .unwrap()
        };
        let mut conn = temp_db();
        let three = seed_timed(&mut conn, "u1", "/proj/u", false, &msgs[..2]);
        let one = seed_timed(&mut conn, "u2", "/proj/u", false, &msgs[..2]);
        let first = [uid(&conn, three), uid(&conn, one)];
        seed_timed(&mut conn, "u1", "/proj/u", false, &msgs[..4]);
        assert_eq!(followup_texts(&conn, three), ["document the budget"]);
        seed_timed(&mut conn, "u1", "/proj/u", false, &msgs);
        seed_timed(&mut conn, "u2", "/proj/u", false, &msgs);
        let back = [uid(&conn, three), uid(&conn, one)];
        for (i, t) in [three, one].into_iter().enumerate() {
            assert_eq!(followup_texts(&conn, t), ["cap the retry queue"]);
            assert_ne!(back[i], first[i], "a new fact");
        }
        {
            let tx = conn.transaction().unwrap();
            rebuild_thread_todos(&tx, three, now).unwrap();
            rebuild_thread_todos(&tx, one, now).unwrap();
            tx.commit().unwrap();
        }
        assert_eq!([uid(&conn, three), uid(&conn, one)], back);
    }

    /// A deferred-work list more than 30 days old at derive time yields nothing and removes
    /// nothing, judged by its own message's time (seconds or milliseconds; none counts as
    /// new): a newer list's rows stay even when the old list comes later in the thread.
    #[test]
    fn an_old_follow_up_list_yields_nothing_and_removes_nothing() {
        let now = chrono::Utc::now().timestamp();
        let old = now - FOLLOWUP_MAX_AGE_SECS - 86_400;
        let recent = "TODO: tidy the recent marker\n## Follow-ups\n- cap the retry queue";
        let stale = "TODO: tidy the old marker\n## Remaining\n- port the hook to Cursor";
        let mut conn = temp_db();
        for (ext, ts) in [("a1", old), ("a2", old * 1_000)] {
            let t = seed_timed(
                &mut conn,
                ext,
                "/proj/a",
                false,
                &[("go", Some(ts)), (stale, Some(ts))],
            );
            assert!(followup_texts(&conn, t).is_empty(), "{ext}");
            assert_eq!(
                heuristic_todos(&conn, t).len(),
                1,
                "{ext}: its marker stays"
            );
        }
        let t = seed_timed(
            &mut conn,
            "a3",
            "/proj/a",
            false,
            &[("go", None), (stale, None)],
        );
        assert_eq!(followup_texts(&conn, t), ["port the hook to Cursor"]);

        // The old list after a recent one (the clocks disagree), on a full derive and on an
        // appended tail.
        let msgs = [
            ("go", Some(now - 60)),
            (recent, Some(now - 59)),
            ("and?", Some(old)),
            (stale, Some(old)),
        ];
        let full = seed_timed(&mut conn, "a4", "/proj/a", false, &msgs);
        let tail = seed_timed(&mut conn, "a5", "/proj/a", false, &msgs[..2]);
        seed_timed(&mut conn, "a5", "/proj/a", false, &msgs);
        for t in [full, tail] {
            assert_eq!(followup_texts(&conn, t), ["cap the retry queue"]);
            assert_eq!(heuristic_todos(&conn, t).len(), 2, "both markers");
        }
    }

    /// An open follow-up that has aged past the limit leaves on a full re-derive unless the
    /// user pinned it; the heuristic TODO of the same message stays.
    #[test]
    fn an_aged_follow_up_leaves_on_a_full_re_derive_unless_pinned() {
        let now = chrono::Utc::now().timestamp();
        let text =
            "TODO: tidy the marker\n## Follow-ups\n- cap the retry queue\n- pin the kept one";
        let mut conn = temp_db();
        let t = seed_timed(
            &mut conn,
            "g1",
            "/proj/g",
            false,
            &timed(&["go", text], now - 60),
        );
        assert_eq!(
            followup_texts(&conn, t),
            ["cap the retry queue", "pin the kept one"]
        );
        conn.execute(
            "UPDATE facts SET pinned = 1 WHERE text = 'pin the kept one'",
            [],
        )
        .unwrap();
        {
            let tx = conn.transaction().unwrap();
            rebuild_thread_todos(&tx, t, now + FOLLOWUP_MAX_AGE_SECS + 86_400).unwrap();
            tx.commit().unwrap();
        }
        let row = |text: &str, extractor: &str| -> (String, String, String) {
            (text.into(), extractor.into(), "open".into())
        };
        assert_eq!(
            todo_rows(&conn, t),
            [
                row("pin the kept one", "followup"),
                row("tidy the marker", "heuristic")
            ]
        );
    }

    /// The re-judge applies both rules: over threads indexed before follow-ups it adds only a
    /// thread's latest list from the last 30 days, removes an earlier list's open follow-ups
    /// already in the table (one the latest list repeats keeps its uid), leaves heuristic
    /// TODOs as they were, resumes after the last thread it finished and runs once.
    #[test]
    fn the_re_judge_applies_the_follow_up_rules() {
        let now = chrono::Utc::now().timestamp();
        let old = now - FOLLOWUP_MAX_AGE_SECS - 86_400;
        let l1 = "TODO: tidy the first marker\n## Follow-ups\n- port the hook to Cursor\n\
                  - cap the retry queue";
        let l2 = "TODO: tidy the second marker\n## Remaining\n- cap the retry queue\n\
                  - add a Windows path test";
        let latest = ["add a Windows path test", "cap the retry queue"];
        let mut conn = temp_db();
        let p = "/proj/j";
        let walked = seed_timed(
            &mut conn,
            "j0",
            p,
            false,
            &timed(&["go", "## Follow-ups\n- left as it was"], now - 60),
        );
        let two = seed_timed(
            &mut conn,
            "j1",
            p,
            false,
            &timed(&["go", l1, "and?", l2], now - 60),
        );
        let stale = seed_timed(&mut conn, "j2", p, false, &timed(&["go", l1], old));
        let one = seed_timed(&mut conn, "j3", p, false, &timed(&["go", l2], now - 60));
        // As an index from before follow-ups: none, except the first list's, as if the
        // indexer had appended them to `two` before its second list arrived.
        conn.execute("DELETE FROM facts WHERE extractor = 'followup'", [])
            .unwrap();
        let l1_message: i64 = conn
            .query_row(
                "SELECT id FROM messages WHERE thread_id = ?1 AND seq = 1",
                [two],
                |r| r.get(0),
            )
            .unwrap();
        let shared = new_fact_uid();
        for (text, uid) in [
            ("port the hook to Cursor", new_fact_uid()),
            ("cap the retry queue", shared.clone()),
        ] {
            conn.execute(
                "INSERT INTO facts (thread_id, kind, text, status, extractor, source_message_id,
                                    source_seq, created_at, uid)
                 VALUES (?1, 'todo', ?2, 'open', 'followup', ?3, 1, ?4, ?5)",
                params![two, text, l1_message, now - 59, uid],
            )
            .unwrap();
        }
        let markers = |conn: &Connection| -> Vec<(i64, String, String)> {
            conn.prepare(
                "SELECT thread_id, text, status FROM facts WHERE extractor = 'heuristic'
                 ORDER BY thread_id, text",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
        };
        let before = markers(&conn);
        assert_eq!(before.len(), 4, "{before:?}");
        // Interrupted after the first thread.
        app_config::put(&conn, TODOS_REGATED, &walked.to_string()).unwrap();
        let db = crate::db::Db(std::sync::Mutex::new(conn));
        let report = regate_todos_once(&db, now, &|| false).unwrap().unwrap();
        assert_eq!(report.threads, 3, "{report:?}");
        // `two` holds two follow-ups before and after; `one` gains its two.
        assert_eq!((report.dropped, report.added), (0, 2), "{report:?}");
        {
            let conn = db.0.lock().unwrap();
            assert!(followup_texts(&conn, walked).is_empty(), "not walked again");
            assert_eq!(followup_texts(&conn, two), latest);
            let uid: String = conn
                .query_row(
                    "SELECT uid FROM facts WHERE thread_id = ?1 AND text = 'cap the retry queue'",
                    [two],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(uid, shared);
            assert!(followup_texts(&conn, stale).is_empty(), "an old list");
            assert_eq!(followup_texts(&conn, one), latest);
            assert_eq!(markers(&conn), before);
        }
        assert_eq!(regate_todos_once(&db, now, &|| false).unwrap(), None);
    }

    /// The re-judge's pre-gate state: TODO rows as the old rule stored them, one per curated
    /// state, each named by its text. Every curated row holds text its source message yields
    /// (in another case), so only the curation predicate keeps it from the re-judge.
    fn regate_fixture() -> (crate::db::Db, i64, i64) {
        let mut conn = temp_db();
        // Indexed under today's rules: the assistant's how-to checklist yields nothing, and no
        // other message repeats its line (one that does keeps the row:
        // `the_re_judge_moves_a_rejected_todo_another_message_yields`). The edited row below
        // makes the thread hand-curated, so the re-judge adds none of its follow-ups.
        let howto = "- [ ] Post the clip on Tuesday\n- [ ] wire `retry_backoff` in\n\
                     TODO: post the clip later";
        let t = seed_thread(
            &mut conn,
            "r1",
            "/proj/r",
            false,
            &[
                "- [ ] Post the clip on Monday",
                howto,
                "and the rest?",
                "## Follow-ups\n- cap the retry queue",
            ],
        );
        let other = seed_thread(
            &mut conn,
            "r2",
            "/proj/r",
            false,
            &["hi", "- [ ] Upload it"],
        );
        let (user_msg, asst_msg): (i64, i64) = conn
            .query_row(
                "SELECT (SELECT id FROM messages WHERE thread_id = ?1 AND seq = 0),
                        (SELECT id FROM messages WHERE thread_id = ?1 AND seq = 1)",
                [t],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        conn.execute("DELETE FROM facts", []).unwrap();
        // (text, extractor, status, pinned, hidden, edited, source message, source seq).
        type Row<'a> = (
            &'a str,
            &'a str,
            &'a str,
            i64,
            i64,
            i64,
            Option<i64>,
            Option<i64>,
        );
        let rows: [Row; 13] = [
            // The one row the gate rejects.
            (
                "Post the clip on Tuesday",
                "heuristic",
                "open",
                0,
                0,
                0,
                Some(asst_msg),
                Some(1),
            ),
            // The same line, curated or written by someone else: all kept.
            (
                "post the clip on tuesday",
                "heuristic",
                "done",
                0,
                0,
                0,
                Some(asst_msg),
                Some(1),
            ),
            (
                "Post The Clip On Tuesday",
                "heuristic",
                "open",
                1,
                0,
                0,
                Some(asst_msg),
                Some(1),
            ),
            (
                "POST the clip on Tuesday",
                "heuristic",
                "open",
                0,
                1,
                0,
                Some(asst_msg),
                Some(1),
            ),
            // Reworded by only its case, as a user fixing a capital would.
            (
                "Post the clip on TUESDAY",
                "heuristic",
                "open",
                0,
                0,
                1,
                Some(asst_msg),
                Some(1),
            ),
            (
                "Post the clip on Tuesday",
                "user",
                "open",
                0,
                0,
                0,
                None,
                None,
            ),
            (
                "Post the clip on Tuesday",
                "agent",
                "open",
                0,
                0,
                0,
                None,
                None,
            ),
            (
                "Post the clip on Tuesday",
                "llm",
                "open",
                0,
                0,
                0,
                Some(asst_msg),
                Some(1),
            ),
            // A follow-up: in this hand-curated thread no derive touches one.
            (
                "Post the clip on Tuesday",
                "followup",
                "open",
                0,
                0,
                0,
                None,
                Some(3),
            ),
            // The user's own checklist, a source that is gone, a specific, a marker.
            (
                "Post the clip on Monday",
                "heuristic",
                "open",
                0,
                0,
                0,
                Some(user_msg),
                Some(0),
            ),
            (
                "Post the clip on Tuesday",
                "heuristic",
                "open",
                0,
                0,
                0,
                None,
                None,
            ),
            (
                "wire `retry_backoff` in",
                "heuristic",
                "open",
                0,
                0,
                0,
                Some(asst_msg),
                Some(1),
            ),
            (
                "post the clip later",
                "heuristic",
                "open",
                0,
                0,
                0,
                Some(asst_msg),
                Some(1),
            ),
        ];
        for (text, extractor, status, pinned, hidden, edited, mid, seq) in rows {
            conn.execute(
                "INSERT INTO facts (thread_id, kind, text, status, extractor, pinned, hidden,
                                    edited, source_message_id, source_seq, created_at, uid)
                 VALUES (?1, 'todo', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 5, ?10)",
                params![
                    t,
                    text,
                    status,
                    extractor,
                    pinned,
                    hidden,
                    edited,
                    mid,
                    seq,
                    new_fact_uid()
                ],
            )
            .unwrap();
        }
        // Found only by its seq: a re-index nulled the message id.
        conn.execute(
            "INSERT INTO facts (thread_id, kind, text, status, extractor, source_seq, created_at)
             VALUES (?1, 'todo', 'Upload it', 'open', 'heuristic', 1, 5)",
            [other],
        )
        .unwrap();
        (crate::db::Db(std::sync::Mutex::new(conn)), t, other)
    }

    /// The re-judge deletes only open, uncurated heuristic TODOs that an assistant's checklist
    /// raised and the gate now rejects: never a done, pinned, hidden or edited one, one a
    /// user or agent wrote, one from another extractor, the user's own checklist, one whose
    /// source is gone, or a marker. It adds no follow-up to the hand-curated thread (it added
    /// one before an edited row froze a thread's follow-ups) and runs once.
    #[test]
    fn the_re_judge_drops_only_rejected_checklist_todos() {
        let (db, t, other) = regate_fixture();
        let before = |conn: &Connection| -> i64 {
            conn.query_row("SELECT COUNT(*) FROM facts WHERE kind = 'todo'", [], |r| {
                r.get(0)
            })
            .unwrap()
        };
        let n = before(&db.0.lock().unwrap());
        let polls = std::cell::Cell::new(0);
        let pause = || {
            polls.set(polls.get() + 1);
            false
        };
        let report = regate_todos_once(&db, 9_999, &pause).unwrap().unwrap();
        assert!(polls.get() >= 1, "asks before its batch");
        assert_eq!(report.dropped, 2, "{report:?}");
        assert_eq!(report.added, 0, "{report:?}");
        let conn = db.0.lock().unwrap();
        assert_eq!(before(&conn), n - 2);
        let rows = todo_rows(&conn, t);
        let has = |text: &str, extractor: &str, status: &str| {
            rows.iter()
                .filter(|r| r.0 == text && r.1 == extractor && r.2 == status)
                .count()
        };
        // Gone: the open, uncurated, assistant-raised one (the other thread's too, found
        // by its seq).
        let curated: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM facts WHERE thread_id = ?1 AND extractor = 'heuristic'
                   AND text = 'Post the clip on Tuesday' AND source_message_id IS NOT NULL
                   AND source_seq = 1 AND status = 'open' AND pinned = 0 AND hidden = 0
                   AND edited = 0",
                [t],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(curated, 0);
        assert!(todo_rows(&conn, other).is_empty());
        // Kept: every curated state, every other extractor, the user's checklist, the row
        // with no source, the specific and the marker.
        assert_eq!(has("post the clip on tuesday", "heuristic", "done"), 1);
        assert_eq!(has("Post The Clip On Tuesday", "heuristic", "open"), 1);
        assert_eq!(has("POST the clip on Tuesday", "heuristic", "open"), 1);
        assert_eq!(has("Post the clip on TUESDAY", "heuristic", "open"), 1);
        for extractor in ["user", "agent", "llm", "followup"] {
            assert_eq!(
                has("Post the clip on Tuesday", extractor, "open"),
                1,
                "{extractor}"
            );
        }
        assert_eq!(has("Post the clip on Monday", "heuristic", "open"), 1);
        assert_eq!(
            has("Post the clip on Tuesday", "heuristic", "open"),
            1,
            "the one whose source is gone"
        );
        assert_eq!(has("wire `retry_backoff` in", "heuristic", "open"), 1);
        assert_eq!(has("post the clip later", "heuristic", "open"), 1);
        // Not added: the thread is hand-curated.
        assert_eq!(has("cap the retry queue", "followup", "open"), 0);
        assert_eq!(
            app_config::get(&conn, TODOS_REGATED).unwrap().as_deref(),
            Some(REGATE_DONE)
        );
        drop(conn);
        assert_eq!(regate_todos_once(&db, 9_999, &|| false).unwrap(), None);
    }

    /// A checklist row the gate rejects in its own message stays when another message of the
    /// thread still yields its text as a heuristic TODO, in any case (a later `TODO:` marker,
    /// the user's own `- [ ]` line): it moves to the first such message under its uid, where a
    /// full re-derive keeps it. One that no message yields still goes.
    #[test]
    fn the_re_judge_moves_a_rejected_todo_another_message_yields() {
        let mut conn = temp_db();
        let checklist = "- [ ] update the docs";
        let mut threads = Vec::new();
        for (ext, rest) in [
            ("m1", &["and?", "TODO: update the docs"][..]),
            ("m2", &["- [ ] Update the Docs"][..]),
            ("m3", &["thanks"][..]),
        ] {
            let texts: Vec<&str> = ["go", checklist].iter().chain(rest).copied().collect();
            let t = seed_thread(&mut conn, ext, "/proj/m", false, &texts);
            conn.execute("DELETE FROM facts WHERE thread_id = ?1", [t])
                .unwrap();
            // As the old rule stored it: from the assistant's checklist at seq 1.
            let uid = new_fact_uid();
            conn.execute(
                "INSERT INTO facts (thread_id, kind, text, status, extractor, source_message_id,
                                    source_seq, created_at, uid)
                 SELECT ?1, 'todo', 'update the docs', 'open', 'heuristic', id, 1, 5, ?2
                 FROM messages WHERE thread_id = ?1 AND seq = 1",
                params![t, uid],
            )
            .unwrap();
            threads.push((t, uid));
        }
        let db = crate::db::Db(std::sync::Mutex::new(conn));
        let report = regate_todos_once(&db, 9_999, &|| false).unwrap().unwrap();
        assert_eq!((report.dropped, report.added), (1, 0), "{report:?}");
        let conn = db.0.lock().unwrap();
        // (text, source seq, the seq of its source message, created_at, uid).
        let row = |t: i64| -> Option<(String, i64, i64, i64, String)> {
            conn.query_row(
                "SELECT f.text, f.source_seq, m.seq, f.created_at, f.uid
                 FROM facts f JOIN messages m ON m.id = f.source_message_id
                 WHERE f.thread_id = ?1",
                [t],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()
            .unwrap()
        };
        let moved = |seq: i64, uid: &str| {
            Some((
                "update the docs".to_string(),
                seq,
                seq,
                1_000 + seq,
                uid.to_string(),
            ))
        };
        assert_eq!(row(threads[0].0), moved(3, &threads[0].1), "a later marker");
        assert_eq!(
            row(threads[1].0),
            moved(2, &threads[1].1),
            "the user's line"
        );
        assert_eq!(row(threads[2].0), None, "no message yields it");
    }

    /// A `- [ ]` item under a "Follow-ups" heading that the old rule stored and the user then
    /// reworded never comes back as an open follow-up with its original text: the thread is
    /// hand-curated, so the re-judge adds no follow-up there (this pinned the later list's
    /// item being added before), and it leaves the rows a later full re-index leaves.
    #[test]
    fn the_re_judge_never_brings_back_a_reworded_todo_as_a_follow_up() {
        let mut conn = temp_db();
        let t = seed_thread(
            &mut conn,
            "e1",
            "/proj/e",
            false,
            &[
                "go",
                "## Follow-ups\n- [ ] wire `retry` into jobs.rs",
                "and?",
                "## Remaining\n- cap the retry queue",
            ],
        );
        conn.execute("DELETE FROM facts WHERE thread_id = ?1", [t])
            .unwrap();
        let asst: i64 = conn
            .query_row(
                "SELECT id FROM messages WHERE thread_id = ?1 AND seq = 1",
                [t],
                |r| r.get(0),
            )
            .unwrap();
        // As the old rule stored the checklist item, after the user reworded it.
        conn.execute(
            "INSERT INTO facts (thread_id, kind, text, status, extractor, edited,
                                source_message_id, source_seq, created_at, uid)
             VALUES (?1, 'todo', 'wire `retry` into the job runner', 'open', 'heuristic', 1,
                     ?2, 1, 5, ?3)",
            params![t, asst, new_fact_uid()],
        )
        .unwrap();
        let db = crate::db::Db(std::sync::Mutex::new(conn));
        let report = regate_todos_once(&db, 9_999, &|| false).unwrap().unwrap();
        assert_eq!((report.dropped, report.added), (0, 0), "{report:?}");
        let mut conn = db.0.lock().unwrap();
        let after = todo_rows(&conn, t);
        assert_eq!(
            after,
            vec![(
                "wire `retry` into the job runner".to_string(),
                "heuristic".to_string(),
                "open".to_string()
            )]
        );
        let tx = conn.transaction().unwrap();
        rebuild_thread_todos(&tx, t, 9_999).unwrap();
        tx.commit().unwrap();
        assert_eq!(
            todo_rows(&conn, t),
            after,
            "a full re-derive keeps the same rows"
        );
    }

    /// An interrupted pass resumes after the last thread it finished; one that already ran,
    /// or a fresh index, does nothing.
    #[test]
    fn the_re_judge_resumes_and_never_repeats() {
        let (db, t, other) = regate_fixture();
        {
            let conn = db.0.lock().unwrap();
            app_config::put(&conn, TODOS_REGATED, &t.to_string()).unwrap();
        }
        let report = regate_todos_once(&db, 9_999, &|| false).unwrap().unwrap();
        assert_eq!(report.threads, 1, "only the threads after the cursor");
        assert_eq!(report.dropped, 1);
        let conn = db.0.lock().unwrap();
        assert!(todo_rows(&conn, other).is_empty());
        assert!(
            todo_rows(&conn, t)
                .iter()
                .any(|r| r.0 == "Post the clip on Tuesday" && r.1 == "heuristic"),
            "the thread before the cursor is not judged again"
        );
        drop(conn);

        let mut fresh = temp_db();
        mark_todos_backfilled(&fresh).unwrap();
        seed_thread(
            &mut fresh,
            "n1",
            "/proj/n",
            false,
            &["hi", "- [ ] Upload it"],
        );
        let db = crate::db::Db(std::sync::Mutex::new(fresh));
        assert_eq!(regate_todos_once(&db, 9_999, &|| false).unwrap(), None);
    }

    /// On an index without the columns it reads (before 0034's uid and source seq), the
    /// re-judge does nothing and records nothing, so it runs once the app has upgraded it.
    #[test]
    fn the_re_judge_skips_an_index_without_its_columns() {
        let path = crate::db::test_support::TestPath::new("regate_old");
        crate::db::test_support::index_at_version(&path, 33, |conn| {
            conn.execute(
                "INSERT INTO threads (source_id, external_id, title) VALUES (1, 'x', 'x')",
                [],
            )
            .unwrap();
        });
        let conn = Connection::open(&*path).unwrap();
        let db = crate::db::Db(std::sync::Mutex::new(conn));
        assert_eq!(regate_todos_once(&db, 9_999, &|| false).unwrap(), None);
        let conn = db.0.lock().unwrap();
        assert_eq!(app_config::get(&conn, TODOS_REGATED).unwrap(), None);
    }

    /// Real-data gate of the checklist gate and follow-ups, on an APFS clone of an index that
    /// this test WRITES: it migrates the clone, plans the re-judge in a transaction it rolls
    /// back (timing each thread), then runs it for real and counts again. Prints counts,
    /// timings and samples (ids and the first 60 characters) to stderr for a person to judge;
    /// nothing is stored. Refuses a file in the app's data folder.
    ///
    /// CALLIMACHUS_GROUNDING_DB=<clone>/index.db cargo test -p callimachus-core --lib \
    ///   real_todo_regate -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_todo_regate() {
        use std::time::{Duration, Instant};
        let Ok(path) = std::env::var("CALLIMACHUS_GROUNDING_DB") else {
            eprintln!("set CALLIMACHUS_GROUNDING_DB to an index clone");
            return;
        };
        let path = std::path::PathBuf::from(path);
        let canonical =
            |p: &std::path::Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        assert!(
            canonical(&path).parent() != Some(canonical(&crate::db::app_data_dir()).as_path()),
            "{} is in the app's data folder; clone it elsewhere first (cp -c on APFS)",
            path.display()
        );
        let started = Instant::now();
        let conn = crate::db::open(&path).unwrap();
        eprintln!("open + migrate: {:.1?}", started.elapsed());
        let now = chrono::Utc::now().timestamp();
        let open_by = |conn: &Connection| -> Vec<(String, i64)> {
            conn.prepare(
                "SELECT extractor, COUNT(*) FROM facts
                 WHERE kind = 'todo' AND status = 'open' AND hidden = 0 GROUP BY 1 ORDER BY 1",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
        };
        let rows = |conn: &Connection, sql: &str| -> Vec<(i64, String)> {
            conn.prepare(sql)
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        let listed = |conn: &Connection| list_open_todos(conn, None, None, None, 1 << 30).unwrap();
        let before = open_by(&conn);
        let listed_before = listed(&conn).len();

        // The plan, rolled back: what each thread would lose and gain, and how long it takes.
        let ids: Vec<i64> = conn
            .prepare("SELECT id FROM threads WHERE is_subagent = 0 ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let (mut dropped, mut slowest, mut total) = (Vec::new(), Duration::ZERO, Duration::ZERO);
        {
            let tx = conn.unchecked_transaction().unwrap();
            for &id in &ids {
                // One that another message still yields moves there instead.
                for (d, _) in regate_drops(&tx, id)
                    .unwrap()
                    .into_iter()
                    .filter(|(_, to)| to.is_none())
                {
                    let text: String = tx
                        .query_row("SELECT text FROM facts WHERE id = ?1", [d], |r| r.get(0))
                        .unwrap();
                    dropped.push((d, text));
                }
                let at = Instant::now();
                regate_thread(&tx, id, now).unwrap();
                slowest = slowest.max(at.elapsed());
                total += at.elapsed();
            }
            // Dropped without a commit: rolled back.
        }

        let db = crate::db::Db(std::sync::Mutex::new(conn));
        let at = Instant::now();
        let report = regate_todos_once(&db, now, &|| false).unwrap();
        let wall = at.elapsed();
        let conn = db.0.lock().unwrap();
        let after = open_by(&conn);
        let kept = rows(
            &conn,
            "SELECT id, text FROM facts WHERE kind = 'todo' AND status = 'open' AND hidden = 0
               AND extractor = 'heuristic' ORDER BY id",
        );
        let followups = rows(
            &conn,
            "SELECT id, text FROM facts WHERE kind = 'todo' AND extractor = 'followup' ORDER BY id",
        );
        eprintln!("threads: {} top-level", ids.len());
        eprintln!("open TODOs before: {before:?} (listed {listed_before})");
        eprintln!(
            "open TODOs after:  {after:?} (listed {})",
            listed(&conn).len()
        );
        eprintln!("run: {report:?} in {wall:.1?}");
        eprintln!(
            "plan: {} dropped; per thread slowest {slowest:.1?}, all {total:.1?}",
            dropped.len()
        );
        let sample = |label: &str, items: &[(i64, String)]| {
            let step = (items.len() / 30).max(1);
            eprintln!("-- {label}: {} (every {step}th)", items.len());
            for (id, text) in items.iter().step_by(step).take(30) {
                let head: String = text.chars().take(60).collect();
                eprintln!("  #{id} {}", head.replace(['\n', '\r'], " "));
            }
        };
        sample("dropped heuristic", &dropped);
        sample("kept heuristic", &kept);
        sample("followup", &followups);
        // Which title each follow-up came from: the nearest title line above its bullet.
        let mut by_title: std::collections::BTreeMap<&str, Vec<(i64, String)>> = Default::default();
        for (id, text) in &followups {
            let message: Option<String> = conn
                .query_row(
                    "SELECT m.text FROM facts f JOIN messages m ON m.id = f.source_message_id
                     WHERE f.id = ?1",
                    [id],
                    |r| r.get(0),
                )
                .optional()
                .unwrap();
            let title = message.and_then(|m| {
                let lines: Vec<&str> = m.lines().collect();
                let at = lines.iter().position(|l| l.contains(text.as_str()))?;
                lines[..at]
                    .iter()
                    .rev()
                    .find_map(|l| followup_title(l.trim()))
            });
            by_title
                .entry(title.unwrap_or("?"))
                .or_default()
                .push((*id, text.clone()));
        }
        for (title, items) in &by_title {
            sample(&format!("followup under {title:?}"), items);
        }
    }

    /// The re-judge deletes rows from the user's index, so only the watcher's job thread may
    /// start it: never a hook, the MCP server or a `cal` read command.
    #[test]
    fn the_re_judge_runs_only_from_the_watchers_job() {
        // The workspace: core, the app crate and the standalone binaries.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut callers = Vec::new();
        let mut stack = vec![
            root.join("core/src"),
            root.join("src"),
            root.join("standalone"),
        ];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs")
                    && std::fs::read_to_string(&p)
                        .unwrap()
                        .contains("regate_todos_once(")
                {
                    let rel = p.strip_prefix(&root).unwrap_or(&p);
                    callers.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }
        callers.sort();
        assert_eq!(
            callers,
            ["core/src/jobs.rs", "core/src/knowledge/mod.rs"],
            "{callers:?}"
        );
        let jobs = std::fs::read_to_string(root.join("core/src/jobs.rs")).unwrap();
        let kick = jobs.find("pub fn todo_backfill_kick").unwrap();
        let call = jobs.find("regate_todos_once(").unwrap();
        let next_fn = jobs[kick + 1..].find("\npub fn ").unwrap() + kick + 1;
        assert!(
            kick < call && call < next_fn,
            "called from todo_backfill_kick only"
        );
    }

    fn engine(id: &str, kind: &'static str, detected: bool) -> MemoryEngine {
        MemoryEngine {
            id: id.into(),
            label: id.into(),
            kind,
            detected,
            detail: String::new(),
        }
    }

    #[test]
    fn recommended_engine_prefers_cli_then_ollama_then_first_key() {
        let all = |claude, codex, ollama| {
            vec![
                engine("claude-cli", "cli", claude),
                engine("codex-cli", "cli", codex),
                engine("ollama", "ollama", ollama),
                engine("openai", "key", true),
                engine("gemini", "key", true),
            ]
        };
        let rec = |e: Vec<MemoryEngine>| recommended_engine(&e);
        assert_eq!(rec(all(true, true, true)).as_deref(), Some("claude-cli"));
        assert_eq!(rec(all(false, true, true)).as_deref(), Some("codex-cli"));
        assert_eq!(rec(all(false, false, true)).as_deref(), Some("ollama"));
        assert_eq!(rec(all(false, false, false)).as_deref(), Some("openai"));
        // Order in the list does not outrank the preference.
        let mut reversed = all(true, false, true);
        reversed.reverse();
        assert_eq!(rec(reversed).as_deref(), Some("claude-cli"));
        let none = vec![
            engine("claude-cli", "cli", false),
            engine("ollama", "ollama", false),
        ];
        assert_eq!(rec(none), None);
    }

    /// The one-click consent stores the named engine (not the legacy None) and turns
    /// auto-distill on; a later Settings off keeps the engine for the next on.
    #[test]
    fn enable_memory_names_the_engine_and_turns_auto_distill_on() {
        let conn = temp_db();
        enable_memory(&conn, "claude-cli", None).unwrap();
        let cfg = get_config(&conn).unwrap();
        assert!(cfg.enabled && cfg.auto_distill);
        assert_eq!(cfg.provider.as_deref(), Some("claude-cli"));
        assert_eq!(cfg.model, None);
        enable_memory(&conn, "ollama", Some("qwen3")).unwrap();
        let cfg = get_config(&conn).unwrap();
        assert_eq!(cfg.provider.as_deref(), Some("ollama"));
        assert_eq!(cfg.model.as_deref(), Some("qwen3"));
    }

    fn seed_thread(
        conn: &mut Connection,
        ext: &str,
        project: &str,
        subagent: bool,
        texts: &[&str],
    ) -> i64 {
        let messages: Vec<(&str, Option<i64>)> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| (*t, Some(1_000 + i as i64)))
            .collect();
        seed_timed(conn, ext, project, subagent, &messages)
    }

    /// [`seed_thread`] with each message's time: (text, ts), user and assistant in turn. A
    /// thread indexed again with more messages after the same ones takes the appended-tail
    /// path.
    fn seed_timed(
        conn: &mut Connection,
        ext: &str,
        project: &str,
        subagent: bool,
        messages: &[(&str, Option<i64>)],
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
                messages: messages
                    .iter()
                    .enumerate()
                    .map(|(i, (t, ts))| ParsedMessage {
                        role: if i % 2 == 0 { "user" } else { "assistant" }.into(),
                        text: (*t).into(),
                        tool_name: None,
                        ts: *ts,
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
        conn.execute("UPDATE threads SET updated_at = ?1", [now - 3_600])
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

    /// A session still in progress is left alone until it has been idle for
    /// AUTO_DISTILL_IDLE_SECS, without spending the day's allowance on it; manual and
    /// per-project distills (pending_threads) still see it.
    #[test]
    fn auto_distill_waits_until_a_thread_goes_idle() {
        let mut conn = temp_db();
        let live = seed_thread(
            &mut conn,
            "live",
            "/proj/idle",
            false,
            &["a", "b", "c", "d"],
        );
        let now = 1_800_000_000;
        conn.execute("UPDATE threads SET updated_at = ?1", [now - 60])
            .unwrap();
        assert!(auto_distill_worklist(&conn, 25, now).unwrap().is_empty());
        assert_eq!(
            auto_distill_remaining_today(&conn, now).unwrap(),
            AUTO_DISTILL_DEFAULT_DAILY_CAP,
            "a skipped live thread costs no allowance"
        );
        assert_eq!(pending_threads(&conn, 10).unwrap(), vec![live]);

        // Exactly at the boundary it is still live; one second past it, it is due.
        let edge = now - AUTO_DISTILL_IDLE_SECS;
        conn.execute("UPDATE threads SET updated_at = ?1", [edge])
            .unwrap();
        assert!(auto_distill_worklist(&conn, 25, now).unwrap().is_empty());
        conn.execute("UPDATE threads SET updated_at = ?1", [edge - 1])
            .unwrap();
        assert_eq!(auto_distill_worklist(&conn, 25, now).unwrap(), vec![live]);
    }

    /// The deadline kick lands once the earliest skipped live thread is due, the Settings
    /// count matches what the worklist would take, and nothing is scheduled once no pending
    /// thread is waiting on the idle filter.
    #[test]
    fn auto_distill_next_idle_at_is_when_the_earliest_live_thread_is_due() {
        let mut conn = temp_db();
        let first = seed_thread(&mut conn, "first", "/proj/a", false, &["a", "b", "c", "d"]);
        let later = seed_thread(&mut conn, "later", "/proj/b", false, &["a", "b", "c", "d"]);
        let now = 1_800_000_000;
        let set = |conn: &Connection, id: i64, at: i64| {
            conn.execute(
                "UPDATE threads SET updated_at = ?1 WHERE id = ?2",
                params![at, id],
            )
            .unwrap();
        };
        set(&conn, first, now - 120);
        set(&conn, later, now - 10);

        let due = auto_distill_next_idle_at(&conn, now).unwrap().unwrap();
        assert_eq!(
            due,
            now - 120 + AUTO_DISTILL_IDLE_SECS + AUTO_DISTILL_IDLE_SLACK_SECS
        );
        assert_eq!(auto_distill_eligible_count(&conn, now).unwrap(), 0);
        assert_eq!(auto_distill_eligible_count(&conn, due).unwrap(), 1);
        assert_eq!(auto_distill_worklist(&conn, 25, due).unwrap(), vec![first]);

        // Re-armed from the remaining live thread, then nothing once it is idle too.
        let next = auto_distill_next_idle_at(&conn, due).unwrap().unwrap();
        assert_eq!(
            next,
            now - 10 + AUTO_DISTILL_IDLE_SECS + AUTO_DISTILL_IDLE_SLACK_SECS
        );
        assert_eq!(auto_distill_next_idle_at(&conn, next).unwrap(), None);

        // A live thread that is not pending (it errored) schedules nothing.
        set(&conn, first, now - 2 * AUTO_DISTILL_IDLE_SECS);
        set(&conn, later, now);
        conn.execute(
            "UPDATE threads SET knowledge_error = 'boom' WHERE id = ?1",
            [later],
        )
        .unwrap();
        assert_eq!(auto_distill_next_idle_at(&conn, now).unwrap(), None);
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
