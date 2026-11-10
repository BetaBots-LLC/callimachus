//! Codex CLI indexer. Thread metadata lives in the state DB `state_<N>.sqlite` (`threads`
//! table; `state_5` today) under Codex's SQLite home, and the conversation itself in the
//! per-thread rollout file at `threads.rollout_path` (JSONL of `response_item` /
//! `event_msg` / etc.).

use super::common::{
    begin_file, count_file, count_line, count_malformed, count_skipped, parse_rfc3339,
    push_message, truncate_title,
};
use super::{
    file_change_state, set_file_state, source_id, upsert_thread, IndexReport, MsgUsage,
    ParsedMessage, ParsedThread,
};
use anyhow::Result;
use rusqlite::Connection;
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

pub const KIND: &str = "codex";

/// Rollout record types skipped on purpose (the parse canary, `parse_health`): the session
/// header (read for metadata by `meta_from_rollout`), the UI events that mirror response
/// items (`event_msg`, whose `token_count` is read for usage), and compaction markers.
const SKIPPED_TYPES: &[&str] = &["session_meta", "event_msg", "compacted"];

/// Response item types skipped on purpose: the model's reasoning, and snapshots of the
/// worktree.
const SKIPPED_ITEMS: &[&str] = &["reasoning", "ghost_snapshot"];

static CODEX_HOME_OVERRIDE: std::sync::RwLock<Option<PathBuf>> = std::sync::RwLock::new(None);

/// Point the indexer at a relocated Codex home (a Settings value). A GUI app launched from
/// Finder/Dock never sees the shell's `CODEX_HOME`, so the app needs a way to set it
/// explicitly. `None` (or an empty path) restores the default lookup.
pub fn set_codex_home_override(dir: Option<PathBuf>) {
    if let Ok(mut slot) = CODEX_HOME_OVERRIDE.write() {
        *slot = dir
            .filter(|d| !d.as_os_str().is_empty())
            .map(super::claude::expand_tilde);
    }
}

/// `app_config` key holding a user-chosen Codex home (a Settings value).
pub const CODEX_HOME_SETTING: &str = "paths.codex_home";

/// Apply the [`CODEX_HOME_SETTING`] override from the index (cleared when unset). Called at
/// the start of every scan, so a changed setting takes effect on the next pass.
pub fn load_codex_home_setting(conn: &Connection) {
    set_codex_home_override(super::claude::read_path_setting(conn, CODEX_HOME_SETTING));
}

/// Codex's home: the override, else `$CODEX_HOME`, else `~/.codex`. The CLI, the Desktop
/// app and the IDE extension all honor `CODEX_HOME`, so the index must too.
pub fn codex_root() -> Option<PathBuf> {
    if let Some(dir) = CODEX_HOME_OVERRIDE.read().ok().and_then(|g| g.clone()) {
        return Some(dir);
    }
    if let Some(dir) = std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()) {
        return Some(super::claude::expand_tilde(PathBuf::from(dir)));
    }
    dirs::home_dir().map(|h| h.join(".codex"))
}

/// The directory holding Codex's SQLite state DB. Codex resolves it as `sqlite_home` in
/// `config.toml`, else `$CODEX_SQLITE_HOME`, else the Codex home
/// (`cfg.sqlite_home.or(sqlite_home_env).unwrap_or(codex_home)` in
/// codex-rs/core/src/config/mod.rs, checked 2026-11-07). A relative value is taken from the
/// Codex home, where the config file lives.
fn sqlite_home(root: &Path) -> PathBuf {
    let resolve = |raw: &str| {
        let p = super::claude::expand_tilde(PathBuf::from(raw.trim()));
        if p.is_absolute() {
            p
        } else {
            root.join(p)
        }
    };
    let configured = fs::read_to_string(root.join("config.toml"))
        .ok()
        .and_then(|text| text.parse::<toml_edit::DocumentMut>().ok())
        .and_then(|doc| {
            doc.get("sqlite_home")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .filter(|v| !v.trim().is_empty());
    if let Some(dir) = configured {
        return resolve(&dir);
    }
    if let Some(dir) = std::env::var("CODEX_SQLITE_HOME")
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
        return resolve(&dir);
    }
    root.to_path_buf()
}

/// Every `state_<N>.sqlite` in `dir`, the one Codex wrote last first. Codex bumps N when the
/// state schema changes (`STATE_DB_FILENAME` in codex-rs/state/src/sqlite.rs) and leaves the
/// old file behind, so the highest N is usually the live one, but not always: a downgraded
/// Codex writes its own lower N beside the newer leftover, which then never changes again.
/// So the order is by last write (the file or its `-wal`, where Codex's writes land until a
/// checkpoint), with the higher N first on a tie. The rest are fallbacks for when one can't
/// be read (a schema this indexer doesn't know yet).
fn state_dbs(dir: &Path) -> Vec<PathBuf> {
    let modified = |p: &Path| fs::metadata(p).and_then(|m| m.modified()).ok();
    let mut found: Vec<(Option<std::time::SystemTime>, u32, PathBuf)> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let n = name
                .to_str()?
                .strip_prefix("state_")?
                .strip_suffix(".sqlite")?
                .parse::<u32>()
                .ok()?;
            let path = e.path();
            let mut wal = path.clone().into_os_string();
            wal.push("-wal");
            let written = modified(&path).max(modified(Path::new(&wal)));
            Some((written, n, path))
        })
        .collect();
    found.sort_by_key(|f| std::cmp::Reverse((f.0, f.1)));
    found.into_iter().map(|(_, _, p)| p).collect()
}

