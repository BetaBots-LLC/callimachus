//! Gemini CLI indexer. Chat history lives under `~/.gemini/tmp/<project-id>/chats/`
//! as append-only JSONL. The first line of each file is a session metadata record
//! (`sessionId`, `startTime`, `lastUpdated`, `directories`); subsequent lines are
//! message records typed `user` / `gemini` (= assistant), each carrying a
//! `content` PartListUnion (a plain string or an array of `{text}` / `{functionCall}`
//! / `{functionResponse}` parts). We index user/assistant text and tool calls.
//!
//! Schema verified against google-gemini/gemini-cli `chatRecordingService.ts`.

use super::{set_file_state, source_id, upsert_thread, IndexReport, ParsedMessage, ParsedThread};
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

/// Recursively collect every chat `.jsonl` under `dir`. Restricted to files inside
/// a `chats/` segment so we skip the sibling `logs/` and `checkpoints/` dirs.
fn collect_chat_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_chat_jsonl(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl")
            && path.to_string_lossy().contains("/chats/")
        {
            out.push(path);
        }
    }
}

/// Walk every project tmp dir, (re)indexing chat files whose mtime/size changed.
pub fn scan(conn: &mut Connection, tick: &mut dyn FnMut()) -> Result<IndexReport> {
    let mut report = IndexReport::default();
    let Some(root) = tmp_root() else {
        return Ok(report);
    };
    if !root.is_dir() {
        return Ok(report);
    }
    let sid = source_id(conn, KIND)?;

    let mut files = Vec::new();
    collect_chat_jsonl(&root, &mut files);

    for path in files {
        tick();
        match index_file(conn, sid, &root, &path) {
            Ok(Some(n)) => {
                report.threads_indexed += 1;
                report.messages_indexed += n;
            }
            Ok(None) => report.threads_skipped += 1,
            Err(_) => report.errors += 1,
        }
    }
    Ok(report)
}

/// Index one chat file. Returns Some(message_count) if indexed, None if unchanged.
fn index_file(conn: &mut Connection, sid: i64, root: &Path, path: &Path) -> Result<Option<usize>> {
    let path_str = path.to_string_lossy().to_string();
    let meta = fs::metadata(path)?;
    let size = meta.len() as i64;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    if let Some((prev_mtime, prev_size)) = super::file_state(conn, &path_str)? {
        if prev_mtime == mtime && prev_size == size {
            return Ok(None); // unchanged
        }
    }

    // external_id = path relative to the tmp root: stable and unique per file.
    let rel = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string();

    let mut thread = parse_file(path, &rel).with_context(|| format!("parsing {path_str}"))?;
    if let Some(t) = thread.as_mut() {
        // Top-level sessions are `<id>/chats/session-*.jsonl` (one segment after
        // `chats/`); subagent transcripts nest one level deeper.
        t.is_subagent = segments_after_chats(&rel) >= 2;
    }
    let n = if let Some(t) = thread {
        upsert_thread(conn, sid, &t)?
    } else {
        0
    };
    set_file_state(conn, &path_str, KIND, mtime, size)?;
    Ok(Some(n))
}

/// How many path segments follow the `chats/` segment (1 = top-level session file).
fn segments_after_chats(rel: &str) -> usize {
    match rel.split('/').position(|s| s == "chats") {
        Some(i) => rel.split('/').count().saturating_sub(i + 1),
        None => 0,
    }
}

/// Parse a Gemini chat `.jsonl` file. `external_id` keys the thread. Returns None
/// if it has no user/assistant messages.
pub fn parse_file(path: &Path, external_id: &str) -> Result<Option<ParsedThread>> {
    let content = fs::read_to_string(path)?;
    let mut thread = ParsedThread {
        external_id: external_id.to_string(),
        ..Default::default()
    };
    let mut first_user_text: Option<String> = None;

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(line) else {
            continue; // tolerate the occasional malformed line
        };
        ingest_line(&mut thread, &obj, &mut first_user_text);
    }

    if thread.messages.is_empty() {
        return Ok(None);
    }
    if thread.title.is_none() {
        thread.title = first_user_text.map(|t| {
            let t = t.trim();
            if t.chars().count() > 80 {
                format!("{}…", t.chars().take(80).collect::<String>())
            } else {
                t.to_string()
            }
        });
    }
    Ok(Some(thread))
}

/// Fold one JSONL line into the thread under construction. Handles both the
/// session-metadata line and the message records.
fn ingest_line(thread: &mut ParsedThread, obj: &Value, first_user_text: &mut Option<String>) {
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
        if let Some(ts) = obj.get(key).and_then(Value::as_str).and_then(parse_ts) {
            thread.created_at = Some(thread.created_at.map_or(ts, |c| c.min(ts)));
            thread.updated_at = Some(thread.updated_at.map_or(ts, |u| u.max(ts)));
        }
    }

    let ts = obj
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_ts);
    let role = match obj.get("type").and_then(Value::as_str) {
        Some("user") => "user",
        Some("gemini") => "assistant",
        _ => return, // info / error / warning / tool records — skipped for now
    };

    let before = thread.messages.len();
    extract_content(thread, role, obj.get("content"), ts);
    if role == "user" && first_user_text.is_none() {
        if let Some(m) = thread.messages.get(before) {
            if m.role == "user" {
                *first_user_text = Some(m.text.clone());
            }
        }
    }
}

/// Turn a record's `content` (a string or an array of Gemini parts) into messages.
fn extract_content(
    thread: &mut ParsedThread,
    role: &str,
    content: Option<&Value>,
    ts: Option<i64>,
) {
    let push = |thread: &mut ParsedThread, role: &str, text: String, tool: Option<String>| {
        let text = text.trim().to_string();
        if !text.is_empty() {
            thread.messages.push(ParsedMessage {
                role: role.to_string(),
                text,
                tool_name: tool,
                ts,
            });
        }
    };

    match content {
        Some(Value::String(s)) => push(thread, role, s.clone(), None),
        Some(Value::Array(parts)) => {
            for part in parts {
                if let Some(t) = part.get("text").and_then(Value::as_str) {
                    push(thread, role, t.to_string(), None);
                } else if let Some(call) = part.get("functionCall") {
                    let name = call
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("tool")
                        .to_string();
                    let args = call.get("args").map(|v| v.to_string()).unwrap_or_default();
                    push(thread, "assistant", format!("{name}: {args}"), Some(name));
                } else if let Some(resp) = part.get("functionResponse") {
                    let text = resp
                        .get("response")
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| resp.to_string());
                    push(thread, "tool", text, None);
                }
            }
        }
        _ => {}
    }
}

/// ISO-8601 / RFC-3339 timestamp -> epoch seconds.
fn parse_ts(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.timestamp())
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
