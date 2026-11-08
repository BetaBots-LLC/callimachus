//! Gemini CLI indexer. Chat history lives under `~/.gemini/tmp/<project-id>/chats/`
//! as append-only JSONL. The first line of each file is a session metadata record
//! (`sessionId`, `startTime`, `lastUpdated`, `directories`); subsequent lines are
//! message records typed `user` / `gemini` (= assistant), each carrying a
//! `content` PartListUnion (a plain string or an array of `{text}` / `{functionCall}`
//! / `{functionResponse}` parts). We index user/assistant text and tool calls, plus each
//! `gemini` record's `model` and `tokens` (the response's usage metadata) for spend.
//!
//! The recorder appends a message again, whole and under the same `id`, when it changes:
//! the tokens of a response arrive after its text (`recordMessageTokens`), and tool calls
//! are added to the last message (`recordToolCalls`). The latest copy of a record wins.
//!
//! Schema verified against google-gemini/gemini-cli `chatRecordingService.ts`.

use super::common::{parse_rfc3339, push_message, truncate_title};
use super::{
    file_change_state, set_file_state, source_id, upsert_thread, IndexReport, MsgUsage,
    ParsedMessage, ParsedThread,
};
use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

pub const KIND: &str = "gemini";

/// `~/.gemini/tmp` (per-project chat history is partitioned beneath it), or None
/// if HOME is unset.
pub fn tmp_root() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".gemini").join("tmp"))
}

/// Walk every project tmp dir, (re)indexing chat files whose mtime/size changed.
pub fn scan(conn: &mut Connection, tick: &mut dyn FnMut()) -> Result<IndexReport> {
    match tmp_root() {
        Some(root) => scan_chats(conn, KIND, &root, parse_file, tick),
        None => Ok(IndexReport::default()),
    }
}

// ---------------------------------------------------------------------------------------
// Shared with Qwen Code, a Gemini CLI fork that keeps the same `tmp/<project>/chats/`
// layout and Gemini `parts`, with its own record shape.
// ---------------------------------------------------------------------------------------

/// Parses one chat file into a thread keyed by the given external id.
pub(super) type ParseChat = fn(&Path, &str) -> Result<Option<ParsedThread>>;

/// Index every chat `.jsonl` under `root` (a `<cli home>/tmp` dir) into source `kind`,
/// skipping files whose (mtime, size) is unchanged.
pub(super) fn scan_chats(
    conn: &mut Connection,
    kind: &str,
    root: &Path,
    parse: ParseChat,
    tick: &mut dyn FnMut(),
) -> Result<IndexReport> {
    let mut report = IndexReport::default();
    if !root.is_dir() {
        return Ok(report);
    }
    let sid = source_id(conn, kind)?;

    let mut files = Vec::new();
    collect_chat_jsonl(root, root, &mut files);
    for path in files {
        tick();
        report.record_at(&path, index_chat(conn, sid, kind, root, &path, parse));
    }
    Ok(report)
}

/// Recursively collect every chat `.jsonl` under `dir`. Restricted to files inside a
/// `chats` directory below `root`, so the sibling `logs/` and `checkpoints/` dirs are
/// skipped. Compared by path component, so it holds for Windows separators too.
fn collect_chat_jsonl(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_chat_jsonl(root, &path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl")
            && in_chats_dir(root, &path)
        {
            out.push(path);
        }
    }
}

fn in_chats_dir(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root)
        .ok()
        .and_then(Path::parent)
        .is_some_and(|dir| dir.components().any(|c| c.as_os_str() == "chats"))
}

/// Index one chat file. Returns Some(message_count) if indexed, None if unchanged.
fn index_chat(
    conn: &mut Connection,
    sid: i64,
    kind: &str,
    root: &Path,
    path: &Path,
    parse: ParseChat,
) -> Result<Option<usize>> {
    let (unchanged, mtime, size) = file_change_state(conn, path)?;
    if unchanged {
        return Ok(None);
    }

    // external_id = path relative to the tmp root: stable and unique per file, joined with
    // '/' on every OS (the `chats/` segment check below splits on '/').
    let rel = super::claude::external_id_for(root, path);
    let path_str = path.to_string_lossy();
    let mut thread = parse(path, &rel).with_context(|| format!("parsing {path_str}"))?;
    if let Some(t) = thread.as_mut() {
        // Top-level sessions are `<id>/chats/session-*.jsonl` (one segment after
        // `chats/`); subagent transcripts nest one level deeper.
        t.is_subagent = segments_after_chats(&rel) >= 2;
    }
    let n = match thread {
        Some(t) => upsert_thread(conn, sid, &t)?,
        None => 0,
    };
    set_file_state(conn, &path_str, kind, mtime, size)?;
    Ok(Some(n))
}

