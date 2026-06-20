//! Cline indexer — and the shared engine for Cline-architecture VS Code agents
//! (Roo Code, Kilo Code), which are forks storing tasks the same way. All are
//! editor extensions (no CLI), so this is index-only. Each task lives under a
//! per-editor globalStorage dir, keyed by the extension id:
//!   <editor>/User/globalStorage/<ext-id>/tasks/<taskId>/
//!     api_conversation_history.json   -> Anthropic Messages array (the transcript)
//!     task_metadata.json              -> metadata (cwd if present)
//! `<taskId>` is a unix-millisecond timestamp. The transcript uses the standard
//! Anthropic content-block shape (text / tool_use / tool_result), so we parse it
//! the same way as Claude Code. Roo/Kilo reuse `scan_ext` with their own ext id.
//!
//! Path verified against cline/cline task-history docs.

use super::{set_file_state, source_id, upsert_thread, IndexReport, ParsedMessage, ParsedThread};
use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

pub const KIND: &str = "cline";

/// Cline's extension id under each editor's globalStorage.
pub const EXT_ID: &str = "saoudrizwan.claude-dev";

/// Editors that may host a Cline-architecture extension ("Application Support" names).
const EDITORS: &[&str] = &["Code", "Code - Insiders", "Cursor", "VSCodium", "Windsurf"];

/// All existing `…/<editor>/User/globalStorage/<ext_id>/tasks` dirs.
pub fn task_roots_for(ext_id: &str) -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let support = PathBuf::from(home).join("Library/Application Support");
    EDITORS
        .iter()
        .map(|ed| {
            support
                .join(ed)
                .join("User/globalStorage")
                .join(ext_id)
                .join("tasks")
        })
        .filter(|p| p.is_dir())
        .collect()
}

/// Cline's own task roots (used by the watcher).
pub fn task_roots() -> Vec<PathBuf> {
    task_roots_for(EXT_ID)
}

pub fn scan(conn: &mut Connection, tick: &mut dyn FnMut()) -> Result<IndexReport> {
    scan_ext(conn, KIND, EXT_ID, tick)
}

/// Index a Cline-architecture agent's tasks under `ext_id` into source `kind`.
/// Shared by Cline, Roo Code, and Kilo Code.
pub fn scan_ext(conn: &mut Connection, kind: &str, ext_id: &str, tick: &mut dyn FnMut()) -> Result<IndexReport> {
    let mut report = IndexReport::default();
    let roots = task_roots_for(ext_id);
    if roots.is_empty() {
        return Ok(report);
    }
    let sid = source_id(conn, kind)?;

    for root in roots {
        // Tag threads by editor so task ids never collide across editors.
        let editor = editor_tag(&root);
        let Ok(tasks) = fs::read_dir(&root) else {
            continue;
        };
        for task in tasks.flatten() {
            tick();
            let dir = task.path();
            if !dir.is_dir() {
                continue;
            }
            match index_task(conn, sid, &editor, &dir) {
                Ok(Some(n)) => {
                    report.threads_indexed += 1;
                    report.messages_indexed += n;
                }
                Ok(None) => report.threads_skipped += 1,
                Err(_) => report.errors += 1,
            }
        }
    }
    Ok(report)
}

/// The editor folder name two levels above `tasks/` (e.g. "Code", "Cursor").
fn editor_tag(tasks_dir: &Path) -> String {
    tasks_dir
        .ancestors()
        .nth(3) // tasks -> globalStorage -> User -> <editor>
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("editor")
        .to_string()
}

fn index_task(conn: &mut Connection, sid: i64, editor: &str, dir: &Path) -> Result<Option<usize>> {
    let history = dir.join("api_conversation_history.json");
    if !history.is_file() {
        return Ok(None);
    }
    let path_str = history.to_string_lossy().to_string();
    let meta = fs::metadata(&history)?;
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

    let task_id = dir.file_name().and_then(|n| n.to_str()).unwrap_or("task");
    // taskId is a unix-ms timestamp; use it for the thread's start time.
    let created_at = task_id.parse::<i64>().ok().map(|ms| ms / 1000);
    let project_path = read_cwd(&dir.join("task_metadata.json"));

    let thread = parse_history(
        &history,
        &format!("{editor}/{task_id}"),
        created_at,
        if mtime > 0 { Some(mtime) } else { created_at },
        project_path,
    )
    .with_context(|| format!("parsing {path_str}"))?;
    let n = if let Some(t) = thread {
        upsert_thread(conn, sid, &t)?
    } else {
        0
    };
    set_file_state(conn, &path_str, KIND, mtime, size)?;
    Ok(Some(n))
}

