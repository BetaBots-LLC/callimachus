//! Qwen Code indexer. Qwen Code is a fork of the Gemini CLI; its chat history
//! lives under `~/.qwen/tmp/<project-hash>/chats/<sessionId>.jsonl` as append-only
//! JSONL. Each line is a `ChatRecord` carrying `type` (`user` / `assistant` /
//! `tool_result` / `system`), an ISO `timestamp`, `cwd` / `gitBranch`, and a
//! `message` Gemini-`Content` object (`{ role, parts: [{text}|{functionCall}|…] }`).
//!
//! Schema verified against QwenLM/qwen-code `chatRecordingService.ts`. Recording
//! can be disabled in Qwen, so an absent dir is not an error.

use super::{set_file_state, source_id, upsert_thread, IndexReport, ParsedMessage, ParsedThread};
use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

pub const KIND: &str = "qwen";

/// `~/.qwen/tmp`, or None if HOME is unset.
pub fn tmp_root() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".qwen").join("tmp"))
}

/// Recursively collect chat `.jsonl` files (inside a `chats/` segment).
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
            return Ok(None);
        }
    }

    let rel = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string();
    let thread = parse_file(path, &rel).with_context(|| format!("parsing {path_str}"))?;
    let n = if let Some(t) = thread {
        upsert_thread(conn, sid, &t)?
    } else {
        0
    };
    set_file_state(conn, &path_str, KIND, mtime, size)?;
    Ok(Some(n))
}

/// Parse a Qwen chat `.jsonl` file. Returns None if it has no messages.
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
            continue;
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

fn ingest_line(thread: &mut ParsedThread, obj: &Value, first_user_text: &mut Option<String>) {
    if let Some(cwd) = obj.get("cwd").and_then(Value::as_str) {
        if !cwd.is_empty() {
            thread.project_path = Some(cwd.to_string());
        }
    }
    if let Some(branch) = obj.get("gitBranch").and_then(Value::as_str) {
        if !branch.is_empty() {
            thread.git_branch = Some(branch.to_string());
        }
    }
    let ts = obj
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_ts);
    if let Some(ts) = ts {
        thread.created_at = Some(thread.created_at.map_or(ts, |c| c.min(ts)));
        thread.updated_at = Some(thread.updated_at.map_or(ts, |u| u.max(ts)));
    }

    let role = match obj.get("type").and_then(Value::as_str) {
        Some("user") => "user",
        Some("assistant") => "assistant",
        Some("tool_result") => "tool",
        _ => return, // system / unknown — skipped
    };

    // Content lives in the nested Gemini `message` object: { role, parts: [...] }.
    let parts = obj.get("message").and_then(|m| m.get("parts"));
    let before = thread.messages.len();
    extract_parts(thread, role, parts, ts);
    if role == "user" && first_user_text.is_none() {
        if let Some(m) = thread.messages.get(before) {
            if m.role == "user" {
                *first_user_text = Some(m.text.clone());
            }
        }
    }
}

/// Flatten a Gemini `parts` array into messages.
fn extract_parts(thread: &mut ParsedThread, role: &str, parts: Option<&Value>, ts: Option<i64>) {
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

    match parts {
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

    const SAMPLE: &str = r#"{"uuid":"u1","parentUuid":null,"sessionId":"sess-q","type":"user","timestamp":"2026-06-01T10:00:00.000Z","cwd":"/Users/me/proj","gitBranch":"main","message":{"role":"user","parts":[{"text":"index qwen threads with sqlite fts5"}]}}
{"uuid":"u2","parentUuid":"u1","sessionId":"sess-q","type":"assistant","timestamp":"2026-06-01T10:00:05.000Z","message":{"role":"model","parts":[{"text":"Sure, using FTS5"},{"functionCall":{"name":"run_shell","args":{"command":"cargo build"}}}]}}
{"uuid":"u3","parentUuid":"u2","sessionId":"sess-q","type":"system","subtype":"chat_compression","timestamp":"2026-06-01T10:00:05.500Z"}
{"uuid":"u4","parentUuid":"u3","sessionId":"sess-q","type":"tool_result","timestamp":"2026-06-01T10:00:06.000Z","message":{"role":"user","parts":[{"functionResponse":{"name":"run_shell","response":{"output":"Finished dev profile"}}}]}}
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
        let path = write_sample("qwen_sample.jsonl");
        let thread = parse_file(&path, "hash/chats/sess-q.jsonl")
            .unwrap()
            .expect("non-empty");
        assert_eq!(thread.project_path.as_deref(), Some("/Users/me/proj"));
        assert_eq!(thread.git_branch.as_deref(), Some("main"));
        // user text, assistant text, assistant functionCall, tool_result = 4 (system skipped)
        assert_eq!(thread.messages.len(), 4);
        assert_eq!(
            thread.title.as_deref(),
            Some("index qwen threads with sqlite fts5")
        );
        let tool = thread
            .messages
            .iter()
            .find(|m| m.tool_name.is_some())
            .unwrap();
        assert_eq!(tool.tool_name.as_deref(), Some("run_shell"));
    }

    #[test]
    fn index_then_search_roundtrip() {
        let path = write_sample("qwen_rt.jsonl");
        let mut conn = crate::db::open(&temp_path("qwen_rt.db")).unwrap();
        let sid = source_id(&conn, KIND).unwrap();
        let thread = parse_file(&path, "hash/chats/rt.jsonl").unwrap().unwrap();
        upsert_thread(&mut conn, sid, &thread).unwrap();
        let hits =
            crate::search::search(&conn, "fts5", &crate::search::SearchFilters::default()).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.source == "qwen"));
    }

    #[test]
    #[ignore]
    fn real_qwen_index() {
        let mut conn = crate::db::open(&temp_path("qwen_real.db")).unwrap();
        eprintln!("{:?}", scan(&mut conn, &mut || {}).unwrap());
    }
}