/// A state DB that is busy (Codex mid-migration or mid-checkpoint holds the lock), as
/// opposed to one this indexer can't read. Busy is worth waiting for: falling back to an
/// older DB would write its stale titles, and those stick until the rollout next changes.
fn is_busy(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<rusqlite::Error>().is_some_and(|e| {
            matches!(
                e.sqlite_error_code(),
                Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
            )
        })
    })
}

struct ThreadMeta {
    id: String,
    title: String,
    cwd: String,
    git_branch: Option<String>,
    created_at: Option<i64>,
    updated_at: Option<i64>,
    rollout_path: String,
    first_user_message: String,
}

/// Index Codex history. The Codex CLI, the Desktop app, and the VS Code/IDE
/// extension (`openai.chatgpt`) all share `~/.codex` (CODEX_HOME). Two passes:
///   1. the newest `state_<N>.sqlite` thread DB under the SQLite home (authoritative
///      title/cwd/branch/timestamps), then
///   2. any `sessions/` + `archived_sessions/` rollout JSONL not already covered.
///
/// Pass 2 exists because a session is not always registered in the thread DB the
/// way pass 1 expects: some extension builds write only the rollout file, and the
/// DB version isn't always `state_5`. The rollout file is self-describing via its
/// `session_meta` header, so it can be indexed without the DB. Dedup is by session
/// id, so a session present in both passes is indexed once (pass 1 wins).
pub fn scan(conn: &mut Connection, tick: &mut dyn FnMut()) -> Result<IndexReport> {
    load_codex_home_setting(conn);
    let Some(root) = codex_root() else {
        return Ok(IndexReport::default());
    };
    scan_in(conn, &root, tick)
}

fn scan_in(conn: &mut Connection, root: &Path, tick: &mut dyn FnMut()) -> Result<IndexReport> {
    let mut report = IndexReport::default();
    if !root.exists() {
        return Ok(report);
    }
    let sid = source_id(conn, KIND)?;
    let mut seen: HashSet<String> = HashSet::new();
    // Rollout files pass 1 already covered. The watcher re-runs this scan on every Codex
    // write, so without this pass 2 would reopen and JSON-parse every registered session's
    // header each time, only to drop it on the id check. PathBuf equality compares
    // components, so a doubled or trailing separator in the DB's spelling still matches;
    // a path spelled any other way just misses here and is deduped by id as before.
    let mut registered: HashSet<PathBuf> = HashSet::new();

    // Pass 1: thread metadata from the most recently written state DB that reads. Only
    // when none does (or there is none) does every session fall through to pass 2. A busy
    // DB ends the pass instead: the next one retries it, rather than indexing from an older
    // DB's (or no DB's) titles that would then stick.
    let mut metas = None;
    for db in state_dbs(&sqlite_home(root)) {
        match read_thread_metas(&db) {
            Ok(m) => {
                metas = Some(m);
                break;
            }
            Err(e) if is_busy(&e) => {
                app_log!(
                    "[index] {} is busy, retrying next pass: {e:#}",
                    db.display()
                );
                report.errors += 1;
                return Ok(report);
            }
            Err(e) => app_log!("[index] {}: {e:#}", db.display()),
        }
    }
    if let Some(metas) = metas {
        for meta in metas {
            tick();
            seen.insert(meta.id.clone());
            registered.insert(PathBuf::from(&meta.rollout_path));
            report.record_at(
                Path::new(&meta.rollout_path),
                index_rollout(conn, sid, &meta),
            );
        }
    }

    // Pass 2: rollout JSONL on disk not registered in the state DB.
    let mut files = Vec::new();
    collect_rollout_files(&root.join("sessions"), &mut files);
    collect_rollout_files(&root.join("archived_sessions"), &mut files);
    for f in files {
        tick();
        if registered.contains(&f) {
            continue; // indexed (or checked unchanged) by pass 1
        }
        let Some(meta) = meta_from_rollout(&f) else {
            continue; // no readable session_meta header
        };
        if meta.id.is_empty() || !seen.insert(meta.id.clone()) {
            continue; // missing id, or already indexed via the state DB
        }
        report.record_at(
            Path::new(&meta.rollout_path),
            index_rollout(conn, sid, &meta),
        );
    }
    Ok(report)
}

fn read_thread_metas(state_db: &Path) -> Result<Vec<ThreadMeta>> {
    let ro = super::open_external_readonly(state_db)?;
    let mut stmt = ro.prepare(
        "SELECT id, title, cwd, git_branch, created_at, updated_at, rollout_path, first_user_message
         FROM threads",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(ThreadMeta {
            id: r.get(0)?,
            title: r.get(1)?,
            cwd: r.get(2)?,
            git_branch: r.get::<_, Option<String>>(3)?,
            created_at: r.get::<_, Option<i64>>(4)?,
            updated_at: r.get::<_, Option<i64>>(5)?,
            rollout_path: r.get(6)?,
            first_user_message: r.get::<_, Option<String>>(7)?.unwrap_or_default(),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn index_rollout(conn: &mut Connection, sid: i64, meta: &ThreadMeta) -> Result<Option<usize>> {
    let path = Path::new(&meta.rollout_path);
    if !path.exists() {
        return Ok(None);
    }
    let (unchanged, mtime, size) = file_change_state(conn, path)?;
    if unchanged {
        return Ok(None);
    }
    // The user deleted this thread: don't bring it back (and don't pay to parse it).
    if crate::cleanup::is_tombstoned(conn, sid, &meta.id)? {
        set_file_state(conn, &meta.rollout_path, KIND, mtime, size)?;
        return Ok(None);
    }

    let Rollout { messages, usage } = parse_rollout(path)?;
    // `codex exec` runs the app's own distill / Ask calls (codex-cli engine) and always
    // writes a rollout; those are not the user's history.
    let cwd = (!meta.cwd.is_empty()).then_some(meta.cwd.as_str());
    let user_texts = messages
        .iter()
        .filter(|m| m.role == "user")
        .take(3)
        .map(|m| m.text.as_str());
    if crate::agent::is_app_cli_session(cwd, user_texts) {
        set_file_state(conn, &meta.rollout_path, KIND, mtime, size)?;
        return Ok(None);
    }
    let title = if !meta.title.trim().is_empty() {
        Some(meta.title.clone())
    } else if !meta.first_user_message.trim().is_empty() {
        first_line(&meta.first_user_message)
    } else {
        // Pass-2 sessions carry no state-DB metadata: derive a title from the
        // first user message in the rollout itself.
        messages
            .iter()
            .find(|m| m.role == "user")
            .and_then(|m| first_line(&m.text))
    };
    let thread = ParsedThread {
        external_id: meta.id.clone(),
        title,
        project_path: (!meta.cwd.is_empty()).then(|| meta.cwd.clone()),
        git_branch: meta.git_branch.clone().filter(|b| !b.is_empty()),
        created_at: meta.created_at,
        updated_at: meta.updated_at,
        is_subagent: false,
        usage,
        messages,
    };
    let n = upsert_thread(conn, sid, &thread)?;
    set_file_state(conn, &meta.rollout_path, KIND, mtime, size)?;
    Ok(Some(n))
}

/// Recursively collect `rollout-*.jsonl` files under `dir` (handles the
/// `sessions/YYYY/MM/DD/` partitioning).
fn collect_rollout_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rollout_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl")
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("rollout-"))
        {
            out.push(path);
        }
    }
}

