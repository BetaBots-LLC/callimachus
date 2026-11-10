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

use super::common::{
    begin_file, count_file, count_line, count_malformed, count_skipped, join_text_parts,
    push_message, read_json, truncate_title, uncounted,
};
use super::{
    file_change_state, file_state, set_file_state, source_id, unreadable_key, upsert_thread,
    IndexReport, ParsedMessage, ParsedThread,
};
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
    // VSCode-family per-user config dir: macOS `~/Library/Application Support`,
    // Windows `%APPDATA%`, Linux `~/.config` — where editors keep User/globalStorage.
    let Some(support) = dirs::config_dir() else {
        return Vec::new();
    };
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
pub fn scan_ext(
    conn: &mut Connection,
    kind: &str,
    ext_id: &str,
    tick: &mut dyn FnMut(),
) -> Result<IndexReport> {
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
            report.record_at(&dir, index_task(conn, sid, kind, &editor, &dir));
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

fn index_task(
    conn: &mut Connection,
    sid: i64,
    kind: &str,
    editor: &str,
    dir: &Path,
) -> Result<Option<usize>> {
    let history = dir.join("api_conversation_history.json");
    if !history.is_file() {
        return Ok(None);
    }
    let path_str = history.to_string_lossy().to_string();
    let (unchanged, mtime, size) = file_change_state(conn, &history)?;
    if unchanged {
        return Ok(None);
    }

    let task_id = dir.file_name().and_then(|n| n.to_str()).unwrap_or("task");
    // taskId is a unix-ms timestamp; use it for the thread's start time.
    let created_at = task_id.parse::<i64>().ok().map(|ms| ms / 1000);
    let project_path = read_cwd(&dir.join("task_metadata.json"));

    // A history that is not JSON (a write cut short) gets no stamp, so every pass reads it
    // again: the parse canary counts it once per change, not once per pass.
    let unreadable = unreadable_key(&path_str);
    let counted = file_state(conn, &unreadable)? == Some((mtime, size));
    let parse = || {
        parse_history(
            &history,
            &format!("{editor}/{task_id}"),
            created_at,
            if mtime > 0 { Some(mtime) } else { created_at },
            project_path,
        )
    };
    let thread = match if counted { uncounted(parse) } else { parse() } {
        Ok(thread) => thread,
        Err(e) => {
            if !counted && e.downcast_ref::<serde_json::Error>().is_some() {
                set_file_state(conn, &unreadable, kind, mtime, size)?;
            }
            return Err(e).with_context(|| format!("parsing {path_str}"));
        }
    };
    let n = if let Some(t) = thread {
        upsert_thread(conn, sid, &t)?
    } else {
        0
    };
    set_file_state(conn, &path_str, kind, mtime, size)?;
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
    let root: Value = match serde_json::from_str(&fs::read_to_string(path)?) {
        Ok(v) => v,
        Err(e) => {
            count_malformed();
            return Err(e.into());
        }
    };
    let file = begin_file();
    let Some(msgs) = root.as_array() else {
        // The whole document is one record of a shape this parser does not know, and a file
        // that lost its messages: a format change that wraps the array (`{"messages": [..]}`)
        // must reach the canary's counts, not vanish before them.
        count_line();
        count_skipped("history:", Some(super::claude::json_kind(Some(&root))), &[]);
        count_file(file, 0);
        return Ok(None);
    };

    let mut messages: Vec<ParsedMessage> = Vec::new();
    let mut first_user: Option<String> = None;
    for m in msgs {
        count_line();
        let role = m.get("role").and_then(Value::as_str);
        let role = match role {
            Some("assistant") => "assistant",
            Some("user") => "user",
            _ => {
                count_skipped("role:", role, &[]);
                continue;
            }
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

    count_file(file, messages.len());
    if messages.is_empty() {
        return Ok(None);
    }
    Ok(Some(ParsedThread {
        external_id: external_id.to_string(),
        title: first_user.as_deref().map(truncate_title),
        project_path,
        git_branch: None,
        created_at,
        updated_at,
        is_subagent: false,
        usage: Vec::new(),
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
    match content {
        Some(Value::String(s)) => push_message(out, role, s, None, ts),
        Some(Value::Array(blocks)) => {
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(Value::as_str) {
                            push_message(out, role, t, None, ts);
                        }
                    }
                    Some("tool_use") => {
                        let name = b
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("tool")
                            .to_string();
                        let input = b.get("input").map(|v| v.to_string()).unwrap_or_default();
                        let text = format!("{name}: {input}");
                        push_message(out, "assistant", &text, Some(name), ts);
                    }
                    Some("tool_result") => {
                        // Same head + tail cap as the Claude indexer.
                        let text = join_text_parts(b.get("content"));
                        let text = super::claude::cap_tool_text(text.trim());
                        push_message(out, "tool", &text, None, ts);
                    }
                    // Images and thinking are never indexed.
                    other => count_skipped(
                        "block:",
                        other,
                        &["image", "thinking", "redacted_thinking", "document"],
                    ),
                }
            }
        }
        None | Some(Value::Null) => {}
        other => count_skipped("content:", Some(super::claude::json_kind(other)), &[]),
    }
}

/// Pull a `cwd` out of Cline's task_metadata.json if present.
fn read_cwd(meta_path: &Path) -> Option<String> {
    read_json(meta_path)?
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .map(str::to_string)
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

    /// The parse canary (Cline, Roo and Kilo share this parser): an invented block type and
    /// an invented role are counted, images are not, and a history that is not JSON is a
    /// malformed record.
    #[test]
    fn parse_counts_name_unknown_types_and_broken_files() {
        let path = temp_path("cline_canary.json");
        let text = HISTORY.replace(
            r#"{"type":"text","text":"Sure, using FTS5"}"#,
            r#"{"type":"image","source":{}},{"type":"hologram"},{"type":"text","text":"Sure"}"#,
        );
        let text = text.replacen('[', r#"[{"role":"narrator","content":"x"},"#, 1);
        std::fs::write(&path, text).unwrap();
        let (thread, stats) = crate::indexer::common::collect_parse_stats(|| {
            parse_history(&path, "Code/1", None, None, None)
                .unwrap()
                .unwrap()
        });
        assert_eq!(thread.messages.len(), 4);
        assert_eq!(stats.lines, 4);
        assert_eq!(
            stats.unknown_kinds,
            std::collections::BTreeMap::from([
                ("block:hologram".to_string(), 1),
                ("role:narrator".to_string(), 1),
            ])
        );
        std::fs::write(&path, "[{\"role\":").unwrap();
        let (result, stats) = crate::indexer::common::collect_parse_stats(|| {
            parse_history(&path, "Code/1", None, None, None)
        });
        assert!(result.is_err());
        assert_eq!(stats.malformed_lines, 1);
        // An update that wraps the array: the document is one unknown record and a file that
        // lost its messages, so the canary sees it even when no message was ever counted.
        std::fs::write(&path, format!(r#"{{"messages":{HISTORY}}}"#)).unwrap();
        let (result, stats) = crate::indexer::common::collect_parse_stats(|| {
            parse_history(&path, "Code/1", None, None, None)
        });
        let _ = std::fs::remove_file(&path);
        assert!(result.unwrap().is_none());
        assert_eq!((stats.lines, stats.unknown()), (1, 1));
        assert_eq!(stats.unknown_kinds["history:object"], 1);
        assert_eq!((stats.files, stats.files_with_zero_messages), (1, 1));
        // An empty history is an empty file, not one that lost its messages.
        let (_, stats) = crate::indexer::common::collect_parse_stats(|| {
            std::fs::write(&path, "[]").unwrap();
            let r = parse_history(&path, "Code/1", None, None, None);
            let _ = std::fs::remove_file(&path);
            r
        });
        assert_eq!((stats.files, stats.files_with_zero_messages), (1, 0));
    }

    /// A history that is not JSON gets no stamp and is read again on every pass; the parse
    /// canary counts it once per change (see `unreadable_key`).
    #[test]
    fn a_broken_history_is_counted_once_per_change() {
        let dir = crate::db::test_support::TestDir::new("cline_broken");
        let task = dir.with_file_name("1700000000000");
        std::fs::create_dir_all(&task).unwrap();
        let history = task.join("api_conversation_history.json");
        std::fs::write(&history, "[{\"role\":").unwrap();
        let mut conn = crate::db::open(&dir).unwrap();
        let sid = source_id(&conn, KIND).unwrap();
        let pass = |conn: &mut Connection| {
            let (result, stats) = crate::indexer::common::collect_parse_stats(|| {
                index_task(conn, sid, KIND, "Code", &task)
            });
            assert!(
                result.is_err(),
                "still reported as a file that did not parse"
            );
            stats
        };
        assert_eq!(pass(&mut conn).malformed_lines, 1);
        assert!(pass(&mut conn).is_empty(), "the same file: counted once");
        assert_eq!(
            file_state(&conn, &history.to_string_lossy()).unwrap(),
            None,
            "never recorded as indexed"
        );
        // Rewritten and still broken: a change, counted again.
        std::fs::write(&history, "[{\"role\": \"user\", \"content\":").unwrap();
        assert_eq!(pass(&mut conn).malformed_lines, 1);
        assert!(pass(&mut conn).is_empty());
    }

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
