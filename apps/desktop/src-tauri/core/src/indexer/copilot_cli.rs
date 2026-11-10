//! GitHub Copilot CLI indexer (the standalone terminal agent; Copilot Chat in VS Code-family
//! editors is `copilot.rs`). Each session is a directory under
//! `$COPILOT_HOME/session-state/<id>/` (`~/.copilot` by default) whose `events.jsonl` is the
//! session's persisted event log, the same `{type, data, id, timestamp, parentId}` events
//! the Copilot SDK streams (docs/features/streaming-events.md in github/copilot-sdk, checked
//! 2026-11-07). `workspace.yaml` beside it holds the cwd, branch and a user-given name.
//!
//! Only interactive conversations are indexed. Other programs drive the same agent through
//! the Copilot SDK and leave sessions here too: on a real machine all 63 sessions were
//! GitHub Desktop commit-message runs (`client_name: sdk` in workspace.yaml, the whole diff
//! as the one prompt, up to 14 MB). A session whose workspace says `client_name: sdk`, or
//! that has no user message with text, is skipped.
//!
//! What becomes the thread: `user.message`, `assistant.message` (plus its `toolRequests` as
//! tool calls), `tool.execution_complete` results (capped), compaction summaries, and one
//! usage row per model at each `session.shutdown`. Ephemeral events are never on disk, and
//! records marked `checkpoint` (a replay copy of earlier history) or carrying an `agentId`
//! (a sub-agent's own turns, summarized back through the parent's tool call) are skipped.
//! Those three rules come from the SDK's event types: none of the real sessions here carried
//! any of the three fields.
//!
//! A live `events.jsonl` only grows, so after the first full parse it keeps a byte cursor
//! (as Claude Code transcripts do, see claude.rs): a later pass parses just the appended
//! lines onto the stored thread instead of re-reading the whole journal on every write.

use super::common::{
    begin_file, count_file, count_line, count_malformed, count_skipped, fnv1a, parse_rfc3339,
    push_message, truncate_title,
};
use super::{
    append_messages, file_stamp, source_id, upsert_thread, IndexReport, MsgUsage, ParsedMessage,
    ParsedThread,
};
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub const KIND: &str = "copilot_cli";

static COPILOT_HOME_OVERRIDE: std::sync::RwLock<Option<PathBuf>> = std::sync::RwLock::new(None);

/// `app_config` key holding a user-chosen Copilot CLI home (a Settings value). A GUI app
/// launched from Finder/Dock never sees the shell's `COPILOT_HOME`, so without it the app
/// and a shell `cal watch` could index different folders.
pub const COPILOT_HOME_SETTING: &str = "paths.copilot_home";

/// Point the indexer at a relocated Copilot CLI home. `None` (or an empty path) restores
/// the default lookup.
pub fn set_copilot_home_override(dir: Option<PathBuf>) {
    if let Ok(mut slot) = COPILOT_HOME_OVERRIDE.write() {
        *slot = dir
            .filter(|d| !d.as_os_str().is_empty())
            .map(super::claude::expand_tilde);
    }
}

/// Apply the [`COPILOT_HOME_SETTING`] override from the index (cleared when unset). Called
/// at the start of every scan, so a changed setting takes effect on the next pass.
pub fn load_copilot_home_setting(conn: &Connection) {
    set_copilot_home_override(super::claude::read_path_setting(conn, COPILOT_HOME_SETTING));
}

/// Copilot CLI's home: the Settings override, else `$COPILOT_HOME`, else `~/.copilot` (the
/// CLI config dir reference on docs.github.com: config, session history and logs all live
/// there).
pub fn copilot_home() -> Option<PathBuf> {
    if let Some(dir) = COPILOT_HOME_OVERRIDE.read().ok().and_then(|g| g.clone()) {
        return Some(dir);
    }
    if let Some(dir) = std::env::var_os("COPILOT_HOME").filter(|v| !v.is_empty()) {
        return Some(super::claude::expand_tilde(PathBuf::from(dir)));
    }
    dirs::home_dir().map(|h| h.join(".copilot"))
}

/// Where the session event logs live.
pub fn session_root() -> Option<PathBuf> {
    copilot_home().map(|h| h.join("session-state"))
}

pub fn scan(conn: &mut Connection, tick: &mut dyn FnMut()) -> Result<IndexReport> {
    load_copilot_home_setting(conn);
    let Some(root) = session_root() else {
        return Ok(IndexReport::default());
    };
    scan_in(conn, &root, tick)
}

/// One session on disk: its id, the journal files in replay order (numbered archives
/// first, the live `events.jsonl` last) and the directory, if it has one.
struct Session {
    id: String,
    journals: Vec<PathBuf>,
    dir: Option<PathBuf>,
}