/// Build a `ThreadMeta` from a rollout file's `session_meta` header (its first
/// line), for sessions not present in the state DB. Reads ONLY the first line, so
/// it stays cheap even for large rollouts and for files that will be deduped away.
fn meta_from_rollout(path: &Path) -> Option<ThreadMeta> {
    let file = fs::File::open(path).ok()?;
    let mut first = String::new();
    BufReader::new(file).read_line(&mut first).ok()?;
    let obj: Value = serde_json::from_str(first.trim()).ok()?;
    if obj.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let p = obj.get("payload")?;
    let id = p.get("id").and_then(Value::as_str)?.to_string();
    let cwd = p
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let created_at = p
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339)
        .or_else(|| {
            obj.get("timestamp")
                .and_then(Value::as_str)
                .and_then(parse_rfc3339)
        });
    let updated_at = fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .or(created_at);
    Some(ThreadMeta {
        id,
        title: String::new(),
        cwd,
        git_branch: None,
        created_at,
        updated_at,
        rollout_path: path.to_string_lossy().to_string(),
        first_user_message: String::new(),
    })
}

/// A rollout's messages and the token usage of each model call, keyed like
/// [`ParsedThread::usage`].
#[derive(Default)]
struct Rollout {
    messages: Vec<ParsedMessage>,
    usage: Vec<(usize, MsgUsage)>,
}

/// Parse a rollout JSONL file into ordered messages and their usage.
fn parse_rollout(path: &Path) -> Result<Rollout> {
    let content = fs::read_to_string(path)?;
    let file = begin_file();
    let mut thread = ParsedThread::default();
    let mut tokens = TokenCounter::default();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(line) else {
            count_malformed();
            continue;
        };
        count_line();
        let kind = obj.get("type").and_then(Value::as_str);
        let Some(payload) = obj.get("payload") else {
            count_skipped("", kind, SKIPPED_TYPES);
            continue;
        };
        match kind {
            Some("response_item") => {
                let ts = obj
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .and_then(parse_rfc3339);
                ingest_payload(&mut thread.messages, payload, ts);
            }
            // Each turn names its model; usage events that follow are that model's.
            Some("turn_context") => {
                if let Some(m) = payload.get("model").and_then(Value::as_str) {
                    tokens.model = Some(m.to_string());
                }
            }
            Some("event_msg")
                if payload.get("type").and_then(Value::as_str) == Some("token_count") =>
            {
                tokens.ingest(&mut thread, payload);
            }
            other => count_skipped("", other, SKIPPED_TYPES),
        }
    }
    count_file(file, thread.messages.len());
    Ok(Rollout {
        messages: thread.messages,
        usage: thread.usage,
    })
}

/// A rollout's messages and usage as a thread, for the usage backfill
/// (`indexer::usage_backfill`), which writes only the usage onto the stored rows.
pub(super) fn parse_rollout_thread(path: &Path) -> Result<ParsedThread> {
    let Rollout { messages, usage } = parse_rollout(path)?;
    Ok(ParsedThread {
        messages,
        usage,
        ..Default::default()
    })
}

/// Turns Codex's `token_count` events into per-call usage. Each event carries
/// `info.total_token_usage`, the session's running total, so a call's usage is the
/// difference from the previous event; Codex repeats an event with an unchanged total (a
/// rate-limit update), which adds nothing. A count that restarted (the total equals the
/// event's own `last_token_usage`, or went down) takes `last_token_usage` instead. Shape checked against
/// `TokenUsage` / `TokenCountEvent` in codex-rs/protocol/src/protocol.rs on 2026-11-07:
/// `input_tokens` includes the cached (and cache-write) tokens, `output_tokens` includes
/// reasoning.
#[derive(Default)]
struct TokenCounter {
    model: Option<String>,
    total: Option<[i64; 4]>,
    /// The first message of the call the next event reports: messages pushed since the
    /// previous event belong to it.
    call_start: usize,
}

