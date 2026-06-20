//! OpenCode indexer. OpenCode (sst/opencode) stores each session as a tree of
//! small JSON files under `$OPENCODE_DATA_DIR` / `$XDG_DATA_HOME/opencode/storage`
//! (default `~/.local/share/opencode/storage`):
//!   session/<projectID>/ses_<id>.json   -> { id, title, directory, time:{created} }
//!   message/<sessionID>/msg_*.json       -> { id, role, time:{created} }
//!   part/<messageID>/<n>_*.json          -> { type:"text", text }
//! A turn is reconstructed by joining message + its parts by id. Timestamps are
//! unix milliseconds.
//!
//! Schema verified against opencode `packages/sdk/js/src/gen/types.gen.ts`.

use super::{
    file_state, set_file_state, source_id, upsert_thread, IndexReport, ParsedMessage, ParsedThread,
};
use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

pub const KIND: &str = "opencode";

/// OpenCode storage root. Honors OPENCODE_DATA_DIR (first entry of its list),
/// then XDG_DATA_HOME, else ~/.local/share/opencode/storage.
pub fn storage_root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("OPENCODE_DATA_DIR") {
        let first = dir.to_string_lossy();
        let first = first.split(',').next().unwrap_or("").trim();
        if !first.is_empty() {
            return Some(PathBuf::from(first).join("storage"));
        }
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(base.join("opencode/storage"))
}

pub fn scan(conn: &mut Connection, tick: &mut dyn FnMut()) -> Result<IndexReport> {
    let Some(root) = storage_root() else {
        return Ok(IndexReport::default());
    };
    scan_root(conn, &root, tick)
}

fn scan_root(conn: &mut Connection, root: &Path, tick: &mut dyn FnMut()) -> Result<IndexReport> {
    let mut report = IndexReport::default();
    let session_dir = root.join("session");
    if !session_dir.is_dir() {
        return Ok(report);
    }
    let sid = source_id(conn, KIND)?;

    let mut session_files = Vec::new();
    collect_session_files(&session_dir, &mut session_files);

    for sf in session_files {
        tick();
        match index_session(conn, sid, root, &sf) {
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

/// Collect `ses_*.json` files under session/<projectID>/.
fn collect_session_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_session_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("json")
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("ses_"))
        {
            out.push(path);
        }
    }
}

fn index_session(conn: &mut Connection, sid: i64, root: &Path, sf: &Path) -> Result<Option<usize>> {
    let session = read_json(sf).with_context(|| format!("reading {}", sf.display()))?;
    let session_id = session
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| sf.file_stem().and_then(|s| s.to_str()).map(str::to_string))
        .unwrap_or_default();
    if session_id.is_empty() {
        return Ok(None);
    }

    let msg_dir = root.join("message").join(&session_id);
    // Change fingerprint for the session: (max message-file mtime, message count).
    // Avoids re-upserting (and re-embedding) unchanged sessions every pass.
    let (fp_mtime, fp_count) = dir_fingerprint(&msg_dir);
    let key = sf.to_string_lossy().to_string();
    if let Some((prev_mtime, prev_count)) = file_state(conn, &key)? {
        if prev_mtime == fp_mtime && prev_count == fp_count {
            return Ok(None);
        }
    }

    let mut messages: Vec<ParsedMessage> = Vec::new();
    let mut first_user: Option<String> = None;
    let mut max_ts: Option<i64> = None;

    let mut msg_files = read_dir_sorted(&msg_dir);
    msg_files.sort();
    for mf in msg_files {
        let Some(msg) = read_json(&mf) else { continue };
        let role = match msg.get("role").and_then(Value::as_str) {
            Some("assistant") => "assistant",
            Some("user") => "user",
            _ => continue,
        };
        let ts = msg
            .get("time")
            .and_then(|t| t.get("created"))
            .and_then(Value::as_i64)
            .map(|ms| ms / 1000);
        if let Some(ts) = ts {
            max_ts = Some(max_ts.map_or(ts, |m: i64| m.max(ts)));
        }
        let message_id = msg.get("id").and_then(Value::as_str).unwrap_or_default();
        let text = read_parts_text(root, message_id);
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

    set_file_state(conn, &key, KIND, fp_mtime, fp_count)?;

    if messages.is_empty() {
        return Ok(Some(0));
    }

    let created_at = session
        .get("time")
        .and_then(|t| t.get("created"))
        .and_then(Value::as_i64)
        .map(|ms| ms / 1000);
    let title = session
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|t| !t.trim().is_empty())
        .or_else(|| first_user.map(truncate_title));
    let project_path = session
        .get("directory")
        .and_then(Value::as_str)
        .filter(|d| !d.is_empty())
        .map(str::to_string);

    let thread = ParsedThread {
        external_id: session_id,
        title,
        project_path,
        git_branch: None,
        created_at,
        updated_at: max_ts.or(created_at),
        is_subagent: false,
        messages,
    };
    Ok(Some(upsert_thread(conn, sid, &thread)?))
}

