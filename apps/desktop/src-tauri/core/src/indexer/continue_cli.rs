//! Continue CLI (`cn`) indexer. Sessions are one JSON file each under
//! `~/.continue/sessions/<sessionId>.json`:
//!   { sessionId, title, workspaceDirectory, history: [{ message: { role, content } }] }
//! `content` is a string or an array of parts (`{type:"text", text}`). System
//! messages are already filtered out by Continue. There are no per-message
//! timestamps, so we use the file's mtime for the thread's time bounds.
//!
//! Schema verified against continuedev/continue `extensions/cli/src/session.ts`.

use super::common::{
    begin_file, count_file, count_line, count_malformed, count_skipped, join_text_parts,
    truncate_title, uncounted,
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

pub const KIND: &str = "continue";

/// `~/.continue/sessions`, or None if HOME is unset.
pub fn sessions_root() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".continue").join("sessions"))
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
        report.record_at(&path, index_file(conn, sid, &path));
    }
    Ok(report)
}

fn index_file(conn: &mut Connection, sid: i64, path: &Path) -> Result<Option<usize>> {
    let path_str = path.to_string_lossy().to_string();
    let (unchanged, mtime, size) = file_change_state(conn, path)?;
    if unchanged {
        return Ok(None);
    }

    let fallback_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("session");
    // A session that is not JSON (a write cut short) gets no stamp, so every pass reads it
    // again: the parse canary counts it once per change, not once per pass.
    let unreadable = unreadable_key(&path_str);
    let counted = file_state(conn, &unreadable)? == Some((mtime, size));
    let parse = || parse_file(path, fallback_id, mtime);
    let thread = match if counted { uncounted(parse) } else { parse() } {
        Ok(thread) => thread,
        Err(e) => {
            if !counted && e.downcast_ref::<serde_json::Error>().is_some() {
                set_file_state(conn, &unreadable, KIND, mtime, size)?;
            }
            return Err(e).with_context(|| format!("parsing {path_str}"));
        }
    };
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
    let root: Value = match serde_json::from_str(&fs::read_to_string(path)?) {
        Ok(v) => v,
        Err(e) => {
            count_malformed();
            return Err(e.into());
        }
    };

    let external_id = root
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or(fallback_id)
        .to_string();
    let ts = if mtime > 0 { Some(mtime) } else { None };

    let file = begin_file();
    let mut messages: Vec<ParsedMessage> = Vec::new();
    let mut first_user: Option<String> = None;
    let history = root.get("history");
    if let Some(history) = history.and_then(Value::as_array) {
        for item in history {
            count_line();
            let msg = item.get("message").unwrap_or(item);
            let role = msg.get("role").and_then(Value::as_str);
            let role = match role {
                Some("assistant") => "assistant",
                Some("user") => "user",
                Some("tool") => "tool",
                // system already filtered by Continue
                _ => {
                    count_skipped("role:", role, &["system"]);
                    continue;
                }
            };
            // `content` is a plain string or an array of `{type:"text", text}` parts.
            let text = join_text_parts(msg.get("content"));
            let text = text.trim();
            // Same head + tail cap as the Claude indexer for tool output.
            let text = if role == "tool" {
                super::claude::cap_tool_text(text)
            } else {
                text.to_string()
            };
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
    } else {
        // No `history` array: the document is one record of a shape this parser does not
        // know (an update that renamed or nested it), counted so the canary sees the file
        // stop yielding messages instead of nothing at all.
        count_line();
        count_skipped("history:", Some(super::claude::json_kind(history)), &[]);
    }

    count_file(file, messages.len());
    if messages.is_empty() {
        return Ok(None);
    }
    let title = root
        .get("title")
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
        .map(str::to_string)
        .or_else(|| first_user.as_deref().map(truncate_title));
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

    /// A session file that is not JSON gets no stamp and is read again on every pass; the
    /// parse canary counts it once per change. Counted on every pass, one broken file summed
    /// over the watcher's batches into a format-change warning.
    #[test]
    fn a_broken_session_is_counted_once_per_change() {
        let dir = crate::db::test_support::TestDir::new("continue_broken");
        let broken = dir.with_file_name("s1.json");
        std::fs::write(&broken, "{\"sessionId\":").unwrap();
        let mut conn = crate::db::open(&dir).unwrap();
        let sid = source_id(&conn, KIND).unwrap();
        let pass = |conn: &mut Connection| {
            let (result, stats) =
                crate::indexer::common::collect_parse_stats(|| index_file(conn, sid, &broken));
            assert!(
                result.is_err(),
                "still reported as a file that did not parse"
            );
            stats
        };
        assert_eq!(pass(&mut conn).malformed_lines, 1);
        assert!(pass(&mut conn).is_empty(), "the same file: counted once");
        assert_eq!(
            file_state(&conn, &broken.to_string_lossy()).unwrap(),
            None,
            "never recorded as indexed"
        );
        // Rewritten and still broken: a change, counted again.
        std::fs::write(&broken, "{\"sessionId\": \"x\", \"history\": [").unwrap();
        assert_eq!(pass(&mut conn).malformed_lines, 1);
        assert!(pass(&mut conn).is_empty());
    }

    const SAMPLE: &str = r#"{"sessionId":"sess-cn","title":"FTS5 work","workspaceDirectory":"/Users/me/proj","history":[
        {"message":{"role":"system","content":"you are helpful"}},
        {"message":{"role":"user","content":"index continue threads with sqlite fts5"}},
        {"message":{"role":"assistant","content":[{"type":"text","text":"Sure, using FTS5"}]}}
    ]}"#;

    /// The parse canary: a history item of an invented role is counted, `system` is not.
    #[test]
    fn parse_counts_name_unknown_roles() {
        let path = temp_path("cn_canary.json");
        let text = SAMPLE.replace(
            r#"{"message":{"role":"system","content":"you are helpful"}},"#,
            r#"{"message":{"role":"system","content":"x"}},{"message":{"role":"narrator","content":"x"}},"#,
        );
        std::fs::write(&path, text).unwrap();
        let (thread, stats) = crate::indexer::common::collect_parse_stats(|| {
            parse_file(&path, "fallback", 1780000000).unwrap().unwrap()
        });
        assert_eq!(thread.messages.len(), 2);
        assert_eq!(stats.lines, 4);
        assert_eq!(
            stats.unknown_kinds,
            std::collections::BTreeMap::from([("role:narrator".to_string(), 1)])
        );
        assert_eq!((stats.files, stats.files_with_zero_messages), (1, 0));

        // An update that renames `history`: before, the file yielded nothing and counted
        // nothing; now it is one unknown record in a file without messages.
        std::fs::write(&path, SAMPLE.replace(r#""history""#, r#""turns""#)).unwrap();
        let (thread, stats) = crate::indexer::common::collect_parse_stats(|| {
            parse_file(&path, "fallback", 1780000000).unwrap()
        });
        let _ = std::fs::remove_file(&path);
        assert!(thread.is_none());
        assert_eq!((stats.lines, stats.unknown()), (1, 1));
        assert_eq!(stats.unknown_kinds["history:null"], 1);
        assert_eq!((stats.files, stats.files_with_zero_messages), (1, 1));
    }

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