/// Parse Cline's `api_conversation_history.json` (an Anthropic Messages array).
pub fn parse_history(
    path: &Path,
    external_id: &str,
    created_at: Option<i64>,
    updated_at: Option<i64>,
    project_path: Option<String>,
) -> Result<Option<ParsedThread>> {
    let root: Value = serde_json::from_str(&fs::read_to_string(path)?)?;
    let Some(msgs) = root.as_array() else {
        return Ok(None);
    };

    let mut messages: Vec<ParsedMessage> = Vec::new();
    let mut first_user: Option<String> = None;
    for m in msgs {
        let role = match m.get("role").and_then(Value::as_str) {
            Some("assistant") => "assistant",
            Some("user") => "user",
            _ => continue,
        };
        let before = messages.len();
        extract_blocks(&mut messages, role, m.get("content"), created_at);
        if role == "user" && first_user.is_none() {
            if let Some(msg) = messages.get(before) {
                if msg.role == "user" {
                    first_user = Some(msg.text.clone());
                }
            }
        }
    }

    if messages.is_empty() {
        return Ok(None);
    }
    Ok(Some(ParsedThread {
        external_id: external_id.to_string(),
        title: first_user.map(truncate_title),
        project_path,
        git_branch: None,
        created_at,
        updated_at,
        is_subagent: false,
        messages,
    }))
}

/// Anthropic content: a string or an array of text / tool_use / tool_result blocks.
fn extract_blocks(
    out: &mut Vec<ParsedMessage>,
    role: &str,
    content: Option<&Value>,
    ts: Option<i64>,
) {
    let push = |out: &mut Vec<ParsedMessage>, role: &str, text: String, tool: Option<String>| {
        let text = text.trim().to_string();
        if !text.is_empty() {
            out.push(ParsedMessage {
                role: role.to_string(),
                text,
                tool_name: tool,
                ts,
            });
        }
    };
    match content {
        Some(Value::String(s)) => push(out, role, s.clone(), None),
        Some(Value::Array(blocks)) => {
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(Value::as_str) {
                            push(out, role, t.to_string(), None);
                        }
                    }
                    Some("tool_use") => {
                        let name = b
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("tool")
                            .to_string();
                        let input = b.get("input").map(|v| v.to_string()).unwrap_or_default();
                        push(out, "assistant", format!("{name}: {input}"), Some(name));
                    }
                    Some("tool_result") => {
                        push(out, "tool", stringify_tool_result(b.get("content")), None);
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn stringify_tool_result(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Pull a `cwd` out of Cline's task_metadata.json if present.
fn read_cwd(meta_path: &Path) -> Option<String> {
    let v: Value = serde_json::from_str(&fs::read_to_string(meta_path).ok()?).ok()?;
    v.get("cwd")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .map(str::to_string)
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

    fn temp_path(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("callimachus_{name}_{}_{n}", std::process::id()))
    }

    const HISTORY: &str = r#"[
        {"role":"user","content":[{"type":"text","text":"index cline threads with sqlite fts5"}]},
        {"role":"assistant","content":[{"type":"text","text":"Sure, using FTS5"},{"type":"tool_use","name":"execute_command","input":{"command":"cargo build"}}]},
        {"role":"user","content":[{"type":"tool_result","content":"Finished dev profile"}]}
    ]"#;

    #[test]
    fn parses_history() {
        let path = temp_path("cline_hist.json");
        std::fs::write(&path, HISTORY).unwrap();
        let thread = parse_history(
            &path,
            "Code/1780000000000",
            Some(1780000000),
            Some(1780000000),
            Some("/Users/me/proj".into()),
        )
        .unwrap()
        .expect("non-empty");
        assert_eq!(thread.external_id, "Code/1780000000000");
        assert_eq!(thread.project_path.as_deref(), Some("/Users/me/proj"));
        // user text, assistant text, tool_use, tool_result = 4
        assert_eq!(thread.messages.len(), 4);
        let tool = thread
            .messages
            .iter()
            .find(|m| m.tool_name.is_some())
            .unwrap();
        assert_eq!(tool.tool_name.as_deref(), Some("execute_command"));
    }

    #[test]
    fn index_then_search_roundtrip() {
        let path = temp_path("cline_rt.json");
        std::fs::write(&path, HISTORY).unwrap();
        let mut conn = crate::db::open(&temp_path("cline_rt.db")).unwrap();
        let sid = source_id(&conn, KIND).unwrap();
        let thread = parse_history(&path, "Code/1", None, None, None)
            .unwrap()
            .unwrap();
        upsert_thread(&mut conn, sid, &thread).unwrap();
        let hits =
            crate::search::search(&conn, "fts5", &crate::search::SearchFilters::default()).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.source == "cline"));
    }

    #[test]
    #[ignore]
    fn real_cline_index() {
        let mut conn = crate::db::open(&temp_path("cline_real.db")).unwrap();
        eprintln!("roots: {:?}", task_roots());
        eprintln!("{:?}", scan(&mut conn, &mut || {}).unwrap());
    }
}