impl Session {
    /// The `index_state` key: the live journal.
    fn key(&self) -> String {
        self.journals
            .last()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

/// Every session under `root`: `<id>/events*.jsonl` (the current layout) and flat
/// `<id>.jsonl` files (reported for early CLI builds; never seen on disk here).
fn sessions(root: &Path) -> Vec<Session> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            let mut journals: Vec<(u64, PathBuf)> = std::fs::read_dir(&path)
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    if !(n.starts_with("events") && n.ends_with(".jsonl")) {
                        return None;
                    }
                    // `events.jsonl` sorts last; an archive (`events.<n>.jsonl` or similar)
                    // by its number.
                    let digits: String = n.chars().filter(char::is_ascii_digit).collect();
                    let order = if n == "events.jsonl" {
                        u64::MAX
                    } else {
                        digits.parse().unwrap_or(0)
                    };
                    Some((order, e.path()))
                })
                .collect();
            if journals.is_empty() {
                continue;
            }
            journals.sort();
            out.push(Session {
                id: name,
                journals: journals.into_iter().map(|(_, p)| p).collect(),
                dir: Some(path),
            });
        } else if let Some(id) = name.strip_suffix(".jsonl") {
            out.push(Session {
                id: id.to_string(),
                journals: vec![path.clone()],
                dir: None,
            });
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

fn scan_in(conn: &mut Connection, root: &Path, tick: &mut dyn FnMut()) -> Result<IndexReport> {
    let mut report = IndexReport::default();
    if !root.is_dir() {
        return Ok(report);
    }
    let sid = source_id(conn, KIND)?;
    for session in sessions(root) {
        tick();
        let key = session.key();
        report.record_at(Path::new(&key), index_session(conn, sid, &session));
    }
    Ok(report)
}

/// The change key over every journal of a session: newest mtime, total size.
fn journals_stamp(journals: &[PathBuf]) -> Result<(i64, i64)> {
    let mut stamp = (0, 0);
    for j in journals {
        let (m, s) = file_stamp(j)?;
        stamp = (stamp.0.max(m), stamp.1 + s);
    }
    Ok(stamp)
}

fn index_session(conn: &mut Connection, sid: i64, session: &Session) -> Result<Option<usize>> {
    let key = session.key();
    let (mtime, size) = journals_stamp(&session.journals)?;
    let cursor = read_cursor(conn, &key)?;
    if cursor
        .as_ref()
        .is_some_and(|c| c.mtime == mtime && c.size == size)
    {
        return Ok(None);
    }
    if crate::cleanup::is_tombstoned(conn, sid, &session.id)? {
        write_cursor(conn, &key, mtime, size, 0, None)?;
        return Ok(None);
    }
    let workspace = session
        .dir
        .as_deref()
        .map(|d| read_workspace(&d.join("workspace.yaml")))
        .unwrap_or_default();
    // An SDK client's run (GitHub Desktop's commit messages), decided before any journal is
    // read: theirs held a whole diff each.
    if workspace.client_name.as_deref() == Some("sdk") {
        write_cursor(conn, &key, mtime, size, 0, None)?;
        return Ok(None);
    }

    // The append fast path, for a session with only its live journal.
    let live = session
        .journals
        .last()
        .filter(|_| session.journals.len() == 1);
    if let (Some(live), Some(c)) = (live, cursor.as_ref()) {
        if c.offset > 0 && size >= c.offset {
            match try_append(conn, sid, session, live, &workspace, c, mtime, size)? {
                Append::Applied(n) => return Ok(Some(n)),
                Append::NoChange => return Ok(None),
                Append::Fallback => {}
            }
        }
    }

    let file = begin_file();
    let mut parsed = Parsed::default();
    let mut offset = 0;
    for j in &session.journals {
        let bytes = std::fs::read(j)?;
        // Only whole lines: a line still being written is read once it ends, and the
        // cursor never lands inside it.
        let end = complete_lines(&bytes);
        parsed.read(&String::from_utf8_lossy(&bytes[..end]));
        offset = end;
    }
    count_file(file, parsed.messages.len());
    // Archives are replayed whole each time, so only a lone live journal keeps a cursor.
    let offset = if live.is_some() { offset } else { 0 };
    let tag = |n: usize, totals: &HashMap<String, Counters>| -> Result<Option<String>> {
        Ok(match live.filter(|_| offset > 0) {
            Some(live) => Some(cursor_tag(
                &fingerprint_file(live, offset as u64)?,
                n,
                totals,
            )),
            None => None,
        })
    };
    let project = parsed.cwd.clone().or(workspace.cwd);
    let user_texts = parsed
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .take(3)
        .map(|m| m.text.as_str());
    // No user message with text: not a conversation (yet). push_message drops empty text,
    // so any user row has some.
    let has_prompt = parsed.messages.iter().any(|m| m.role == "user");
    if !has_prompt || crate::agent::is_app_cli_session(project.as_deref(), user_texts) {
        // No cursor: a session that starts its conversation later is parsed whole then.
        write_cursor(conn, &key, mtime, size, 0, None)?;
        return Ok(None);
    }
    let title = workspace.name.or_else(|| {
        parsed
            .messages
            .iter()
            .find(|m| m.role == "user")
            .map(|m| truncate_title(m.text.lines().next().unwrap_or_default()))
    });
    let thread = ParsedThread {
        external_id: session.id.clone(),
        title,
        project_path: project,
        git_branch: parsed.branch.or(workspace.branch).filter(|b| !b.is_empty()),
        created_at: parsed.started.or(parsed.first_ts),
        updated_at: parsed.last_ts.or(Some(mtime)),
        is_subagent: false,
        messages: parsed.messages,
        usage: parsed.usage,
    };
    let n = upsert_thread(conn, sid, &thread)?;
    let tag = tag(thread.messages.len(), &parsed.totals)?;
    write_cursor(conn, &key, mtime, size, offset as i64, tag.as_deref())?;
    Ok(Some(n))
}

/// Event types skipped on purpose (the parse canary, `parse_health`): turn markers, errors,
/// usage checkpoints, permissions, skills and the tool start events whose completion is read.
/// Seen on the author's machine: `system.message`, `assistant.turn_start`,
/// `assistant.turn_end`, `session.error` (63 sessions, 441 lines).
const SKIPPED_EVENTS: &[&str] = &[
    "system.message",
    "assistant.turn_start",
    "assistant.turn_end",
    "assistant.reasoning",
    "assistant.intent",
    "session.error",
    "session.info",
    "session.warning",
    "session.usage_checkpoint",
    "session.model_change",
    "session.truncation",
    "session.compaction_start",
    "tool.execution_start",
    "tool.execution_partial_result",
    "tool.user_requested",
    "permission.requested",
    "permission.completed",
    "skill.invoked",
    "hook.start",
    "hook.end",
];

/// Bytes up to and including the last newline: the journal's complete lines.
fn complete_lines(bytes: &[u8]) -> usize {
    bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1)
}

