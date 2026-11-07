//! Qwen Code indexer. Qwen Code is a fork of the Gemini CLI; its chat history
//! lives under `~/.qwen/tmp/<project-hash>/chats/<sessionId>.jsonl` as append-only
//! JSONL. Each line is a `ChatRecord` carrying `type` (`user` / `assistant` /
//! `tool_result` / `system`), an ISO `timestamp`, `cwd` / `gitBranch`, and a
//! `message` Gemini-`Content` object (`{ role, parts: [{text}|{functionCall}|…] }`).
//!
//! Schema verified against QwenLM/qwen-code `chatRecordingService.ts`. Recording
//! can be disabled in Qwen, so an absent dir is not an error.

use super::common::parse_rfc3339;
use super::gemini::{ingest_parts, parse_chat_jsonl, scan_chats};
use super::{IndexReport, ParsedThread};
use anyhow::Result;
use rusqlite::Connection;
use serde_json::Value;
use std::path::{Path, PathBuf};

pub const KIND: &str = "qwen";

/// `~/.qwen/tmp`, or None if HOME is unset.
pub fn tmp_root() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".qwen").join("tmp"))
}

/// Walk every project tmp dir, (re)indexing chat files whose mtime/size changed. The
/// walk, change detection and external ids (the path below the tmp root, '/'-joined) are
/// Gemini's: Qwen Code keeps the Gemini CLI's layout.
pub fn scan(conn: &mut Connection, tick: &mut dyn FnMut()) -> Result<IndexReport> {
    match tmp_root() {
        Some(root) => scan_chats(conn, KIND, &root, parse_file, tick),
        None => Ok(IndexReport::default()),
    }
}

/// Parse a Qwen chat `.jsonl` file. Returns None if it has no messages.
pub fn parse_file(path: &Path, external_id: &str) -> Result<Option<ParsedThread>> {
    parse_chat_jsonl(path, external_id, ingest_line)
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
        .and_then(parse_rfc3339);
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
    ingest_parts(thread, role, parts, ts, first_user_text);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::{source_id, upsert_thread};
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
