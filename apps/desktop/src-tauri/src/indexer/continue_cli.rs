//! Continue CLI (`cn`) indexer. Sessions are one JSON file each under
//! `~/.continue/sessions/<sessionId>.json`:
//!   { sessionId, title, workspaceDirectory, history: [{ message: { role, content } }] }
//! `content` is a string or an array of parts (`{type:"text", text}`). System
//! messages are already filtered out by Continue. There are no per-message
//! timestamps, so we use the file's mtime for the thread's time bounds.
//!
//! Schema verified against continuedev/continue `extensions/cli/src/session.ts`.

use super::{set_file_state, source_id, upsert_thread, IndexReport, ParsedMessage, ParsedThread};
use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

pub const KIND: &str = "continue";

/// `~/.continue/sessions`, or None if HOME is unset.
pub fn sessions_root() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".continue").join("sessions"))
}

pub fn scan(conn: &mut Connection, tick: &mut dyn FnMut()) -> Result<IndexReport> {
    let mut report = IndexReport::default();
    let Some(root) = sessions_root() else {
        return Ok(report);
    };
    if !root.is_dir() {
        return Ok(report);
    }
    let sid = source_id(conn, KIND)?;

    let Ok(entries) = fs::read_dir(&root) else {
        return Ok(report);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        tick();
        match index_file(conn, sid, &path) {
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

fn index_file(conn: &mut Connection, sid: i64, path: &Path) -> Result<Option<usize>> {
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

    let fallback_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("session");
    let thread =
        parse_file(path, fallback_id, mtime).with_context(|| format!("parsing {path_str}"))?;
    let n = if let Some(t) = thread {
        upsert_thread(conn, sid, &t)?
    } else {
        0
    };
    set_file_state(conn, &path_str, KIND, mtime, size)?;
    Ok(Some(n))
}

/// Parse a Continue session JSON. `mtime` seeds the time bounds (no per-message ts).
pub fn parse_file(path: &Path, fallback_id: &str, mtime: i64) -> Result<Option<ParsedThread>> {
    let root: Value = serde_json::from_str(&fs::read_to_string(path)?)?;

    let external_id = root
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or(fallback_id)
        .to_string();
    let ts = if mtime > 0 { Some(mtime) } else { None };

    let mut messages: Vec<ParsedMessage> = Vec::new();
    let mut first_user: Option<String> = None;
    if let Some(history) = root.get("history").and_then(Value::as_array) {
        for item in history {
            let msg = item.get("message").unwrap_or(item);
            let role = match msg.get("role").and_then(Value::as_str) {
                Some("assistant") => "assistant",
                Some("user") => "user",
                Some("tool") => "tool",
                _ => continue, // system already filtered by Continue
            };
            let text = content_text(msg.get("content"));
            let text = text.trim().to_string();
            if text.is_empty() {
                continue;
            }
            if role == "user" && first_user.is_none() {
                first_user = Some(text.clone());
            }
            messages.push(ParsedMessage {
                role: role.to_string(),
                text,
                tool_name: None,
                ts,
            });
        }
    }

    if messages.is_empty() {
        return Ok(None);
    }
    let title = root
        .get("title")
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
        .map(str::to_string)
        .or_else(|| first_user.map(truncate_title));
    let project_path = root
        .get("workspaceDirectory")
        .and_then(Value::as_str)
        .filter(|d| !d.is_empty())
        .map(str::to_string);

    Ok(Some(ParsedThread {
        external_id,
        title,
        project_path,
        git_branch: None,
        created_at: ts,
        updated_at: ts,
        is_subagent: false,
        usage: Vec::new(),
        messages,
    }))
}

/// `content` is a plain string or an array of `{type:"text", text}` parts.
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn truncate_title(s: String) -> String {
    let s = s.trim();
    if s.chars().count() > 80 {
        format!("{}…", s.chars().take(80).collect::<String>())
    } else {
        s.to_string()
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

    const SAMPLE: &str = r#"{"sessionId":"sess-cn","title":"FTS5 work","workspaceDirectory":"/Users/me/proj","history":[
        {"message":{"role":"system","content":"you are helpful"}},
        {"message":{"role":"user","content":"index continue threads with sqlite fts5"}},
        {"message":{"role":"assistant","content":[{"type":"text","text":"Sure, using FTS5"}]}}
    ]}"#;

    #[test]
    fn parses_sample_thread() {
        let path = temp_path("cn_sample.json");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(SAMPLE.as_bytes())
            .unwrap();
        let thread = parse_file(&path, "fallback", 1780000000)
            .unwrap()
            .expect("non-empty");
        assert_eq!(thread.external_id, "sess-cn");
        assert_eq!(thread.title.as_deref(), Some("FTS5 work"));
        assert_eq!(thread.project_path.as_deref(), Some("/Users/me/proj"));
        // system filtered -> user + assistant = 2
        assert_eq!(thread.messages.len(), 2);
        assert_eq!(thread.created_at, Some(1780000000));
    }

    #[test]
    fn index_then_search_roundtrip() {
        let path = temp_path("cn_rt.json");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(SAMPLE.as_bytes())
            .unwrap();
        let mut conn = crate::db::open(&temp_path("cn_rt.db")).unwrap();
        let sid = source_id(&conn, KIND).unwrap();
        let thread = parse_file(&path, "fallback", 1780000000).unwrap().unwrap();
        upsert_thread(&mut conn, sid, &thread).unwrap();
        let hits =
            crate::search::search(&conn, "fts5", &crate::search::SearchFilters::default()).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.source == "continue"));
    }

    #[test]
    #[ignore]
    fn real_continue_index() {
        let mut conn = crate::db::open(&temp_path("cn_real.db")).unwrap();
        eprintln!("{:?}", scan(&mut conn, &mut || {}).unwrap());
    }
}