/// A session journal's byte cursor in `index_state`.
struct Cursor {
    mtime: i64,
    size: i64,
    offset: i64,
    /// [`cursor_tag`]: the bytes' fingerprint, the stored message count and each model's
    /// usage totals at `offset`.
    tag: Option<String>,
}

fn read_cursor(conn: &Connection, key: &str) -> Result<Option<Cursor>> {
    Ok(conn
        .query_row(
            "SELECT mtime, size, last_offset, hash FROM index_state WHERE path = ?1",
            [key],
            |r| {
                Ok(Cursor {
                    mtime: r.get(0)?,
                    size: r.get(1)?,
                    offset: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    tag: r.get(3)?,
                })
            },
        )
        .optional()?)
}

fn write_cursor(
    conn: &Connection,
    key: &str,
    mtime: i64,
    size: i64,
    offset: i64,
    tag: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO index_state (path, source_kind, mtime, size, last_offset, hash, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (path) DO UPDATE SET
            mtime = excluded.mtime, size = excluded.size, last_offset = excluded.last_offset,
            hash = excluded.hash, updated_at = excluded.updated_at",
        params![
            key,
            KIND,
            mtime,
            size,
            offset,
            tag,
            chrono::Utc::now().timestamp()
        ],
    )?;
    Ok(())
}

/// Bytes hashed at each end of the journal before the cursor (as claude.rs does).
const FP_WINDOW: u64 = 4096;

/// The first and last [`FP_WINDOW`] bytes before `offset`: if either changed, the journal
/// was rewritten rather than appended to.
fn fingerprint_file(path: &Path, offset: u64) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let window = offset.min(FP_WINDOW) as usize;
    let mut head = vec![0u8; window];
    file.read_exact(&mut head)?;
    let mut tail = vec![0u8; window];
    file.seek(SeekFrom::Start(offset - window as u64))?;
    file.read_exact(&mut tail)?;
    Ok(format!("{:016x}{:016x}", fnv1a(&head), fnv1a(&tail)))
}

/// The cursor's `hash` value as JSON: `{fp, n, totals: {model: [input, output, cache_write,
/// cache_read, requests]}}`. The totals are what the next shutdown's delta is taken against.
fn cursor_tag(fingerprint: &str, messages: usize, totals: &HashMap<String, Counters>) -> String {
    let totals: BTreeMap<&str, [i64; 5]> = totals
        .iter()
        .map(|(m, c)| {
            let row = [c.input, c.output, c.cache_write, c.cache_read, c.requests];
            (m.as_str(), row)
        })
        .collect();
    serde_json::json!({"fp": fingerprint, "n": messages, "totals": totals}).to_string()
}

fn split_tag(tag: &str) -> Option<(String, usize, HashMap<String, Counters>)> {
    let v: Value = serde_json::from_str(tag).ok()?;
    let fp = v.get("fp")?.as_str()?.to_string();
    let n = usize::try_from(v.get("n")?.as_u64()?).ok()?;
    let mut totals = HashMap::new();
    for (model, c) in v.get("totals")?.as_object()? {
        let c: [i64; 5] = serde_json::from_value(c.clone()).ok()?;
        totals.insert(
            model.clone(),
            Counters {
                input: c[0],
                output: c[1],
                cache_write: c[2],
                cache_read: c[3],
                requests: c[4],
            },
        );
    }
    Some((fp, n, totals))
}

enum Append {
    /// The new lines were indexed; carries the number of message rows inserted.
    Applied(usize),
    /// Only non-message lines were appended; the cursor advanced, the thread is unchanged.
    NoChange,
    /// Not a clean append (or no stored thread to append to): do a full parse.
    Fallback,
}

/// Parse only the lines appended since the cursor onto the stored thread. The cursor's
/// fingerprint proves the journal only grew, and its message count that the stored thread
/// is the one it was taken against (a pass that wrote the rows but died before moving the
/// cursor would otherwise append them twice).
#[allow(clippy::too_many_arguments)] // all distinct; this is the one call site
fn try_append(
    conn: &mut Connection,
    sid: i64,
    session: &Session,
    live: &Path,
    workspace: &Workspace,
    cursor: &Cursor,
    mtime: i64,
    size: i64,
) -> Result<Append> {
    let Some((fp, stored_n, totals)) = cursor.tag.as_deref().and_then(split_tag) else {
        return Ok(Append::Fallback);
    };
    let offset = cursor.offset as u64;
    if fingerprint_file(live, offset)? != fp {
        return Ok(Append::Fallback);
    }
    let Some(stored) = stored_thread(conn, sid, &session.id)? else {
        return Ok(Append::Fallback);
    };
    if stored.message_count != stored_n {
        return Ok(Append::Fallback);
    }
    let mut file = std::fs::File::open(live)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut tail = Vec::with_capacity((size as u64).saturating_sub(offset) as usize);
    file.read_to_end(&mut tail)?;
    let end = complete_lines(&tail);
    let mut parsed = Parsed {
        totals,
        ..Default::default()
    };
    parsed.read(&String::from_utf8_lossy(&tail[..end]));
    let new_offset = offset + end as u64;

    let thread = ParsedThread {
        external_id: session.id.clone(),
        // A name the user gave since; otherwise the title the first prompt set.
        title: workspace.name.clone().or(stored.title.clone()),
        // The first working directory the session reported stays its project.
        project_path: stored
            .project_path
            .clone()
            .or(parsed.cwd.clone())
            .or(workspace.cwd.clone()),
        git_branch: parsed
            .branch
            .clone()
            .filter(|b| !b.is_empty())
            .or(stored.git_branch.clone()),
        created_at: stored.created_at,
        updated_at: parsed.last_ts.or(stored.updated_at),
        is_subagent: false,
        messages: std::mem::take(&mut parsed.messages),
        usage: std::mem::take(&mut parsed.usage),
    };
    let changed = !thread.messages.is_empty()
        || thread.title != stored.title
        || thread.project_path != stored.project_path
        || thread.git_branch != stored.git_branch;
    let result = if changed {
        match append_messages(conn, sid, &thread, stored_n)? {
            Some(n) => Append::Applied(n),
            // Another writer moved the thread since the check above.
            None => return Ok(Append::Fallback),
        }
    } else {
        Append::NoChange
    };
    let tag = cursor_tag(
        &fingerprint_file(live, new_offset)?,
        stored_n + thread.messages.len(),
        &parsed.totals,
    );
    write_cursor(
        conn,
        &session.key(),
        mtime,
        size,
        new_offset as i64,
        Some(&tag),
    )?;
    Ok(result)
}