impl TokenCounter {
    fn ingest(&mut self, thread: &mut ParsedThread, payload: &Value) {
        let Some(info) = payload.get("info").filter(|i| i.is_object()) else {
            return;
        };
        let Some(total) = info.get("total_token_usage").map(token_counts) else {
            return;
        };
        let last = info.get("last_token_usage").map(token_counts);
        let delta = match self.total {
            None => total,
            // A repeat (a rate-limit update) adds nothing.
            Some(prev) if total == prev => [0; 4],
            // The count restarted at this call: its total is just this call. Differencing
            // would lose whatever the previous total held (1000 + 3000 input read as 2000).
            Some(_) if last == Some(total) => total,
            Some(prev) if total.iter().zip(prev).all(|(now, was)| *now >= was) => {
                [0, 1, 2, 3].map(|i| total[i] - prev[i])
            }
            // The count restarted earlier and went down: the event's own call is all we know.
            Some(_) => last.unwrap_or([0; 4]),
        };
        self.total = Some(total);
        let start = std::mem::replace(&mut self.call_start, thread.messages.len());
        if delta == [0; 4] {
            return;
        }
        let [input, cached, cache_write, output] = delta;
        // On the call's first assistant row (its text or first tool call), as Claude Code's
        // usage sits; the user prompt or tool output that fed the call came before it.
        let at = thread.messages[start..]
            .iter()
            .position(|m| m.role == "assistant")
            .map(|i| start + i);
        thread.attach_usage(
            at,
            MsgUsage {
                // A rollout from before turns named their model: counted, but unpriced.
                model: self.model.clone().unwrap_or_else(|| "codex".to_string()),
                input: (input - cached - cache_write).max(0),
                output,
                cache_write,
                cache_read: cached,
            },
        );
    }
}

/// `[input, cached input, cache-write input, output]` of one Codex `TokenUsage` object.
fn token_counts(u: &Value) -> [i64; 4] {
    let n = |k: &str| u.get(k).and_then(Value::as_i64).unwrap_or(0).max(0);
    [
        n("input_tokens"),
        n("cached_input_tokens"),
        n("cache_write_input_tokens"),
        n("output_tokens"),
    ]
}

fn ingest_payload(out: &mut Vec<ParsedMessage>, payload: &Value, ts: Option<i64>) {
    match payload.get("type").and_then(Value::as_str) {
        Some("message") => {
            let role = match payload.get("role").and_then(Value::as_str) {
                Some("assistant") => "assistant",
                Some("user") => "user",
                _ => "system", // developer / system instructions
            };
            push_message(out, role, &content_text(payload.get("content")), None, ts);
        }
        Some("function_call") => {
            let name = payload
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string();
            let args = payload
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            push_message(out, "assistant", &format!("{name}: {args}"), Some(name), ts);
        }
        // Freeform tools, `apply_patch` above all: the input is the raw patch text. Capped
        // (a patch can carry whole files) by `cap_call_text`, which keeps every `*** ` file
        // line `file_edits` reads.
        Some("custom_tool_call") => {
            let (name, text) = custom_call_text(payload);
            push_message(out, "assistant", &cap_call_text(&text), Some(name), ts);
        }
        Some("function_call_output") | Some("custom_tool_call_output") => {
            let output = payload.get("output").map(value_to_text).unwrap_or_default();
            // Same index-time cap as Claude tool results (see `claude::cap_tool_text`).
            let output = super::claude::cap_tool_text(output.trim());
            push_message(out, "tool", &output, None, ts);
        }
        other => count_skipped("item:", other, SKIPPED_ITEMS),
    }
}

/// A `custom_tool_call` payload's tool name and its `Name: input` text, uncapped.
fn custom_call_text(payload: &Value) -> (String, String) {
    let name = payload
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("tool")
        .to_string();
    let input = payload.get("input").map(value_to_text).unwrap_or_default();
    let text = format!("{name}: {input}").trim().to_string();
    (name, text)
}

/// A freeform call's text capped at [`super::claude::TOOL_TEXT_CAP_CHARS`]. A patch over the
/// cap keeps every `*** ` line (each file it adds, updates, deletes or moves) and elides hunk
/// lines once the budget is spent: `cap_tool_text`'s head plus tail drops the file headers in
/// the middle of a large multi-file patch, and those are what `file_edits` records.
/// Deterministic, so a re-parse stores the same text.
fn cap_call_text(text: &str) -> String {
    let cap = super::claude::TOOL_TEXT_CAP_CHARS;
    if text.len() <= cap || !text.contains("*** Begin Patch") {
        return super::claude::cap_tool_text(text);
    }
    let is_header = |l: &str| l.trim_start().starts_with("*** ");
    let headers: usize = text
        .lines()
        .filter(|l| is_header(l))
        .map(|l| l.len() + 1)
        .sum();
    let mut budget = cap.saturating_sub(headers);
    let mut out = String::with_capacity(cap.max(headers) + 256);
    let mut elided = 0usize;
    let flush = |out: &mut String, elided: &mut usize| {
        if *elided > 0 {
            out.push_str(&format!("[… {elided} patch lines elided …]\n"));
            *elided = 0;
        }
    };
    for line in text.lines() {
        if is_header(line) {
            flush(&mut out, &mut elided);
            out.push_str(line);
            out.push('\n');
        } else if line.len() < budget {
            budget -= line.len() + 1;
            flush(&mut out, &mut elided);
            out.push_str(line);
            out.push('\n');
        } else {
            budget = 0;
            elided += 1;
        }
    }
    flush(&mut out, &mut elided);
    out.truncate(out.trim_end().len());
    out
}

