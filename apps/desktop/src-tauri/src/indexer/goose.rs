//! Goose indexer. Block's `goose` agent stores all sessions in one global SQLite
//! DB at `$XDG_DATA_HOME/goose/sessions/sessions.db` (default
//! `~/.local/share/goose/sessions/sessions.db`), since Goose v1.10. Two tables:
//! `sessions` (id, name, description, working_dir, …) and `messages`
//! (session_id, role, content_json, created_timestamp). `content_json` holds the
//! serialized Goose message (text + tool calls), so we pull human-readable text
//! out of it best-effort.
//!
//! Schema verified against block/goose `session_manager.rs`.

use super::{
    file_change_state, set_file_state, source_id, upsert_thread, IndexReport, ParsedMessage,
    ParsedThread,
};
use anyhow::Result;
use rusqlite::{params, Connection};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub const KIND: &str = "goose";

/// Path to Goose's global sessions DB (honors XDG_DATA_HOME, else ~/.local/share).
pub fn sessions_db_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/share")))?;
    Some(base.join("goose/sessions/sessions.db"))
}

struct Session {
    id: String,
    name: Option<String>,
    description: Option<String>,
    working_dir: Option<String>,
}

pub fn scan(conn: &mut Connection, tick: &mut dyn FnMut()) -> Result<IndexReport> {
    let Some(db) = sessions_db_path() else {
        return Ok(IndexReport::default());
    };
    scan_path(conn, &db, tick)
}

/// Index Goose sessions from a specific sessions.db. Skips if the file is unchanged.
fn scan_path(conn: &mut Connection, db: &Path, tick: &mut dyn FnMut()) -> Result<IndexReport> {
    let mut report = IndexReport::default();
    if !db.exists() {
        return Ok(report);
    }
    let sid = source_id(conn, KIND)?;
    let (unchanged, mtime, size) = file_change_state(conn, db)?;
    if unchanged {
        return Ok(report);
    }

    let ro = super::open_external_readonly(db)?;
    let sessions = list_sessions(&ro)?;

    let mut msg_stmt = ro.prepare(
        "SELECT role, content_json, created_timestamp
         FROM messages WHERE session_id = ?1 ORDER BY created_timestamp, id",
    )?;

    for s in sessions {
        tick();
        let mut messages: Vec<ParsedMessage> = Vec::new();
        let mut first_user: Option<String> = None;
        let mut min_ts: Option<i64> = None;
        let mut max_ts: Option<i64> = None;

        let rows = msg_stmt.query_map(params![s.id], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?, // role
                r.get::<_, Option<String>>(1)?, // content_json
                r.get::<_, Option<i64>>(2)?,    // created_timestamp
            ))
        })?;

        for row in rows {
            let (role, content_json, raw_ts) = row?;
            let role = match role.as_deref() {
                Some("assistant") => "assistant",
                _ => "user",
            };
            let text = content_json
                .as_deref()
                .map(extract_text)
                .unwrap_or_default();
            let text = text.trim().to_string();
            if text.is_empty() {
                continue;
            }
            let ts = raw_ts.map(normalize_ts);
            if let Some(ts) = ts {
                min_ts = Some(min_ts.map_or(ts, |m: i64| m.min(ts)));
                max_ts = Some(max_ts.map_or(ts, |m: i64| m.max(ts)));
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

        if messages.is_empty() {
            report.threads_skipped += 1;
            continue;
        }

        let title = s
            .name
            .filter(|n| !n.trim().is_empty())
            .or_else(|| s.description.filter(|d| !d.trim().is_empty()))
            .or_else(|| first_user.map(truncate_title));
        let thread = ParsedThread {
            external_id: s.id,
            title,
            project_path: s.working_dir.filter(|w| !w.is_empty()),
            git_branch: None,
            created_at: min_ts,
            updated_at: max_ts.or(min_ts),
            is_subagent: false,
            usage: Vec::new(),
            messages,
        };
        let n = upsert_thread(conn, sid, &thread)?;
        report.threads_indexed += 1;
        report.messages_indexed += n;
    }

    // Record the DB file as indexed ONLY after every session upserted without error: a
    // mid-pass failure (e.g. a transient write-lock) returns above via `?`, leaving the file
    // "changed" so the watcher's retry re-indexes it instead of skipping dropped sessions.
    set_file_state(conn, &db.to_string_lossy(), KIND, mtime, size)?;
    Ok(report)
}