/// The stored thread's metadata and message count, for folding appended lines onto it.
struct StoredThread {
    title: Option<String>,
    project_path: Option<String>,
    git_branch: Option<String>,
    created_at: Option<i64>,
    updated_at: Option<i64>,
    message_count: usize,
}

fn stored_thread(conn: &Connection, sid: i64, external_id: &str) -> Result<Option<StoredThread>> {
    Ok(conn
        .query_row(
            "SELECT title, project_path, git_branch, created_at, updated_at, message_count
             FROM threads WHERE source_id = ?1 AND external_id = ?2",
            params![sid, external_id],
            |r| {
                Ok(StoredThread {
                    title: r.get(0)?,
                    project_path: r.get(1)?,
                    git_branch: r.get(2)?,
                    created_at: r.get(3)?,
                    updated_at: r.get(4)?,
                    message_count: usize::try_from(r.get::<_, i64>(5)?).unwrap_or(usize::MAX),
                })
            },
        )
        .optional()?)
}

/// Token counters of one model at a `session.shutdown`, as stored: `input` excludes the
/// cached tokens, as the other sources' usage does.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Counters {
    input: i64,
    output: i64,
    cache_write: i64,
    cache_read: i64,
    /// `requests.count`: model calls, which `cost` reports as calls for this source.
    requests: i64,
}

/// The text of a shutdown's usage row. Its `, <n> requests` tail is the only place the
/// request count is kept (messages have no column for it, and adding one would mean
/// rebuilding the covering usage index over every message); [`usage_row_requests`] reads
/// it back for `cost`.
fn usage_row_text(model: &str, spent: &Counters) -> String {
    let mut text = format!(
        "Copilot CLI usage: {model}, {} input, {} cache read, {} cache write, {} output tokens",
        spent.input, spent.cache_read, spent.cache_write, spent.output
    );
    if spent.requests > 0 {
        text.push_str(&format!(", {} requests", spent.requests));
    }
    text
}

/// The request count a Copilot CLI usage row carries, if it recorded one.
pub fn usage_row_requests(text: &str) -> Option<i64> {
    let rest = text.strip_prefix("Copilot CLI usage: ")?;
    rest.strip_suffix(" requests")?
        .rsplit(", ")
        .next()?
        .parse()
        .ok()
}

#[derive(Default)]
struct Parsed {
    messages: Vec<ParsedMessage>,
    usage: Vec<(usize, MsgUsage)>,
    cwd: Option<String>,
    branch: Option<String>,
    started: Option<i64>,
    first_ts: Option<i64>,
    last_ts: Option<i64>,
    seen: HashSet<String>,
    /// Each model's counters at the previous shutdown. Shutdown metrics are session-wide
    /// running totals (`modelMetrics`: "accumulated", restored on resume from
    /// `session.usage_checkpoint`), so each shutdown's row carries only what was spent
    /// since the previous one, and a resumed session's rows sum to its total instead of
    /// counting the first run twice.
    totals: HashMap<String, Counters>,
}