/// How many path segments follow the `chats/` segment (1 = top-level session file).
fn segments_after_chats(rel: &str) -> usize {
    match rel.split('/').position(|s| s == "chats") {
        Some(i) => rel.split('/').count().saturating_sub(i + 1),
        None => 0,
    }
}

/// Folds one parsed JSONL record into the thread under construction.
pub(super) type IngestLine = fn(&mut ParsedThread, &Value, &mut ChatState);

/// What a chat parse carries from one record to the next.
#[derive(Default)]
pub(super) struct ChatState {
    /// The first user message's text (the title fallback).
    pub first_user_text: Option<String>,
    /// The last message record, so a re-recorded copy of it replaces the first copy instead
    /// of repeating it.
    last_record: Option<RecordStart>,
}

/// Where a message record's additions to the thread start.
struct RecordStart {
    id: String,
    messages: usize,
    usage: usize,
    /// The last usage entry as it stood before the record: a record with no message of its
    /// own (a tool-call-only turn) adds its tokens into that entry rather than pushing one,
    /// so truncating `usage` alone would keep the first copy's tokens and every copy would
    /// add them again (3 copies of a 2,000-token turn stored 6,000).
    last_usage: Option<MsgUsage>,
}

impl ChatState {
    /// Start a message record with this `id` (None: a record without one). A copy of the
    /// previous record (same id) first drops what that record added, tokens included.
    pub(super) fn begin_record(&mut self, thread: &mut ParsedThread, id: Option<&str>) {
        if let (Some(id), Some(last)) = (id, &self.last_record) {
            if last.id == id {
                thread.messages.truncate(last.messages);
                thread.usage.truncate(last.usage);
                if let (Some(entry), Some(was)) = (thread.usage.last_mut(), &last.last_usage) {
                    entry.1 = was.clone();
                }
                return;
            }
        }
        self.last_record = id.map(|id| RecordStart {
            id: id.to_string(),
            messages: thread.messages.len(),
            usage: thread.usage.len(),
            last_usage: thread.usage.last().map(|(_, u)| u.clone()),
        });
    }
}

/// A Gemini `GenerateContentResponseUsageMetadata` as usage. The prompt count includes the
/// cached tokens; thinking is billed as output and tool-use prompts as input.
pub(super) fn gemini_usage(
    model: &str,
    prompt: i64,
    cached: i64,
    tool: i64,
    output: i64,
    thoughts: i64,
) -> MsgUsage {
    MsgUsage {
        model: model.to_string(),
        input: (prompt - cached).max(0) + tool.max(0),
        output: output.max(0) + thoughts.max(0),
        cache_write: 0,
        cache_read: cached.max(0),
    }
}

/// Parse a chat `.jsonl` file line by line with `ingest`, tolerating malformed lines.
/// Returns None if it yields no messages.
pub(super) fn parse_chat_jsonl(
    path: &Path,
    external_id: &str,
    ingest: IngestLine,
) -> Result<Option<ParsedThread>> {
    let content = fs::read_to_string(path)?;
    let mut thread = ParsedThread {
        external_id: external_id.to_string(),
        ..Default::default()
    };
    let mut state = ChatState::default();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(line) else {
            continue; // tolerate the occasional malformed line
        };
        ingest(&mut thread, &obj, &mut state);
    }
    if thread.messages.is_empty() {
        return Ok(None);
    }
    if thread.title.is_none() {
        thread.title = state.first_user_text.as_deref().map(truncate_title);
    }
    Ok(Some(thread))
}