/// Concatenate the text of all `type:"text"` parts of a message, ordered by file.
fn read_parts_text(root: &Path, message_id: &str) -> String {
    if message_id.is_empty() {
        return String::new();
    }
    let part_dir = root.join("part").join(message_id);
    let mut files = read_dir_sorted(&part_dir);
    files.sort();
    let mut out = Vec::new();
    for pf in files {
        let Some(part) = read_json(&pf) else { continue };
        if part.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(t) = part.get("text").and_then(Value::as_str) {
                let t = t.trim();
                if !t.is_empty() {
                    out.push(t.to_string());
                }
            }
        }
    }
    out.join("\n")
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn read_dir_sorted(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect()
}

/// (max mtime secs, file count) of a directory's immediate files.
fn dir_fingerprint(dir: &Path) -> (i64, i64) {
    let files = read_dir_sorted(dir);
    let count = files.len() as i64;
    let max_mtime = files
        .iter()
        .filter_map(|p| fs::metadata(p).ok())
        .filter_map(|m| m.modified().ok())
        .filter_map(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .max()
        .unwrap_or(0);
    (max_mtime, count)
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
    use std::fs::create_dir_all;

    fn temp_dir(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("callimachus_{name}_{}_{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    /// Build an OpenCode-shaped storage tree and verify reconstruction.
    #[test]
    fn extracts_from_opencode_tree() {
        let root = temp_dir("oc_store");
        create_dir_all(root.join("session/proj1")).unwrap();
        create_dir_all(root.join("message/ses_abc")).unwrap();
        create_dir_all(root.join("part/msg_1")).unwrap();
        create_dir_all(root.join("part/msg_2")).unwrap();

        fs::write(
            root.join("session/proj1/ses_abc.json"),
            r#"{"id":"ses_abc","title":"FTS5 work","directory":"/Users/me/proj","time":{"created":1780000000000}}"#,
        )
        .unwrap();
        fs::write(
            root.join("message/ses_abc/msg_001_msg_1.json"),
            r#"{"id":"msg_1","sessionID":"ses_abc","role":"user","time":{"created":1780000000000}}"#,
        )
        .unwrap();
        fs::write(
            root.join("message/ses_abc/msg_002_msg_2.json"),
            r#"{"id":"msg_2","sessionID":"ses_abc","role":"assistant","time":{"created":1780000005000}}"#,
        )
        .unwrap();
        fs::write(
            root.join("part/msg_1/000_p1.json"),
            r#"{"id":"p1","messageID":"msg_1","type":"text","text":"index opencode threads with sqlite fts5"}"#,
        )
        .unwrap();
        fs::write(
            root.join("part/msg_2/000_p2.json"),
            r#"{"id":"p2","messageID":"msg_2","type":"text","text":"Sure, using FTS5"}"#,
        )
        .unwrap();

        let dst = temp_dir("oc_dst");
        create_dir_all(&dst).unwrap();
        let mut conn = crate::db::open(&dst.join("db.sqlite")).unwrap();
        let report = scan_root(&mut conn, &root, &mut || {}).unwrap();
        assert_eq!(report.threads_indexed, 1);
        assert_eq!(report.messages_indexed, 2);

        let hits =
            crate::search::search(&conn, "fts5", &crate::search::SearchFilters::default()).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.source == "opencode"));

        let tid: i64 = conn
            .query_row("SELECT id FROM threads", [], |r| r.get(0))
            .unwrap();
        let detail = crate::search::thread_detail(&conn, tid).unwrap().unwrap();
        assert_eq!(detail.title.as_deref(), Some("FTS5 work"));
        assert_eq!(detail.project_path.as_deref(), Some("/Users/me/proj"));

        // Second scan with no changes is skipped (fingerprint match).
        let report2 = scan_root(&mut conn, &root, &mut || {}).unwrap();
        assert_eq!(report2.threads_indexed, 0);
        assert_eq!(report2.threads_skipped, 1);
    }

    #[test]
    #[ignore]
    fn real_opencode_index() {
        let mut conn = crate::db::open(&temp_dir("oc_real").join("db.sqlite")).unwrap();
        eprintln!("{:?}", scan(&mut conn, &mut || {}).unwrap());
    }
}