impl Parsed {
    fn read(&mut self, text: &str) {
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            // Callers pass complete lines only, so a line that does not parse is broken.
            let Ok(ev) = serde_json::from_str::<Value>(line) else {
                count_malformed();
                continue;
            };
            count_line();
            self.event(&ev);
        }
    }

    fn event(&mut self, ev: &Value) {
        let flag = |k: &str| ev.get(k).and_then(Value::as_bool).unwrap_or(false);
        if flag("ephemeral") || flag("checkpoint") {
            return;
        }
        if ev
            .get("agentId")
            .and_then(Value::as_str)
            .is_some_and(|a| !a.is_empty())
        {
            return;
        }
        if let Some(id) = ev.get("id").and_then(Value::as_str) {
            if !self.seen.insert(id.to_string()) {
                return; // the same record in an archive and the live journal
            }
        }
        let ts = ev
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339);
        if let Some(t) = ts {
            self.first_ts.get_or_insert(t);
            self.last_ts = Some(self.last_ts.map_or(t, |l| l.max(t)));
        }
        let null = Value::Null;
        let data = ev.get("data").unwrap_or(&null);
        let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        let kind = ev.get("type").and_then(Value::as_str);
        match kind.unwrap_or_default() {
            "session.start" => {
                self.started = s(data, "startTime").as_deref().and_then(parse_rfc3339);
                self.context(data.get("context"));
            }
            "session.resume" => self.context(data.get("context")),
            "session.context_changed" => self.context(Some(data)),
            "user.message" => {
                let text = s(data, "content").unwrap_or_default();
                push_message(&mut self.messages, "user", &text, None, ts);
            }
            "assistant.message" => {
                let text = s(data, "content").unwrap_or_default();
                push_message(&mut self.messages, "assistant", &text, None, ts);
                for req in data
                    .get("toolRequests")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let name = s(req, "name").unwrap_or_else(|| "tool".into());
                    let text = match req.get("arguments") {
                        Some(Value::Null) | None => name.clone(),
                        Some(Value::String(a)) => format!("{name}: {a}"),
                        Some(a) => format!("{name}: {a}"),
                    };
                    push_message(&mut self.messages, "assistant", &text, Some(name), ts);
                }
            }
            "tool.execution_complete" => {
                let result = data.get("result").and_then(|r| r.get("content"));
                let text = match result.and_then(Value::as_str) {
                    Some(t) => t.to_string(),
                    None => data
                        .get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(Value::as_str)
                        .map(|m| format!("Error: {m}"))
                        .unwrap_or_default(),
                };
                // Same index-time cap as Claude tool results (see `claude::cap_tool_text`).
                let text = super::claude::cap_tool_text(text.trim());
                push_message(&mut self.messages, "tool", &text, None, ts);
            }
            "session.compaction_complete" => {
                if let Some(summary) = s(data, "summaryContent") {
                    let text = format!("Compaction summary:\n{summary}");
                    push_message(&mut self.messages, "system", &text, None, ts);
                }
            }
            "session.shutdown" => self.shutdown(data, ts),
            _ => count_skipped("", kind, SKIPPED_EVENTS),
        }
    }

    /// Take the project from the first working directory the session reports, and the
    /// branch from the latest.
    fn context(&mut self, ctx: Option<&Value>) {
        let Some(ctx) = ctx else {
            return;
        };
        if self.cwd.is_none() {
            self.cwd = ctx
                .get("cwd")
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty())
                .map(str::to_string);
        }
        if let Some(b) = ctx.get("branch").and_then(Value::as_str) {
            self.branch = Some(b.to_string());
        }
    }

    /// One `system` row per model that spent tokens since the previous shutdown, carrying
    /// that spend. A row of its own rather than usage on the last assistant message: the
    /// shutdown lands after that message was indexed, and an append only writes usage for
    /// new rows.
    fn shutdown(&mut self, data: &Value, ts: Option<i64>) {
        let Some(metrics) = data.get("modelMetrics").and_then(Value::as_object) else {
            return;
        };
        let n = |v: Option<&Value>| v.and_then(Value::as_i64).unwrap_or(0);
        for (model, m) in metrics {
            let u = m.get("usage");
            let get = |k: &str| n(u.and_then(|u| u.get(k)));
            let cache_read = get("cacheReadTokens");
            let cache_write = get("cacheWriteTokens");
            // `inputTokens` includes the cached ones (a real shutdown: 37,167 input =
            // 27,183 uncached + 9,984 cache read in its tokenDetails).
            let now = Counters {
                input: (get("inputTokens") - cache_read - cache_write).max(0),
                output: get("outputTokens"),
                cache_write,
                cache_read,
                requests: n(m.get("requests").and_then(|r| r.get("count"))),
            };
            let prev = self.totals.insert(model.clone(), now).unwrap_or_default();
            // Never a negative spend. A counter below its previous value started over (a
            // run resumed without its usage checkpoint), so all of it is new spend; the SDK
            // types call these totals session-wide, but no real session here has resumed,
            // so that is not verified.
            let since = |now: i64, prev: i64| if now >= prev { now - prev } else { now };
            let spent = Counters {
                input: since(now.input, prev.input),
                output: since(now.output, prev.output),
                cache_write: since(now.cache_write, prev.cache_write),
                cache_read: since(now.cache_read, prev.cache_read),
                requests: since(now.requests, prev.requests),
            };
            if spent == Counters::default() {
                continue;
            }
            let text = usage_row_text(model, &spent);
            let at = self.messages.len();
            push_message(&mut self.messages, "system", &text, None, ts);
            self.usage.push((
                at,
                MsgUsage {
                    model: model.clone(),
                    input: spent.input,
                    output: spent.output,
                    cache_write: spent.cache_write,
                    cache_read: spent.cache_read,
                },
            ));
        }
    }
}

/// The parts of `workspace.yaml` the index uses.
#[derive(Debug, Default, PartialEq)]
struct Workspace {
    cwd: Option<String>,
    branch: Option<String>,
    /// Only a name the user gave (`user_named: true`); otherwise Copilot fills `name` with
    /// the start of the first prompt, which the title derivation does better.
    name: Option<String>,
    /// Which client drove the session: `sdk` for programs built on the Copilot SDK.
    client_name: Option<String>,
}