/// The freeform tool calls (`custom_tool_call`, `apply_patch` above all) in a rollout file,
/// as (timestamp, tool name, `Name: input`), uncapped. An index from before 0034 never stored
/// them; the `file_edits` backfill reads them from here instead of re-parsing the thread.
pub(crate) fn custom_tool_calls(path: &Path) -> Result<Vec<(Option<i64>, String, String)>> {
    let file = fs::File::open(path)?;
    let mut out = Vec::new();
    for line in BufReader::new(file).lines().map_while(std::io::Result::ok) {
        // Most lines are messages and outputs: skip them before paying for a JSON parse.
        if !line.contains("\"custom_tool_call\"") {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if obj.get("type").and_then(Value::as_str) != Some("response_item") {
            continue;
        }
        let Some(payload) = obj
            .get("payload")
            .filter(|p| p.get("type").and_then(Value::as_str) == Some("custom_tool_call"))
        else {
            continue;
        };
        let ts = obj
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339);
        let (name, text) = custom_call_text(payload);
        out.push((ts, name, text));
    }
    Ok(out)
}

/// The session id (the thread's `external_id`) a rollout file belongs to: the UUID that ends
/// its name (`rollout-<time>-<uuid>.jsonl`), else its `session_meta` header.
pub(crate) fn rollout_session_id(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let tail = stem.get(stem.len().checked_sub(36)?..)?;
    let is_uuid = tail.len() == 36
        && tail.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        });
    if is_uuid {
        return Some(tail.to_ascii_lowercase());
    }
    meta_from_rollout(path).map(|m| m.id)
}

