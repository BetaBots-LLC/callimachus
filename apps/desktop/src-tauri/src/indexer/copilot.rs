//! VS Code-native / GitHub Copilot chat indexer. VS Code and its forks (Cursor,
//! VSCodium, Windsurf, Insiders) persist the built-in chat panel — whatever agent
//! or model drives it, Copilot included — as one JSON file per session:
//!   <editor>/User/workspaceStorage/<hash>/chatSessions/<uuid>.{json,jsonl}  (workspace chats)
//!   <editor>/User/globalStorage/emptyWindowChatSessions/<uuid>.{json,jsonl} (no-folder chats)
//!
//! The extension is `.json` on some VS Code versions/platforms and `.jsonl` on others,
//! but each file is a SINGLE JSON object `{ kind, v }` regardless,
//! where `v.requests[]` are the turns. Per request: `message.text` (the user turn),
//! `response[]` (the assistant markdown), and `modelId` (e.g. "copilot/gpt-5.3-codex").
//! The model is recorded per assistant message via the usage/model column, so a chat
//! that switches models mid-conversation keeps each turn's model. Project path comes
//! from the workspace's `workspace.json` folder URI.

use super::{
    file_state, set_file_state, source_id, upsert_thread, IndexReport, MsgUsage, ParsedMessage,
    ParsedThread,
};
use anyhow::Result;
use rusqlite::Connection;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

pub const KIND: &str = "copilot";

/// VS Code-family editors that may host the native chat. Same set as the Cline forks.
const EDITORS: &[&str] = &["Code", "Code - Insiders", "Cursor", "VSCodium", "Windsurf"];

/// `<config_dir>/<editor>/User` for each installed editor (macOS `~/Library/Application
/// Support`, Windows `%APPDATA%`, Linux `~/.config`).
fn editor_user_dirs() -> Vec<PathBuf> {
    let Some(support) = dirs::config_dir() else {
        return Vec::new();
    };
    EDITORS
        .iter()
        .map(|ed| support.join(ed).join("User"))
        .filter(|p| p.is_dir())
        .collect()
}

/// Targeted watch roots: each editor's `workspaceStorage` and `emptyWindowChatSessions`.
pub fn watch_roots() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for user in editor_user_dirs() {
        let ws = user.join("workspaceStorage");
        if ws.is_dir() {
            out.push(ws);
        }
        let ew = user.join("globalStorage/emptyWindowChatSessions");
        if ew.is_dir() {
            out.push(ew);
        }
    }
    out
}

pub fn scan(conn: &mut Connection, tick: &mut dyn FnMut()) -> Result<IndexReport> {
    let mut report = IndexReport::default();
    let sid = source_id(conn, KIND)?;
    for user in editor_user_dirs() {
        // Per-workspace chats: workspaceStorage/<hash>/chatSessions/*.jsonl
        if let Ok(entries) = fs::read_dir(user.join("workspaceStorage")) {
            for ws in entries.flatten() {
                let hash_dir = ws.path();
                let chat_dir = hash_dir.join("chatSessions");
                if !chat_dir.is_dir() {
                    continue;
                }
                let project = workspace_folder(&hash_dir);
                index_dir(conn, sid, &chat_dir, project.as_deref(), &mut report, tick);
            }
        }
        // No-folder window chats have no associated project.
        let empty = user.join("globalStorage/emptyWindowChatSessions");
        if empty.is_dir() {
            index_dir(conn, sid, &empty, None, &mut report, tick);
        }
    }
    Ok(report)
}

fn index_dir(
    conn: &mut Connection,
    sid: i64,
    dir: &Path,
    project: Option<&str>,
    report: &mut IndexReport,
    tick: &mut dyn FnMut(),
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Session files are `<uuid>.json` on some VS Code versions/platforms and
        // `<uuid>.jsonl` on others; both hold the same single `{kind, v}` JSON.
        let ext = path.extension().and_then(|e| e.to_str());
        if ext != Some("json") && ext != Some("jsonl") {
            continue;
        }
        tick();
        match index_session(conn, sid, &path, project) {
            Ok(Some(n)) => {
                report.threads_indexed += 1;
                report.messages_indexed += n;
            }
            Ok(None) => report.threads_skipped += 1,
            Err(_) => report.errors += 1,
        }
    }
}