/// Read the flat top-level `key: value` lines of `workspace.yaml` (no YAML parser in the
/// tree, and the file is a handful of scalars). A double-quoted value is decoded as a JSON
/// string, which covers the escapes Copilot writes (`\n`, `\"`).
fn read_workspace(path: &Path) -> Workspace {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Workspace::default();
    };
    let mut fields: HashMap<&str, String> = HashMap::new();
    for line in text.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        let value = if v.starts_with('"') {
            serde_json::from_str::<String>(v).unwrap_or_else(|_| v.trim_matches('"').to_string())
        } else {
            v.trim_matches('\'').to_string()
        };
        fields.insert(k.trim(), value);
    }
    let get = |k: &str| fields.get(k).filter(|v| !v.is_empty()).cloned();
    Workspace {
        cwd: get("cwd"),
        branch: get("branch"),
        client_name: get("client_name"),
        name: (get("user_named").as_deref() == Some("true"))
            .then(|| get("name").map(|n| truncate_title(n.lines().next().unwrap_or_default())))
            .flatten(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("calli_copcli_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ev(id: &str, ty: &str, ts: &str, data: Value) -> String {
        format!(
            "{}\n",
            serde_json::json!({"type": ty, "data": data, "id": id, "timestamp": ts, "parentId": null})
        )
    }

    /// A session in the shape Copilot CLI 1.0 writes (see the real `events.jsonl` sample in
    /// the module docs): start, prompt, an answer with a tool call, the tool's result, a
    /// sub-agent's turn and an ephemeral delta (both skipped), and a shutdown with usage.
    fn session_events() -> String {
        let big = "z".repeat(40_000);
        [
            ev("e1", "session.start", "2026-07-19T18:16:37.187Z", serde_json::json!({
                "sessionId": "s1", "startTime": "2026-07-19T18:16:37.158Z",
                "context": {"cwd": "/Users/me/ad-loop", "branch": "main"}})),
            ev("e2", "system.message", "2026-07-19T18:16:37.200Z", serde_json::json!({
                "role": "system", "content": "You are the GitHub Copilot CLI"})),
            ev("e3", "user.message", "2026-07-19T18:16:38.000Z", serde_json::json!({
                "content": "why does the review import fail", "transformedContent": "<current_datetime>x</current_datetime> why"})),
            ev("e4", "assistant.message", "2026-07-19T18:16:40.000Z", serde_json::json!({
                "messageId": "m1", "model": "gpt-5.3-codex", "content": "Running the tests.",
                "toolRequests": [{"toolCallId": "t1", "name": "bash", "arguments": {"command": "pnpm test"}}]})),
            ev("e5", "tool.execution_complete", "2026-07-19T18:16:45.000Z", serde_json::json!({
                "toolCallId": "t1", "success": true, "result": {"content": big}})),
            format!("{}\n", serde_json::json!({"type": "assistant.message", "agentId": "sub-1", "id": "e6",
                "timestamp": "2026-07-19T18:16:46.000Z", "data": {"messageId": "m2", "content": "sub-agent chatter"}})),
            format!("{}\n", serde_json::json!({"type": "assistant.message_delta", "ephemeral": true, "id": "e7",
                "timestamp": "2026-07-19T18:16:46.500Z", "data": {"messageId": "m3", "deltaContent": "partial"}})),
            ev("e8", "assistant.message", "2026-07-19T18:16:50.000Z", serde_json::json!({
                "messageId": "m4", "model": "gpt-5.3-codex", "content": "The env var is missing.", "toolRequests": []})),
            ev("e9", "session.shutdown", "2026-07-19T18:17:00.000Z", serde_json::json!({
                "shutdownType": "routine", "totalApiDurationMs": 5584, "sessionStartTime": 1784484997158_i64,
                "codeChanges": {"linesAdded": 0, "linesRemoved": 0, "filesModified": []},
                "modelMetrics": {"gpt-5.3-codex": {"requests": {"count": 2, "cost": 1},
                    "usage": {"inputTokens": 37167, "outputTokens": 217, "cacheReadTokens": 9984, "cacheWriteTokens": 0}}}})),
        ]
        .concat()
    }

    #[test]
    fn indexes_a_session_with_tools_and_usage() {
        let root = temp_root("basic");
        let dir = root.join("s1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("events.jsonl"), session_events()).unwrap();
        std::fs::write(
            dir.join("workspace.yaml"),
            "id: s1\ncwd: /Users/me/ad-loop\nbranch: main\nname: \"Fix the review import\\nfrom GBP\"\nuser_named: true\n",
        )
        .unwrap();
        // A flat-layout session beside it.
        std::fs::write(
            root.join("s0.jsonl"),
            ev(
                "f1",
                "user.message",
                "2026-01-01T00:00:00Z",
                serde_json::json!({"content": "hello from 0.0.3"}),
            ),
        )
        .unwrap();

        let dbp = root.join("index.db");
        let mut conn = crate::db::open(&dbp).unwrap();
        let report = scan_in(&mut conn, &root, &mut || {}).unwrap();
        assert_eq!((report.threads_indexed, report.errors), (2, 0));

        let (title, project, branch, created): (String, String, String, i64) = conn
            .query_row(
                "SELECT title, project_path, git_branch, created_at FROM threads WHERE external_id = 's1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(title, "Fix the review import");
        assert_eq!(project, "/Users/me/ad-loop");
        assert_eq!(branch, "main");
        assert_eq!(created, 1784484997);

        type Row = (String, String, Option<String>, Option<i64>, Option<i64>);
        let rows: Vec<Row> = conn
            .prepare(
                "SELECT m.role, m.text, m.model, m.input_tokens, m.cache_read_tokens FROM messages m
                 JOIN threads t ON t.id = m.thread_id WHERE t.external_id = 's1' ORDER BY m.seq",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let roles: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
        assert_eq!(
            roles,
            vec![
                "user",
                "assistant",
                "assistant",
                "tool",
                "assistant",
                "system"
            ]
        );
        assert_eq!(
            rows[0].1, "why does the review import fail",
            "what was typed, not the transformed prompt"
        );
        assert_eq!(rows[2].1, r#"bash: {"command":"pnpm test"}"#);
        assert!(rows[3].1.chars().count() <= super::super::claude::TOOL_TEXT_CAP_CHARS);
        assert!(!rows
            .iter()
            .any(|r| r.1.contains("sub-agent") || r.1.contains("partial")));
        let usage = &rows[5];
        assert_eq!(usage.2.as_deref(), Some("gpt-5.3-codex"));
        assert_eq!(
            (usage.3, usage.4),
            (Some(27183), Some(9984)),
            "input excludes cache reads"
        );

        // Unchanged: skipped. Resumed: the second shutdown adds only the new spend.
        assert_eq!(
            scan_in(&mut conn, &root, &mut || {})
                .unwrap()
                .threads_indexed,
            0
        );
        let resumed = ev(
            "r1",
            "session.resume",
            "2026-07-20T09:00:00Z",
            serde_json::json!({"context": {"cwd": "/Users/me/ad-loop", "branch": "fix"}}),
        ) + &ev(
            "r2",
            "user.message",
            "2026-07-20T09:00:05Z",
            serde_json::json!({"content": "and the webhook?"}),
        ) + &ev(
            "r3",
            "session.shutdown",
            "2026-07-20T09:05:00Z",
            serde_json::json!({
                "modelMetrics": {"gpt-5.3-codex": {"usage": {"inputTokens": 47167, "outputTokens": 317, "cacheReadTokens": 9984, "cacheWriteTokens": 0}}}}),
        );
        let mut body = session_events();
        body.push_str(&resumed);
        std::fs::write(dir.join("events.jsonl"), body).unwrap();
        let again = scan_in(&mut conn, &root, &mut || {}).unwrap();
        assert_eq!(again.threads_indexed, 1);
        let (input, output, n): (i64, i64, i64) = conn
            .query_row(
                "SELECT SUM(input_tokens), SUM(output_tokens), COUNT(*) FROM messages WHERE model IS NOT NULL",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (input, output, n),
            (37183, 317, 2),
            "the session total, counted once"
        );
        let branch: String = conn
            .query_row(
                "SELECT git_branch FROM threads WHERE external_id = 's1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(branch, "fix", "the latest branch");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skips_tombstoned_and_replayed_records() {
        let root = temp_root("tomb");
        let dir = root.join("s1");
        std::fs::create_dir_all(&dir).unwrap();
        // A replay checkpoint repeats the prompt; it is history already in the archive.
        let replay = format!(
            "{}\n",
            serde_json::json!({"type": "user.message", "checkpoint": true, "id": "c1",
                "timestamp": "2026-07-19T18:16:38.000Z", "data": {"content": "why does the review import fail"}})
        );
        std::fs::write(dir.join("events.1.jsonl"), session_events()).unwrap();
        std::fs::write(dir.join("events.jsonl"), replay + &session_events()).unwrap();
        let mut conn = crate::db::open(&root.join("index.db")).unwrap();
        scan_in(&mut conn, &root, &mut || {}).unwrap();
        let users: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE role = 'user'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(users, 1, "archive and live journal repeat records by id");

        let tid: i64 = conn
            .query_row("SELECT id FROM threads", [], |r| r.get(0))
            .unwrap();
        crate::cleanup::delete_threads(&mut conn, &[tid]).unwrap();
        std::fs::write(dir.join("events.jsonl"), session_events() + "\n").unwrap();
        assert_eq!(
            scan_in(&mut conn, &root, &mut || {})
                .unwrap()
                .threads_indexed,
            0
        );
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "a deleted session stays deleted");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_name_only_when_the_user_gave_one() {
        let root = temp_root("ws");
        let p = root.join("workspace.yaml");
        std::fs::write(&p, "cwd: /a\nbranch: main\nclient_name: sdk\nname: \"<diff-1>\\n:100644\"\nuser_named: false\nsummary_count: 0\n").unwrap();
        assert_eq!(
            read_workspace(&p),
            Workspace {
                cwd: Some("/a".into()),
                branch: Some("main".into()),
                name: None,
                client_name: Some("sdk".into()),
            }
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A GitHub Desktop commit-message run (an SDK client, the diff as its one prompt) and a
    /// session with no prompt are not conversations; a CLI conversation beside them is.
    #[test]
    fn indexes_conversations_not_sdk_runs() {
        let root = temp_root("sdk");
        let commit = root.join("d1");
        std::fs::create_dir_all(&commit).unwrap();
        std::fs::write(
            commit.join("workspace.yaml"),
            "id: d1\ncwd: /Users/me/ad-loop\nclient_name: sdk\nname: \"<diff-dd99>\\n:100644 M\\tsrc/a.ts\"\nuser_named: false\n",
        )
        .unwrap();
        std::fs::write(
            commit.join("events.jsonl"),
            ev(
                "d",
                "session.start",
                "2026-07-15T17:48:33Z",
                serde_json::json!({"context": {"cwd": "/Users/me/ad-loop"}}),
            ) + &ev(
                "u",
                "user.message",
                "2026-07-15T17:48:34Z",
                serde_json::json!({
                    "content": format!("<diff-dd99>\n{}", "+line\n".repeat(1000))}),
            ),
        )
        .unwrap();
        let silent = root.join("q1");
        std::fs::create_dir_all(&silent).unwrap();
        std::fs::write(
            silent.join("events.jsonl"),
            ev(
                "q",
                "session.start",
                "2026-07-15T17:48:33Z",
                serde_json::json!({}),
            ),
        )
        .unwrap();
        let chat = root.join("c1");
        std::fs::create_dir_all(&chat).unwrap();
        std::fs::write(
            chat.join("workspace.yaml"),
            "id: c1\ncwd: /Users/me/ad-loop\nclient_name: cli\n",
        )
        .unwrap();
        std::fs::write(chat.join("events.jsonl"), session_events()).unwrap();

        let mut conn = crate::db::open(&root.join("index.db")).unwrap();
        let report = scan_in(&mut conn, &root, &mut || {}).unwrap();
        assert_eq!((report.threads_indexed, report.errors), (1, 0));
        let ids: Vec<String> = conn
            .prepare("SELECT external_id FROM threads")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(ids, vec!["c1"]);
        // Skipped sessions are remembered as seen until their files change.
        let again = scan_in(&mut conn, &root, &mut || {}).unwrap();
        assert_eq!((again.threads_indexed, again.errors), (0, 0));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An append to a live journal is parsed from the cursor, not from the start: rows
    /// stored before it are untouched, and a rewrite falls back to a full parse.
    #[test]
    fn appends_parse_from_the_cursor() {
        let root = temp_root("cursor");
        let dir = root.join("s1");
        std::fs::create_dir_all(&dir).unwrap();
        let journal = dir.join("events.jsonl");
        std::fs::write(&journal, session_events()).unwrap();
        let mut conn = crate::db::open(&root.join("index.db")).unwrap();
        scan_in(&mut conn, &root, &mut || {}).unwrap();
        let key = journal.to_string_lossy().into_owned();
        let offset = |conn: &Connection| read_cursor(conn, &key).unwrap().unwrap().offset;
        assert_eq!(offset(&conn) as usize, session_events().len());
        let first_id: i64 = conn
            .query_row("SELECT MIN(id) FROM messages", [], |r| r.get(0))
            .unwrap();

        // An append, with a half-written line after it that waits for its newline.
        let more = ev(
            "a1",
            "user.message",
            "2026-07-19T18:20:00Z",
            serde_json::json!({"content": "one more thing"}),
        );
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&journal)
            .unwrap();
        std::io::Write::write_all(&mut f, format!("{more}{{\"type\":\"user.mess").as_bytes())
            .unwrap();
        drop(f);
        let report = scan_in(&mut conn, &root, &mut || {}).unwrap();
        assert_eq!((report.threads_indexed, report.messages_indexed), (1, 1));
        assert_eq!(offset(&conn) as usize, session_events().len() + more.len());
        let (n, still_first): (i64, i64) = conn
            .query_row("SELECT COUNT(*), MIN(id) FROM messages", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((n, still_first), (7, first_id), "appended, not rewritten");

        // Rewritten from the start (the cursor's fingerprint no longer matches): a full
        // parse, which finds the same seven rows and moves the cursor to the new end.
        let body =
            std::fs::read_to_string(&journal)
                .unwrap()
                .replacen("\"e1\"", "\"rewritten-1\"", 1);
        std::fs::write(&journal, &body).unwrap();
        scan_in(&mut conn, &root, &mut || {}).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 7);
        assert_eq!(offset(&conn) as usize, complete_lines(body.as_bytes()));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The parse canary: an invented event type and a broken line are counted; turn markers
    /// are skipped on purpose.
    #[test]
    fn parse_counts_name_unknown_types_and_broken_lines() {
        let text = concat!(
            r#"{"type":"user.message","id":"a","data":{"content":"hi"}}"#,
            "\n",
            r#"{"type":"assistant.turn_start","id":"b","data":{}}"#,
            "\n",
            r#"{"type":"assistant.hologram","id":"c","data":{"content":"x"}}"#,
            "\n",
            r#"{"type":"assistant.message","id":"d","data":"#,
            "\n",
        );
        let mut p = Parsed::default();
        let ((), stats) = crate::indexer::common::collect_parse_stats(|| p.read(text));
        assert_eq!(p.messages.len(), 1);
        assert_eq!((stats.lines, stats.malformed_lines), (4, 1));
        assert_eq!(
            stats.unknown_kinds,
            std::collections::BTreeMap::from([("assistant.hologram".to_string(), 1)])
        );
    }

    /// A shutdown whose counters fell below the previous one's (a run that started its
    /// totals over) counts them as new spend, never as a negative delta, per counter.
    #[test]
    fn shutdown_deltas_are_never_negative() {
        let mut p = Parsed::default();
        let shutdown = |input: i64, output: i64| {
            serde_json::json!({"modelMetrics": {"m": {"usage": {
                "inputTokens": input, "outputTokens": output, "cacheReadTokens": 0, "cacheWriteTokens": 0}}}})
        };
        p.shutdown(&shutdown(1000, 50), None);
        p.shutdown(&shutdown(400, 80), None);
        let spent: Vec<(i64, i64)> = p.usage.iter().map(|(_, u)| (u.input, u.output)).collect();
        assert_eq!(spent, vec![(1000, 50), (400, 30)]);
    }

    /// Each usage row says how many model requests it covers, for `cost`'s call counts.
    #[test]
    fn usage_rows_carry_their_request_count() {
        let mut p = Parsed::default();
        let shutdown = |requests: i64| {
            serde_json::json!({"modelMetrics": {"m": {"requests": {"count": requests},
                "usage": {"inputTokens": requests * 10, "outputTokens": 1}}}})
        };
        p.shutdown(&shutdown(3), None);
        p.shutdown(&shutdown(5), None);
        let counts: Vec<Option<i64>> = p
            .messages
            .iter()
            .map(|m| usage_row_requests(&m.text))
            .collect();
        assert_eq!(counts, vec![Some(3), Some(2)]);
        assert_eq!(
            usage_row_requests(
                "Copilot CLI usage: m, 1 input, 0 cache read, 0 cache write, 1 output tokens"
            ),
            None,
            "no count recorded"
        );
        assert_eq!(usage_row_requests("3 requests"), None);
    }

    /// Real-data smoke test. Run with
    /// `cargo test -p callimachus-core -- --ignored real_copilot_cli_index --nocapture`.
    #[test]
    #[ignore]
    fn real_copilot_cli_index() {
        let p =
            std::env::temp_dir().join(format!("callimachus_copcli_real_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let mut conn = crate::db::open(&p).unwrap();
        let t0 = std::time::Instant::now();
        let report = scan(&mut conn, &mut || {}).unwrap();
        eprintln!("{report:?} in {:?}", t0.elapsed());
        let (threads, msgs, tokens): (i64, i64, Option<i64>) = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM threads), COUNT(*), SUM(output_tokens) FROM messages",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        eprintln!("threads {threads}, messages {msgs}, output tokens {tokens:?}");
        let again = scan(&mut conn, &mut || {}).unwrap();
        assert_eq!(again.threads_indexed, 0);
        let _ = std::fs::remove_file(&p);
    }
}