fn list_sessions(ro: &Connection) -> Result<Vec<Session>> {
    let mut stmt = ro.prepare("SELECT id, name, description, working_dir FROM sessions")?;
    let rows = stmt.query_map([], |r| {
        Ok(Session {
            id: r.get(0)?,
            name: r.get::<_, Option<String>>(1)?,
            description: r.get::<_, Option<String>>(2)?,
            working_dir: r.get::<_, Option<String>>(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Goose stores `created_timestamp` as unix seconds; defensively rescale if a
/// value is clearly milliseconds.
fn normalize_ts(t: i64) -> i64 {
    if t > 100_000_000_000 {
        t / 1000
    } else {
        t
    }
}

/// Pull human-readable text out of a serialized Goose message. The content is a
/// JSON value whose shape varies (string, `{text}`, tagged enums, arrays of tool
/// parts); we recursively gather every `text` string field, plus bare strings.
fn extract_text(content_json: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(content_json) else {
        return content_json.to_string(); // not JSON — treat as plain text
    };
    let mut out = Vec::new();
    collect_text(&v, &mut out);
    out.join("\n")
}

fn collect_text(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => {
            // Top-level / array-of-string content.
            let s = s.trim();
            if !s.is_empty() {
                out.push(s.to_string());
            }
        }
        Value::Array(items) => items.iter().for_each(|i| collect_text(i, out)),
        Value::Object(map) => {
            if let Some(Value::String(t)) = map.get("text") {
                let t = t.trim();
                if !t.is_empty() {
                    out.push(t.to_string());
                }
            }
            // Recurse into nested objects (tagged enum variants like {"Text":{...}}).
            for (k, val) in map {
                if k != "text" {
                    if let Value::Object(_) = val {
                        collect_text(val, out);
                    } else if let Value::Array(_) = val {
                        collect_text(val, out);
                    }
                }
            }
        }
        _ => {}
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

    fn temp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("callimachus_{name}_{}", std::process::id()))
    }

    #[test]
    fn extracts_nested_text() {
        assert_eq!(extract_text(r#"{"text":"hello"}"#), "hello");
        assert_eq!(extract_text(r#"[{"text":"a"},{"text":"b"}]"#), "a\nb");
        assert_eq!(extract_text(r#"{"Text":{"text":"tagged"}}"#), "tagged");
        assert_eq!(
            extract_text("plain string not json"),
            "plain string not json"
        );
    }

    /// Build a Goose-shaped sessions.db and verify extraction.
    #[test]
    fn extracts_from_goose_shaped_db() {
        let src = temp("goose_src.db");
        let _ = std::fs::remove_file(&src);
        {
            let c = Connection::open(&src).unwrap();
            c.execute_batch(
                "CREATE TABLE sessions (id TEXT PRIMARY KEY, name TEXT, description TEXT, working_dir TEXT);
                 CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT, role TEXT, content_json TEXT, created_timestamp INTEGER);
                 INSERT INTO sessions VALUES ('20260601_1', '', 'Add FTS5 search', '/Users/me/proj');
                 INSERT INTO messages (session_id, role, content_json, created_timestamp) VALUES
                   ('20260601_1', 'user', json('{\"text\":\"index goose threads with sqlite fts5\"}'), 1780000000),
                   ('20260601_1', 'assistant', json('[{\"text\":\"Sure, using FTS5\"}]'), 1780000005);",
            )
            .unwrap();
        }

        let dst = temp("goose_dst.db");
        let _ = std::fs::remove_file(&dst);
        let mut conn = crate::db::open(&dst).unwrap();
        let report = scan_path(&mut conn, &src, &mut || {}).unwrap();
        assert_eq!(report.threads_indexed, 1);
        assert_eq!(report.messages_indexed, 2);

        let hits =
            crate::search::search(&conn, "fts5", &crate::search::SearchFilters::default()).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.source == "goose"));

        let tid: i64 = conn
            .query_row("SELECT id FROM threads", [], |r| r.get(0))
            .unwrap();
        let detail = crate::search::thread_detail(&conn, tid).unwrap().unwrap();
        assert_eq!(detail.title.as_deref(), Some("Add FTS5 search")); // description fallback
        assert_eq!(detail.project_path.as_deref(), Some("/Users/me/proj"));
    }

    #[test]
    #[ignore]
    fn real_goose_index() {
        let mut conn = crate::db::open(&temp("goose_real.db")).unwrap();
        eprintln!("{:?}", scan(&mut conn, &mut || {}).unwrap());
    }
}