/// Parse one chat-session file and upsert it. Skips when the file is unchanged since
/// the last pass (the whole session is one file, rewritten on each new turn).
fn index_session(
    conn: &mut Connection,
    sid: i64,
    path: &Path,
    project: Option<&str>,
) -> Result<Option<usize>> {
    let meta = fs::metadata(path)?;
    let size = meta.len() as i64;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let key = path.to_string_lossy().to_string();
    if let Some((pm, ps)) = file_state(conn, &key)? {
        if pm == mtime && ps == size {
            return Ok(None);
        }
    }

    let Some(root) = read_json(path) else {
        return Ok(None);
    };
    // The session lives under `v` (the `{kind, v}` wrapper); tolerate a bare root too.
    let v = root.get("v").unwrap_or(&root);
    let session_id = v
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    if session_id.is_empty() {
        return Ok(None);
    }

    let mut messages: Vec<ParsedMessage> = Vec::new();
    let mut usage: Vec<(usize, MsgUsage)> = Vec::new();
    let mut first_user: Option<String> = None;
    let mut max_ts: Option<i64> = None;

    if let Some(requests) = v.get("requests").and_then(Value::as_array) {
        for req in requests {
            let ts = req
                .get("timestamp")
                .and_then(Value::as_i64)
                .map(|ms| ms / 1000);
            if let Some(t) = ts {
                max_ts = Some(max_ts.map_or(t, |m: i64| m.max(t)));
            }

            // User turn.
            let utext = req
                .get("message")
                .and_then(|m| m.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if !utext.is_empty() {
                if first_user.is_none() {
                    first_user = Some(utext.clone());
                }
                messages.push(ParsedMessage {
                    role: "user".into(),
                    text: utext,
                    tool_name: None,
                    ts,
                });
            }

            // Assistant turn + its model.
            let atext = response_text(req.get("response"));
            if !atext.is_empty() {
                let idx = messages.len();
                messages.push(ParsedMessage {
                    role: "assistant".into(),
                    text: atext,
                    tool_name: None,
                    ts,
                });
                if let Some(model) = req.get("modelId").and_then(Value::as_str) {
                    // "copilot/gpt-5.3-codex" -> "gpt-5.3-codex"; bare ids pass through.
                    let model = model.rsplit('/').next().unwrap_or(model).to_string();
                    let m = req.get("result").and_then(|r| r.get("metadata"));
                    let input = m
                        .and_then(|m| m.get("promptTokens"))
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    let output = m
                        .and_then(|m| m.get("completionTokens").or_else(|| m.get("outputTokens")))
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    usage.push((
                        idx,
                        MsgUsage {
                            model,
                            input,
                            output,
                            cache_write: 0,
                            cache_read: 0,
                        },
                    ));
                }
            }
        }
    }

    if messages.is_empty() {
        // No upsert that could fail — safe to record the fingerprint now.
        set_file_state(conn, &key, KIND, mtime, size)?;
        return Ok(Some(0));
    }

    let created_at = v
        .get("creationDate")
        .and_then(Value::as_i64)
        .map(|ms| ms / 1000);
    let thread = ParsedThread {
        external_id: session_id,
        title: first_user.map(truncate_title),
        project_path: project.map(str::to_string),
        git_branch: None,
        created_at,
        updated_at: max_ts.or(created_at),
        is_subagent: false,
        usage,
        messages,
    };
    let n = upsert_thread(conn, sid, &thread)?;
    set_file_state(conn, &key, KIND, mtime, size)?;
    Ok(Some(n))
}

/// Concatenate the assistant's rendered markdown from a request's `response[]`,
/// skipping thinking / tool / progress parts (only top-level string `value`s are
/// the answer text).
fn response_text(response: Option<&Value>) -> String {
    let Some(arr) = response.and_then(Value::as_array) else {
        return String::new();
    };
    let mut out = Vec::new();
    for part in arr {
        if part.get("kind").and_then(Value::as_str) == Some("thinking") {
            continue;
        }
        if let Some(s) = part.get("value").and_then(Value::as_str) {
            let s = s.trim();
            if !s.is_empty() {
                out.push(s.to_string());
            }
        }
    }
    out.join("\n")
}

/// Resolve a workspace hash dir to its project folder path via `workspace.json`
/// (`{ "folder": "file:///..." }`). None for multi-root / no-folder windows.
fn workspace_folder(hash_dir: &Path) -> Option<String> {
    let json = read_json(&hash_dir.join("workspace.json"))?;
    let folder = json.get("folder").and_then(Value::as_str)?;
    uri_to_path(folder)
}

/// `file:///Users/me/proj` -> `/Users/me/proj`; `file:///c%3A/Users` -> `C:/Users`.
fn uri_to_path(uri: &str) -> Option<String> {
    let decoded = pct_decode(uri.strip_prefix("file://")?);
    let b = decoded.as_bytes();
    // Windows drive: "/C:/Users/.." -> "C:/Users/.."
    let p = if b.len() > 2 && b[0] == b'/' && b[2] == b':' {
        decoded[1..].to_string()
    } else {
        decoded
    };
    let p = p.trim_end_matches('/').to_string();
    (!p.is_empty()).then_some(p)
}

/// Minimal percent-decoder for `%XX` escapes (paths are effectively ASCII).
fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(byte as char);
                i += 3;
                continue;
            }
        }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
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

    #[test]
    fn indexes_chat_with_per_message_model() {
        let dir = std::env::temp_dir().join(format!("calli_copilot_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("cf8a106e.jsonl");
        // Two turns on different models, to prove per-message capture.
        let sample = r#"{"kind":0,"v":{"version":3,"creationDate":1782773073952,"sessionId":"cf8a106e",
          "requests":[
            {"timestamp":1782773082644,"modelId":"copilot/gpt-5.3-codex",
             "message":{"text":"fix the warranty audit flow"},
             "response":[{"kind":"thinking","value":["hmm"]},{"value":"Done. Updated the flow."}],
             "result":{"metadata":{"promptTokens":29273,"completionTokens":140}}},
            {"timestamp":1782773090000,"modelId":"copilot/claude-sonnet-4.6",
             "message":{"text":"now add a test"},
             "response":[{"value":"Added the test."}],
             "result":{"metadata":{"promptTokens":300,"outputTokens":50}}}
          ]}}"#;
        std::fs::write(&file, sample).unwrap();

        let dbp = dir.join("db.sqlite");
        let mut conn = crate::db::open(&dbp).unwrap();
        let sid = source_id(&conn, KIND).unwrap();
        let n = index_session(&mut conn, sid, &file, Some("/Users/me/proj"))
            .unwrap()
            .unwrap();
        assert_eq!(n, 4); // 2 user + 2 assistant

        // Per-message models land on the two assistant rows, in order.
        let models: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT model FROM messages WHERE role='assistant' ORDER BY seq")
                .unwrap();
            let v = stmt
                .query_map([], |r| r.get::<_, Option<String>>(0))
                .unwrap()
                .map(|r| r.unwrap().unwrap_or_default())
                .collect();
            v
        };
        assert_eq!(models, vec!["gpt-5.3-codex", "claude-sonnet-4.6"]);

        // Thread project + title + source.
        let detail = crate::search::thread_detail(
            &conn,
            conn.query_row("SELECT id FROM threads", [], |r| r.get::<_, i64>(0))
                .unwrap(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(detail.project_path.as_deref(), Some("/Users/me/proj"));
        assert!(detail.title.as_deref().unwrap_or("").contains("warranty"));

        // Re-scan no-ops (fingerprint unchanged).
        assert!(index_session(&mut conn, sid, &file, Some("/Users/me/proj"))
            .unwrap()
            .is_none());
    }

    /// Real-data smoke test. Run with `cargo test -- --ignored real_copilot_index --nocapture`.
    #[test]
    #[ignore]
    fn real_copilot_index() {
        let p = std::env::temp_dir().join(format!("calli_copilot_real_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let mut conn = crate::db::open(&p).unwrap();
        let report = scan(&mut conn, &mut || {}).unwrap();
        eprintln!("{report:?}");
        let by_model: Vec<(String, i64)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT model, COUNT(*) FROM messages WHERE model IS NOT NULL
                     GROUP BY model ORDER BY 2 DESC",
                )
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        eprintln!("models: {by_model:?}");
    }

    #[test]
    fn uri_to_path_handles_posix_and_windows() {
        assert_eq!(
            uri_to_path("file:///Users/me/proj").as_deref(),
            Some("/Users/me/proj")
        );
        assert_eq!(
            uri_to_path("file:///c%3A/Users/me").as_deref(),
            Some("c:/Users/me")
        );
    }
}