/// Join the text of a message's content blocks (input_text / output_text / text).
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| match b.get("type").and_then(Value::as_str) {
                Some("input_text" | "output_text" | "text") => {
                    b.get("text").and_then(Value::as_str).map(str::to_string)
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Best-effort stringification of a JSON value (for tool output).
fn value_to_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A title from the first line of `s`, or None when `s` is blank.
fn first_line(s: &str) -> Option<String> {
    let line = s.trim().lines().next()?;
    Some(truncate_title(line))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// The parse canary: an invented record type, an invented response item and a broken
    /// line are counted; the session header, UI events and reasoning are skipped on purpose.
    #[test]
    fn parse_counts_name_unknown_types_and_broken_lines() {
        let dir = crate::db::test_support::TestDir::new("codex_canary");
        let path = dir.with_file_name("rollout-x.jsonl");
        let sample = r#"{"timestamp":"2026-04-25T22:09:38.623Z","type":"session_meta","payload":{"id":"t1","cwd":"/proj"}}
{"type":"event_msg","payload":{"type":"agent_message","message":"x"}}
{"type":"response_item","payload":{"type":"reasoning","summary":[]}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}}
{"type":"response_item","payload":{"type":"telepathy","text":"x"}}
{"type":"brand_new_record","payload":{}}
{"type":"response_item","payload":
"#;
        std::fs::write(&path, sample).unwrap();
        let (rollout, stats) =
            crate::indexer::common::collect_parse_stats(|| parse_rollout(&path).unwrap());
        assert_eq!(rollout.messages.len(), 1);
        assert_eq!((stats.lines, stats.malformed_lines), (7, 1));
        assert_eq!(
            stats.unknown_kinds,
            std::collections::BTreeMap::from([
                ("brand_new_record".to_string(), 1),
                ("item:telepathy".to_string(), 1),
            ])
        );
        assert_eq!((stats.files, stats.files_with_zero_messages), (1, 0));
    }

    #[test]
    fn parses_rollout_messages() {
        let mut path = std::env::temp_dir();
        path.push(format!("callimachus_codex_{}.jsonl", std::process::id()));
        let sample = r#"{"timestamp":"2026-04-25T22:09:38.623Z","type":"session_meta","payload":{"id":"t1","cwd":"/proj"}}
{"timestamp":"2026-04-25T22:09:39.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"fix the warranty audit"}]}}
{"timestamp":"2026-04-25T22:09:40.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"On it"}]}}
{"timestamp":"2026-04-25T22:09:41.000Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"cmd\":\"ls\"}"}}
{"timestamp":"2026-04-25T22:09:42.000Z","type":"response_item","payload":{"type":"custom_tool_call","status":"completed","call_id":"c1","name":"apply_patch","input":"*** Begin Patch\n*** Update File: src/audit.rs\n@@\n-a\n+b\n*** End Patch\n"}}
{"timestamp":"2026-04-25T22:09:43.000Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"c1","output":"Success. Updated the following files:\nM src/audit.rs"}}
"#;
        std::fs::File::create(&path)
            .unwrap()
            .write_all(sample.as_bytes())
            .unwrap();

        let msgs = parse_rollout(&path).unwrap().messages;
        // user, assistant, function_call, custom_tool_call, its output (session_meta ignored)
        assert_eq!(msgs.len(), 5);
        assert_eq!(msgs[0].role, "user");
        assert!(msgs[0].text.contains("warranty"));
        assert_eq!(msgs[2].tool_name.as_deref(), Some("shell"));
        assert_eq!(msgs[3].tool_name.as_deref(), Some("apply_patch"));
        assert_eq!(
            super::super::file_edits::edits_of("apply_patch", &msgs[3].text),
            vec![(
                "src/audit.rs".to_string(),
                super::super::file_edits::Op::Edit
            )]
        );
        assert_eq!(msgs[4].role, "tool");
    }

    #[test]
    fn a_big_patch_keeps_every_file_line() {
        let mut patch = String::from("apply_patch: *** Begin Patch\n");
        for i in 0..40 {
            patch.push_str(&format!("*** Update File: src/m{i}.rs\n@@\n"));
            for j in 0..100 {
                patch.push_str(&format!("+line {j} of a long hunk in file {i}\n"));
            }
        }
        patch.push_str("*** End Patch");
        let capped = cap_call_text(&patch);
        assert!(capped.len() < super::super::claude::TOOL_TEXT_CAP_CHARS + 1_500);
        let edits = super::super::file_edits::edits_of("apply_patch", &capped);
        assert_eq!(edits.len(), 40, "every file survives the cap");
        assert!(capped.contains("patch lines elided"));
        assert_eq!(cap_call_text(&patch), capped, "deterministic");
        // Not a patch, or under the cap: the usual cap.
        assert_eq!(cap_call_text("apply_patch: x"), "apply_patch: x");
    }

    #[test]
    fn rollout_ids_come_from_the_file_name() {
        assert_eq!(
            rollout_session_id(Path::new(
                "/x/rollout-2026-04-25T15-09-15-019dc6b0-998d-7a31-a39d-1229e6b38072.jsonl"
            ))
            .as_deref(),
            Some("019dc6b0-998d-7a31-a39d-1229e6b38072")
        );
        assert_eq!(
            rollout_session_id(Path::new("/no/such/rollout-x.jsonl")),
            None
        );
    }

    /// `token_count` totals are cumulative: each call's usage is the difference, a repeated
    /// event adds nothing, and the usage sits on the call's first assistant row with the
    /// turn's model. Cached input comes out of fresh input.
    #[test]
    fn token_counts_become_per_call_usage() {
        let path = std::env::temp_dir().join(format!(
            "callimachus_codex_tok_{}.jsonl",
            std::process::id()
        ));
        let tc = |input: i64, cached: i64, output: i64| {
            format!(
                r#"{{"timestamp":"2026-04-25T22:09:45.000Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"output_tokens":{output},"reasoning_output_tokens":0,"total_tokens":{}}},"last_token_usage":{{"input_tokens":1,"cached_input_tokens":0,"output_tokens":1,"reasoning_output_tokens":0,"total_tokens":2}},"model_context_window":272000}},"rate_limits":null}}}}"#,
                input + output
            )
        };
        let lines = [
            r#"{"timestamp":"2026-04-25T22:09:38.623Z","type":"session_meta","payload":{"id":"t1","cwd":"/proj"}}"#.to_string(),
            r#"{"timestamp":"2026-04-25T22:09:38.700Z","type":"turn_context","payload":{"cwd":"/proj","model":"gpt-5.3-codex"}}"#.to_string(),
            r#"{"timestamp":"2026-04-25T22:09:39.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"fix it"}]}}"#.to_string(),
            r#"{"timestamp":"2026-04-25T22:09:39.500Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":null}}"#.to_string(),
            r#"{"timestamp":"2026-04-25T22:09:41.000Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{}"}}"#.to_string(),
            tc(1000, 200, 50),
            tc(1000, 200, 50),
            r#"{"timestamp":"2026-04-25T22:09:42.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c","output":"ok"}}"#.to_string(),
            r#"{"timestamp":"2026-04-25T22:09:43.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}}"#.to_string(),
            tc(2500, 1100, 80),
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        let r = parse_rollout(&path).unwrap();
        assert_eq!(r.messages.len(), 4);
        let got: Vec<(usize, &str, [i64; 4])> = r
            .usage
            .iter()
            .map(|(i, u)| {
                (
                    *i,
                    u.model.as_str(),
                    [u.input, u.output, u.cache_write, u.cache_read],
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                (1, "gpt-5.3-codex", [800, 50, 0, 200]),
                (3, "gpt-5.3-codex", [600, 30, 0, 900]),
            ]
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A restarted running total: one that lands above the previous total with the event's
    /// own call as the whole of it, and one that drops below. Each call counts in full.
    #[test]
    fn a_restarted_token_count_keeps_each_calls_tokens() {
        let path = std::env::temp_dir().join(format!(
            "callimachus_codex_reset_{}.jsonl",
            std::process::id()
        ));
        let tc = |total: [i64; 2], last: [i64; 2]| {
            format!(
                r#"{{"timestamp":"2026-04-25T22:09:45.000Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{},"cached_input_tokens":0,"output_tokens":{}}},"last_token_usage":{{"input_tokens":{},"cached_input_tokens":0,"output_tokens":{}}}}}}}}}"#,
                total[0], total[1], last[0], last[1]
            )
        };
        let say = |text: &str| {
            format!(
                r#"{{"timestamp":"2026-04-25T22:09:43.000Z","type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"{text}"}}]}}}}"#
            )
        };
        let lines = [
            r#"{"timestamp":"2026-04-25T22:09:38.700Z","type":"turn_context","payload":{"cwd":"/proj","model":"gpt-5.3-codex"}}"#.to_string(),
            say("one"),
            tc([1000, 100], [1000, 100]),
            // Restarted at this call, landing above the old total: 3000 in, not 2000.
            say("two"),
            tc([3000, 200], [3000, 200]),
            // Restarted again, now below the old total: the event's own call.
            say("three"),
            tc([500, 40], [500, 40]),
            // Growth after a restart differences as usual.
            say("four"),
            tc([800, 60], [300, 20]),
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        let r = parse_rollout(&path).unwrap();
        let got: Vec<(usize, [i64; 2])> = r
            .usage
            .iter()
            .map(|(i, u)| (*i, [u.input, u.output]))
            .collect();
        assert_eq!(
            got,
            vec![
                (0, [1000, 100]),
                (1, [3000, 200]),
                (2, [500, 40]),
                (3, [300, 20]),
            ]
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Pass 2: a rollout file on disk with NO state DB still gets indexed (covers
    /// the VS Code extension / state-version-skew case), with title derived from
    /// the first user message and project from the session_meta cwd.
    #[test]
    fn indexes_rollout_without_state_db() {
        use std::fs::create_dir_all;
        let root = std::env::temp_dir().join(format!("calli_codex_root_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let day = root.join("sessions/2026/04/25");
        create_dir_all(&day).unwrap();
        let roll =
            day.join("rollout-2026-04-25T15-09-15-019dc6b0-998d-7a31-a39d-1229e6b38072.jsonl");
        let sample = r#"{"timestamp":"2026-04-25T22:09:38.623Z","type":"session_meta","payload":{"id":"019dc6b0-998d-7a31-a39d-1229e6b38072","cwd":"/Users/me/proj","source":"vscode"}}
{"timestamp":"2026-04-25T22:09:39.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"index my codex vscode session"}]}}
{"timestamp":"2026-04-25T22:09:40.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}}
"#;
        std::fs::write(&roll, sample).unwrap();

        let dbp = std::env::temp_dir().join(format!("calli_codex_db_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&dbp);
        let mut conn = crate::db::open(&dbp).unwrap();

        let report = scan_in(&mut conn, &root, &mut || {}).unwrap();
        assert_eq!(report.threads_indexed, 1);
        assert_eq!(report.messages_indexed, 2);

        let tid: i64 = conn
            .query_row("SELECT id FROM threads", [], |r| r.get(0))
            .unwrap();
        let detail = crate::search::thread_detail(&conn, tid).unwrap().unwrap();
        assert!(detail
            .title
            .as_deref()
            .unwrap_or_default()
            .contains("codex vscode"));
        assert_eq!(detail.project_path.as_deref(), Some("/Users/me/proj"));

        // Re-scan is a no-op (rollout fingerprint unchanged).
        let again = scan_in(&mut conn, &root, &mut || {}).unwrap();
        assert_eq!(again.threads_indexed, 0);
    }

    #[test]
    fn caps_tool_output_and_skips_the_apps_own_sessions() {
        let root = std::env::temp_dir().join(format!("calli_codex_cap_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let day = root.join("sessions/2026/05/01");
        std::fs::create_dir_all(&day).unwrap();
        let big = "x".repeat(40_000);
        let user_session = serde_json::json!([
            {"type":"session_meta","payload":{"id":"user-1","cwd":"/Users/me/proj"}},
            {"type":"response_item","payload":{"type":"message","role":"user",
                "content":[{"type":"input_text","text":"run the tests"}]}},
            {"type":"response_item","payload":{"type":"function_call_output","output": big}}
        ]);
        let app_cwd = crate::agent::cli_work_dir_path();
        let app_session = serde_json::json!([
            {"type":"session_meta","payload":{"id":"app-1","cwd": app_cwd.to_string_lossy()}},
            {"type":"response_item","payload":{"type":"message","role":"user",
                "content":[{"type":"input_text","text":"distill this"}]}}
        ]);
        for (name, lines) in [
            ("rollout-a.jsonl", user_session),
            ("rollout-b.jsonl", app_session),
        ] {
            let body: String = lines
                .as_array()
                .unwrap()
                .iter()
                .map(|l| format!("{l}\n"))
                .collect();
            std::fs::write(day.join(name), body).unwrap();
        }

        let dbp = std::env::temp_dir().join(format!("calli_codex_cap_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&dbp);
        let mut conn = crate::db::open(&dbp).unwrap();
        let report = scan_in(&mut conn, &root, &mut || {}).unwrap();
        assert_eq!(
            report.threads_indexed, 1,
            "only the user's session is indexed"
        );
        let tool: String = conn
            .query_row("SELECT text FROM messages WHERE role = 'tool'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(tool.chars().count() <= super::super::claude::TOOL_TEXT_CAP_CHARS);
    }

    /// Pass 2 skips rollouts pass 1 registered before reading their headers: the watcher
    /// re-runs this scan on every Codex write. A registered file whose header id differs
    /// from its state-DB id (so the id check alone would not catch it) is indexed once.
    #[test]
    fn pass_two_skips_rollouts_the_state_db_registered() {
        let root = std::env::temp_dir().join(format!("calli_codex_reg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let day = root.join("sessions/2026/05/02");
        std::fs::create_dir_all(&day).unwrap();
        let rollout = |id: &str, text: &str| {
            format!(
                "{}\n{}\n",
                serde_json::json!({"type":"session_meta","payload":{"id": id,"cwd":"/p"}}),
                serde_json::json!({"type":"response_item","payload":{"type":"message",
                    "role":"user","content":[{"type":"input_text","text": text}]}}),
            )
        };
        let registered = day.join("rollout-registered.jsonl");
        std::fs::write(&registered, rollout("header-id", "registered session")).unwrap();
        std::fs::write(
            day.join("rollout-loose.jsonl"),
            rollout("loose-id", "loose session"),
        )
        .unwrap();
        let state = Connection::open(root.join("state_5.sqlite")).unwrap();
        state
            .execute_batch(
                "CREATE TABLE threads (id TEXT, title TEXT, cwd TEXT, git_branch TEXT,
                    created_at INTEGER, updated_at INTEGER, rollout_path TEXT,
                    first_user_message TEXT)",
            )
            .unwrap();
        state
            .execute(
                "INSERT INTO threads VALUES ('db-id', 'From the DB', '/p', NULL, 1, 2, ?1, NULL)",
                [registered.to_string_lossy()],
            )
            .unwrap();
        drop(state);

        let dbp = std::env::temp_dir().join(format!("calli_codex_reg_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&dbp);
        let mut conn = crate::db::open(&dbp).unwrap();
        let report = scan_in(&mut conn, &root, &mut || {}).unwrap();
        assert_eq!(
            report.threads_indexed, 2,
            "the registered and the loose session"
        );
        let mut ids: Vec<String> = conn
            .prepare("SELECT external_id FROM threads ORDER BY external_id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        ids.sort();
        assert_eq!(
            ids,
            vec!["db-id", "loose-id"],
            "no second copy under the header id"
        );
    }

    /// A state DB with one registered thread whose rollout is `rollout` (a fixture).
    fn write_state_db(path: &Path, id: &str, title: &str, rollout: &Path) {
        let state = Connection::open(path).unwrap();
        state
            .execute_batch(
                "CREATE TABLE threads (id TEXT, title TEXT, cwd TEXT, git_branch TEXT,
                    created_at INTEGER, updated_at INTEGER, rollout_path TEXT,
                    first_user_message TEXT)",
            )
            .unwrap();
        state
            .execute(
                "INSERT INTO threads VALUES (?1, ?2, '/p', NULL, 1, 2, ?3, NULL)",
                rusqlite::params![id, title, rollout.to_string_lossy()],
            )
            .unwrap();
    }

    /// `sqlite_home` in config.toml moves the state DB, and the last written `state_<N>`
    /// wins (the higher N on a tie); a DB that can't be read (an unknown schema) falls back
    /// to the next one.
    #[test]
    fn reads_the_newest_state_db_under_sqlite_home() {
        let root = std::env::temp_dir().join(format!("calli_codex_sqlh_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let day = root.join("sessions/2026/06/01");
        std::fs::create_dir_all(&day).unwrap();
        let rollout = day.join("rollout-x.jsonl");
        std::fs::write(
            &rollout,
            format!(
                "{}\n{}\n",
                serde_json::json!({"type":"session_meta","payload":{"id":"header","cwd":"/p"}}),
                serde_json::json!({"type":"response_item","payload":{"type":"message",
                    "role":"user","content":[{"type":"input_text","text":"hello"}]}}),
            ),
        )
        .unwrap();
        // Stale DBs in the Codex home itself are ignored once sqlite_home points elsewhere.
        write_state_db(&root.join("state_9.sqlite"), "stale", "Stale", &rollout);
        std::fs::write(
            root.join("config.toml"),
            "model = \"o4\"\nsqlite_home = \"db\"\n",
        )
        .unwrap();
        let db_dir = root.join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        write_state_db(
            &db_dir.join("state_5.sqlite"),
            "old",
            "Old schema",
            &rollout,
        );
        write_state_db(
            &db_dir.join("state_12.sqlite"),
            "new",
            "New schema",
            &rollout,
        );
        assert_eq!(sqlite_home(&root), db_dir);
        let set_mtime = |p: &Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(p)
                .unwrap()
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(&db_dir.join("state_5.sqlite"), 1_000);
        set_mtime(&db_dir.join("state_12.sqlite"), 1_000);
        assert_eq!(
            state_dbs(&db_dir),
            vec![
                db_dir.join("state_12.sqlite"),
                db_dir.join("state_5.sqlite")
            ],
            "on a tie, numeric order, not string order"
        );
        // A downgraded Codex writes state_5 beside the newer leftover: state_5 is live.
        set_mtime(&db_dir.join("state_5.sqlite"), 2_000);
        assert_eq!(state_dbs(&db_dir)[0], db_dir.join("state_5.sqlite"));
        // A -wal write counts as a write to its DB.
        std::fs::write(db_dir.join("state_12.sqlite-wal"), b"").unwrap();
        set_mtime(&db_dir.join("state_12.sqlite-wal"), 3_000);
        assert_eq!(state_dbs(&db_dir)[0], db_dir.join("state_12.sqlite"));
        std::fs::remove_file(db_dir.join("state_12.sqlite-wal")).unwrap();
        set_mtime(&db_dir.join("state_12.sqlite"), 4_000);

        let dbp = std::env::temp_dir().join(format!("calli_codex_sqlh_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&dbp);
        let mut conn = crate::db::open(&dbp).unwrap();
        scan_in(&mut conn, &root, &mut || {}).unwrap();
        let title: String = conn
            .query_row(
                "SELECT title FROM threads WHERE external_id = 'new'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(title, "New schema");

        // The newest DB stops reading: the next one serves pass 1.
        std::fs::remove_file(db_dir.join("state_12.sqlite")).unwrap();
        Connection::open(db_dir.join("state_12.sqlite"))
            .unwrap()
            .execute_batch("CREATE TABLE threads_v2 (id TEXT)")
            .unwrap();
        let dbp2 =
            std::env::temp_dir().join(format!("calli_codex_sqlh2_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&dbp2);
        let mut conn = crate::db::open(&dbp2).unwrap();
        scan_in(&mut conn, &root, &mut || {}).unwrap();
        let ids: Vec<String> = conn
            .prepare("SELECT external_id FROM threads")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(ids, vec!["old"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Busy and locked errors (worth waiting for) are told apart from the rest (worth falling
    /// back on), through anyhow context too. A live lock rarely surfaces here: the plain
    /// open waits out rusqlite's busy timeout and then reads `immutable=1`.
    #[test]
    fn busy_errors_are_told_apart() {
        let sqlite = |code: i32| {
            anyhow::Error::from(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                None,
            ))
        };
        assert!(is_busy(&sqlite(rusqlite::ffi::SQLITE_BUSY)));
        assert!(is_busy(
            &sqlite(rusqlite::ffi::SQLITE_LOCKED).context("reading threads")
        ));
        assert!(!is_busy(&sqlite(rusqlite::ffi::SQLITE_ERROR)));
        assert!(!is_busy(&anyhow::anyhow!("no such table: threads")));
    }

    /// Real-data smoke test. Run with `cargo test -- --ignored real_codex_index --nocapture`.
    #[test]
    #[ignore]
    fn real_codex_index() {
        let mut p = std::env::temp_dir();
        p.push(format!("callimachus_codex_real_{}.db", std::process::id()));
        let mut conn = crate::db::open(&p).unwrap();
        let report = scan(&mut conn, &mut || {}).unwrap();
        eprintln!("{report:?}");
    }
}