/// Flatten one record's Gemini parts (a string or an array of `{text}` /
/// `{functionCall}` / `{functionResponse}`) into the thread, keeping the record's first
/// message as the title fallback when it is the first user text.
pub(super) fn ingest_parts(
    thread: &mut ParsedThread,
    role: &str,
    parts: Option<&Value>,
    ts: Option<i64>,
    first_user_text: &mut Option<String>,
) {
    let before = thread.messages.len();
    extract_parts(&mut thread.messages, role, parts, ts);
    if role == "user" && first_user_text.is_none() {
        if let Some(m) = thread.messages.get(before).filter(|m| m.role == "user") {
            *first_user_text = Some(m.text.clone());
        }
    }
}

fn extract_parts(out: &mut Vec<ParsedMessage>, role: &str, parts: Option<&Value>, ts: Option<i64>) {
    match parts {
        Some(Value::String(s)) => push_message(out, role, s, None, ts),
        Some(Value::Array(parts)) => {
            for part in parts {
                if let Some(t) = part.get("text").and_then(Value::as_str) {
                    push_message(out, role, t, None, ts);
                } else if let Some(call) = part.get("functionCall") {
                    let name = call
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("tool")
                        .to_string();
                    let args = call.get("args").map(|v| v.to_string()).unwrap_or_default();
                    let text = format!("{name}: {args}");
                    push_message(out, "assistant", &text, Some(name), ts);
                } else if let Some(resp) = part.get("functionResponse") {
                    let text = resp
                        .get("response")
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| resp.to_string());
                    // Same head + tail cap as the Claude indexer (tool output is never
                    // embedded or distilled, and dominated the index size).
                    let text = super::claude::cap_tool_text(text.trim());
                    push_message(out, "tool", &text, None, ts);
                }
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------------------
// Gemini's record shape
// ---------------------------------------------------------------------------------------

/// Parse a Gemini chat `.jsonl` file. `external_id` keys the thread. Returns None
/// if it has no user/assistant messages.
pub fn parse_file(path: &Path, external_id: &str) -> Result<Option<ParsedThread>> {
    parse_chat_jsonl(path, external_id, ingest_line)
}

/// Fold one JSONL line into the thread under construction. Handles both the
/// session-metadata line and the message records.
fn ingest_line(thread: &mut ParsedThread, obj: &Value, state: &mut ChatState) {
    // Session metadata (first line): pick up the project dir from `directories`.
    if let Some(dirs) = obj.get("directories").and_then(Value::as_array) {
        if let Some(dir) = dirs
            .iter()
            .filter_map(Value::as_str)
            .find(|s| !s.is_empty())
        {
            thread.project_path = Some(dir.to_string());
        }
    }
    // Metadata carries startTime/lastUpdated; messages carry timestamp. Fold all
    // ISO-8601 stamps into the thread's created/updated bounds.
    for key in ["startTime", "lastUpdated", "timestamp"] {
        if let Some(ts) = obj.get(key).and_then(Value::as_str).and_then(parse_rfc3339) {
            thread.created_at = Some(thread.created_at.map_or(ts, |c| c.min(ts)));
            thread.updated_at = Some(thread.updated_at.map_or(ts, |u| u.max(ts)));
        }
    }

    let ts = obj
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339);
    let role = match obj.get("type").and_then(Value::as_str) {
        Some("user") => "user",
        Some("gemini") => "assistant",
        _ => return, // info / error / warning / tool records — skipped for now
    };
    state.begin_record(thread, obj.get("id").and_then(Value::as_str));
    let before = thread.messages.len();
    ingest_parts(
        thread,
        role,
        obj.get("content"),
        ts,
        &mut state.first_user_text,
    );
    // `tokens` is `{input, output, cached, thoughts, tool, total}`, the response's usage
    // metadata as `recordMessageTokens` stores it (chatRecordingService.ts, checked
    // 2026-11-07), on the `gemini` record beside its `model`.
    if let (Some(model), Some(t)) = (
        obj.get("model").and_then(Value::as_str),
        obj.get("tokens").filter(|t| t.is_object()),
    ) {
        let n = |k: &str| t.get(k).and_then(Value::as_i64).unwrap_or(0);
        thread.attach_usage(
            Some(before),
            gemini_usage(
                model,
                n("input"),
                n("cached"),
                n("tool"),
                n("output"),
                n("thoughts"),
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_path(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("callimachus_{name}_{}_{n}", std::process::id()))
    }

    const SAMPLE: &str = r#"{"sessionId":"sess-abc","projectHash":"deadbeef","startTime":"2026-06-01T10:00:00.000Z","lastUpdated":"2026-06-01T10:00:06.000Z","directories":["/Users/me/proj"]}
{"id":"m1","type":"user","timestamp":"2026-06-01T10:00:00.000Z","content":"index gemini threads with sqlite fts5"}
{"id":"m2","type":"gemini","timestamp":"2026-06-01T10:00:05.000Z","content":[{"text":"Sure, using FTS5"},{"functionCall":{"name":"run_shell_command","args":{"command":"cargo build"}}}]}
{"id":"m3","type":"info","timestamp":"2026-06-01T10:00:05.500Z","content":"context compressed"}
{"id":"m4","type":"user","timestamp":"2026-06-01T10:00:06.000Z","content":[{"functionResponse":{"name":"run_shell_command","response":{"output":"Finished dev profile"}}}]}
"#;

    fn write_sample(name: &str) -> PathBuf {
        let path = temp_path(name);
        std::fs::File::create(&path)
            .unwrap()
            .write_all(SAMPLE.as_bytes())
            .unwrap();
        path
    }

    #[test]
    fn parses_sample_thread() {
        let path = write_sample("gem_sample.jsonl");
        let thread = parse_file(&path, "abc/chats/session-x.jsonl")
            .unwrap()
            .expect("non-empty");
        assert_eq!(thread.external_id, "abc/chats/session-x.jsonl");
        // directories -> project_path
        assert_eq!(thread.project_path.as_deref(), Some("/Users/me/proj"));
        // metadata + message stamps fold into bounds
        assert!(thread.created_at.is_some() && thread.updated_at.is_some());
        // user text, gemini text, gemini functionCall, functionResponse = 4 (info skipped)
        assert_eq!(thread.messages.len(), 4);
        assert_eq!(
            thread.title.as_deref(),
            Some("index gemini threads with sqlite fts5")
        );
        let tool = thread
            .messages
            .iter()
            .find(|m| m.tool_name.is_some())
            .unwrap();
        assert_eq!(tool.tool_name.as_deref(), Some("run_shell_command"));
        assert!(tool.text.contains("cargo build"));
    }

    /// A turn with no text (tool calls only) puts its tokens on the message before it; when
    /// the recorder writes that turn three times (recorded, tool calls scheduled, tool calls
    /// done) its tokens still count once.
    #[test]
    fn a_re_recorded_turn_without_text_counts_its_tokens_once() {
        let path = temp_path("gem_empty_turn.jsonl");
        let turn = r#"{"id":"m3","type":"gemini","timestamp":"2026-06-01T10:00:06.000Z","content":"","model":"gemini-2.5-pro","tokens":{"input":2000,"output":0,"cached":0,"thoughts":0,"tool":0,"total":2000}}"#;
        std::fs::write(
            &path,
            format!(
                r#"{{"sessionId":"s","startTime":"2026-06-01T10:00:00.000Z","directories":["/p"]}}
{{"id":"m1","type":"user","timestamp":"2026-06-01T10:00:00.000Z","content":"hello"}}
{{"id":"m2","type":"gemini","timestamp":"2026-06-01T10:00:05.000Z","content":"hi there","model":"gemini-2.5-pro","tokens":{{"input":1000,"output":0,"cached":0,"thoughts":0,"tool":0,"total":1000}}}}
{turn}
{turn}
{turn}
"#
            ),
        )
        .unwrap();
        let t = parse_file(&path, "p/chats/s.jsonl").unwrap().unwrap();
        assert_eq!(t.messages.len(), 2);
        let inputs: Vec<(usize, i64)> = t.usage.iter().map(|(i, u)| (*i, u.input)).collect();
        assert_eq!(inputs, vec![(1, 3000)], "m2's 1000 plus m3's 2000, once");
        let _ = std::fs::remove_file(&path);
    }

    /// The recorder appends a message again once its tokens arrive: the copy replaces the
    /// first one, and its usage (prompt minus cached, thinking as output) lands on it.
    #[test]
    fn a_re_recorded_message_carries_its_tokens_once() {
        let path = temp_path("gem_tokens.jsonl");
        std::fs::write(
            &path,
            r#"{"sessionId":"s","startTime":"2026-06-01T10:00:00.000Z","directories":["/p"]}
{"id":"m1","type":"user","timestamp":"2026-06-01T10:00:00.000Z","content":"hello"}
{"id":"m2","type":"gemini","timestamp":"2026-06-01T10:00:05.000Z","content":"hi there","model":"gemini-2.5-pro"}
{"id":"m2","type":"gemini","timestamp":"2026-06-01T10:00:05.000Z","content":"hi there","model":"gemini-2.5-pro","tokens":{"input":1200,"output":40,"cached":1000,"thoughts":10,"tool":0,"total":1250}}
{"$set":{"lastUpdated":"2026-06-01T10:00:06.000Z"}}
"#,
        )
        .unwrap();
        let t = parse_file(&path, "p/chats/s.jsonl").unwrap().unwrap();
        let texts: Vec<&str> = t.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, vec!["hello", "hi there"]);
        assert_eq!(t.usage.len(), 1);
        let (i, u) = &t.usage[0];
        assert_eq!(*i, 1);
        assert_eq!(u.model, "gemini-2.5-pro");
        assert_eq!(
            [u.input, u.output, u.cache_write, u.cache_read],
            [200, 50, 0, 1000]
        );
    }

    #[test]
    fn subagent_detection_by_depth() {
        assert_eq!(segments_after_chats("id/chats/session-x.jsonl"), 1);
        assert_eq!(
            segments_after_chats("id/chats/parent-sess/child-sess.jsonl"),
            2
        );
    }

    #[test]
    fn index_then_search_roundtrip() {
        let path = write_sample("gem_rt.jsonl");
        let mut conn = crate::db::open(&temp_path("gem_rt.db")).unwrap();
        let sid = source_id(&conn, KIND).unwrap();
        let thread = parse_file(&path, "abc/chats/rt.jsonl").unwrap().unwrap();
        upsert_thread(&mut conn, sid, &thread).unwrap();

        let hits =
            crate::search::search(&conn, "fts5", &crate::search::SearchFilters::default()).unwrap();
        assert_eq!(hits.len(), 2); // user message + "using FTS5"
        assert!(hits.iter().all(|h| h.source == "gemini"));
    }

    /// The shared walk keeps only files under a `chats` dir, keys them by their '/'-joined
    /// path below the root, and marks deeper (subagent) transcripts.
    #[test]
    fn scan_chats_walks_only_chat_dirs() {
        let root = temp_path("gem_root");
        for dir in ["p1/chats/parent", "p1/logs", "p1/checkpoints"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        for file in [
            "p1/chats/session-a.jsonl",
            "p1/chats/parent/child.jsonl",
            "p1/logs/log.jsonl",
            "p1/checkpoints/cp.jsonl",
        ] {
            std::fs::write(root.join(file), SAMPLE).unwrap();
        }
        let mut conn = crate::db::open(&temp_path("gem_walk.db")).unwrap();
        let report = scan_chats(&mut conn, KIND, &root, parse_file, &mut || {}).unwrap();
        assert_eq!(
            report.threads_indexed, 2,
            "logs/ and checkpoints/ are skipped"
        );
        let rows: Vec<(String, bool)> = conn
            .prepare("SELECT external_id, is_subagent FROM threads ORDER BY external_id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                ("p1/chats/parent/child.jsonl".to_string(), true),
                ("p1/chats/session-a.jsonl".to_string(), false),
            ]
        );
        let again = scan_chats(&mut conn, KIND, &root, parse_file, &mut || {}).unwrap();
        assert_eq!(again.threads_skipped, 2, "unchanged files are skipped");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Real-data smoke test against live ~/.gemini history. Ignored by default;
    /// run with: `cargo test -- --ignored real_gemini_index --nocapture`
    #[test]
    #[ignore]
    fn real_gemini_index() {
        let mut conn = crate::db::open(&temp_path("gem_real.db")).unwrap();
        let report = scan(&mut conn, &mut || {}).unwrap();
        eprintln!("{report:?}");
    }
}
