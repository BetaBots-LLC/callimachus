//! Codex CLI indexer. Thread metadata lives in `~/.codex/state_5.sqlite`
//! (`threads` table), and the conversation itself in the per-thread rollout file
//! at `threads.rollout_path` (JSONL of `response_item` / `event_msg` / etc.).

use super::common::{parse_rfc3339, push_message, truncate_title};
use super::{
    file_change_state, set_file_state, source_id, upsert_thread, IndexReport, ParsedMessage,
    ParsedThread,
};
use anyhow::Result;
use rusqlite::Connection;
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

pub const KIND: &str = "codex";

static CODEX_HOME_OVERRIDE: std::sync::RwLock<Option<PathBuf>> = std::sync::RwLock::new(None);

/// Point the indexer at a relocated Codex home (a Settings value). A GUI app launched from
/// Finder/Dock never sees the shell's `CODEX_HOME`, so the app needs a way to set it
/// explicitly. `None` (or an empty path) restores the default lookup.
pub fn set_codex_home_override(dir: Option<PathBuf>) {
    if let Ok(mut slot) = CODEX_HOME_OVERRIDE.write() {
        *slot = dir
            .filter(|d| !d.as_os_str().is_empty())
            .map(super::claude::expand_tilde);
    }
}

/// `app_config` key holding a user-chosen Codex home (a Settings value).
pub const CODEX_HOME_SETTING: &str = "paths.codex_home";

/// Apply the [`CODEX_HOME_SETTING`] override from the index (cleared when unset). Called at
/// the start of every scan, so a changed setting takes effect on the next pass.
pub fn load_codex_home_setting(conn: &Connection) {
    set_codex_home_override(super::claude::read_path_setting(conn, CODEX_HOME_SETTING));
}

/// Codex's home: the override, else `$CODEX_HOME`, else `~/.codex`. The CLI, the Desktop
/// app and the IDE extension all honor `CODEX_HOME`, so the index must too.
pub fn codex_root() -> Option<PathBuf> {
    if let Some(dir) = CODEX_HOME_OVERRIDE.read().ok().and_then(|g| g.clone()) {
        return Some(dir);
    }
    if let Some(dir) = std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()) {
        return Some(super::claude::expand_tilde(PathBuf::from(dir)));
    }
    dirs::home_dir().map(|h| h.join(".codex"))
}

struct ThreadMeta {
    id: String,
    title: String,
    cwd: String,
    git_branch: Option<String>,
    created_at: Option<i64>,
    updated_at: Option<i64>,
    rollout_path: String,
    first_user_message: String,
}

/// Index Codex history. The Codex CLI, the Desktop app, and the VS Code/IDE
/// extension (`openai.chatgpt`) all share `~/.codex` (CODEX_HOME). Two passes:
///   1. the `state_5.sqlite` thread DB (authoritative title/cwd/branch/timestamps), then
///   2. any `sessions/` + `archived_sessions/` rollout JSONL not already covered.
///
/// Pass 2 exists because a session is not always registered in the thread DB the
/// way pass 1 expects: some extension builds write only the rollout file, and the
/// DB version isn't always `state_5`. The rollout file is self-describing via its
/// `session_meta` header, so it can be indexed without the DB. Dedup is by session
/// id, so a session present in both passes is indexed once (pass 1 wins).
pub fn scan(conn: &mut Connection, tick: &mut dyn FnMut()) -> Result<IndexReport> {
    load_codex_home_setting(conn);
    let Some(root) = codex_root() else {
        return Ok(IndexReport::default());
    };
    scan_in(conn, &root, tick)
}

fn scan_in(conn: &mut Connection, root: &Path, tick: &mut dyn FnMut()) -> Result<IndexReport> {
    let mut report = IndexReport::default();
    if !root.exists() {
        return Ok(report);
    }
    let sid = source_id(conn, KIND)?;
    let mut seen: HashSet<String> = HashSet::new();
    // Rollout files pass 1 already covered. The watcher re-runs this scan on every Codex
    // write, so without this pass 2 would reopen and JSON-parse every registered session's
    // header each time, only to drop it on the id check. PathBuf equality compares
    // components, so a doubled or trailing separator in the DB's spelling still matches;
    // a path spelled any other way just misses here and is deduped by id as before.
    let mut registered: HashSet<PathBuf> = HashSet::new();

    // Pass 1: thread metadata from the state DB, when present.
    let state_db = root.join("state_5.sqlite");
    if state_db.exists() {
        for meta in read_thread_metas(&state_db)? {
            tick();
            seen.insert(meta.id.clone());
            registered.insert(PathBuf::from(&meta.rollout_path));
            report.record(index_rollout(conn, sid, &meta));
        }
    }

    // Pass 2: rollout JSONL on disk not registered in the state DB.
    let mut files = Vec::new();
    collect_rollout_files(&root.join("sessions"), &mut files);
    collect_rollout_files(&root.join("archived_sessions"), &mut files);
    for f in files {
        tick();
        if registered.contains(&f) {
            continue; // indexed (or checked unchanged) by pass 1
        }
        let Some(meta) = meta_from_rollout(&f) else {
            continue; // no readable session_meta header
        };
        if meta.id.is_empty() || !seen.insert(meta.id.clone()) {
            continue; // missing id, or already indexed via the state DB
        }
        report.record(index_rollout(conn, sid, &meta));
    }
    Ok(report)
}

fn read_thread_metas(state_db: &Path) -> Result<Vec<ThreadMeta>> {
    let ro = super::open_external_readonly(state_db)?;
    let mut stmt = ro.prepare(
        "SELECT id, title, cwd, git_branch, created_at, updated_at, rollout_path, first_user_message
         FROM threads",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(ThreadMeta {
            id: r.get(0)?,
            title: r.get(1)?,
            cwd: r.get(2)?,
            git_branch: r.get::<_, Option<String>>(3)?,
            created_at: r.get::<_, Option<i64>>(4)?,
            updated_at: r.get::<_, Option<i64>>(5)?,
            rollout_path: r.get(6)?,
            first_user_message: r.get::<_, Option<String>>(7)?.unwrap_or_default(),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn index_rollout(conn: &mut Connection, sid: i64, meta: &ThreadMeta) -> Result<Option<usize>> {
    let path = Path::new(&meta.rollout_path);
    if !path.exists() {
        return Ok(None);
    }
    let (unchanged, mtime, size) = file_change_state(conn, path)?;
    if unchanged {
        return Ok(None);
    }
    // The user deleted this thread: don't bring it back (and don't pay to parse it).
    if crate::cleanup::is_tombstoned(conn, sid, &meta.id)? {
        set_file_state(conn, &meta.rollout_path, KIND, mtime, size)?;
        return Ok(None);
    }

    let messages = parse_rollout(path)?;
    // `codex exec` runs the app's own distill / Ask calls (codex-cli engine) and always
    // writes a rollout; those are not the user's history.
    let cwd = (!meta.cwd.is_empty()).then_some(meta.cwd.as_str());
    let user_texts = messages
        .iter()
        .filter(|m| m.role == "user")
        .take(3)
        .map(|m| m.text.as_str());
    if crate::agent::is_app_cli_session(cwd, user_texts) {
        set_file_state(conn, &meta.rollout_path, KIND, mtime, size)?;
        return Ok(None);
    }
    let title = if !meta.title.trim().is_empty() {
        Some(meta.title.clone())
    } else if !meta.first_user_message.trim().is_empty() {
        first_line(&meta.first_user_message)
    } else {
        // Pass-2 sessions carry no state-DB metadata: derive a title from the
        // first user message in the rollout itself.
        messages
            .iter()
            .find(|m| m.role == "user")
            .and_then(|m| first_line(&m.text))
    };
    let thread = ParsedThread {
        external_id: meta.id.clone(),
        title,
        project_path: (!meta.cwd.is_empty()).then(|| meta.cwd.clone()),
        git_branch: meta.git_branch.clone().filter(|b| !b.is_empty()),
        created_at: meta.created_at,
        updated_at: meta.updated_at,
        is_subagent: false,
        usage: Vec::new(),
        messages,
    };
    let n = upsert_thread(conn, sid, &thread)?;
    set_file_state(conn, &meta.rollout_path, KIND, mtime, size)?;
    Ok(Some(n))
}

/// Recursively collect `rollout-*.jsonl` files under `dir` (handles the
/// `sessions/YYYY/MM/DD/` partitioning).
fn collect_rollout_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rollout_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl")
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("rollout-"))
        {
            out.push(path);
        }
    }
}

/// Build a `ThreadMeta` from a rollout file's `session_meta` header (its first
/// line), for sessions not present in the state DB. Reads ONLY the first line, so
/// it stays cheap even for large rollouts and for files that will be deduped away.
fn meta_from_rollout(path: &Path) -> Option<ThreadMeta> {
    let file = fs::File::open(path).ok()?;
    let mut first = String::new();
    BufReader::new(file).read_line(&mut first).ok()?;
    let obj: Value = serde_json::from_str(first.trim()).ok()?;
    if obj.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let p = obj.get("payload")?;
    let id = p.get("id").and_then(Value::as_str)?.to_string();
    let cwd = p
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let created_at = p
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339)
        .or_else(|| {
            obj.get("timestamp")
                .and_then(Value::as_str)
                .and_then(parse_rfc3339)
        });
    let updated_at = fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .or(created_at);
    Some(ThreadMeta {
        id,
        title: String::new(),
        cwd,
        git_branch: None,
        created_at,
        updated_at,
        rollout_path: path.to_string_lossy().to_string(),
        first_user_message: String::new(),
    })
}

/// Parse a rollout JSONL file into ordered messages.
fn parse_rollout(path: &Path) -> Result<Vec<ParsedMessage>> {
    let content = fs::read_to_string(path)?;
    let mut out = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if obj.get("type").and_then(Value::as_str) != Some("response_item") {
            continue;
        }
        let ts = obj
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339);
        let Some(payload) = obj.get("payload") else {
            continue;
        };
        ingest_payload(&mut out, payload, ts);
    }
    Ok(out)
}

fn ingest_payload(out: &mut Vec<ParsedMessage>, payload: &Value, ts: Option<i64>) {
    match payload.get("type").and_then(Value::as_str) {
        Some("message") => {
            let role = match payload.get("role").and_then(Value::as_str) {
                Some("assistant") => "assistant",
                Some("user") => "user",
                _ => "system", // developer / system instructions
            };
            push_message(out, role, &content_text(payload.get("content")), None, ts);
        }
        Some("function_call") => {
            let name = payload
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string();
            let args = payload
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            push_message(out, "assistant", &format!("{name}: {args}"), Some(name), ts);
        }
        Some("function_call_output") => {
            let output = payload.get("output").map(value_to_text).unwrap_or_default();
            // Same index-time cap as Claude tool results (see `claude::cap_tool_text`).
            let output = super::claude::cap_tool_text(output.trim());
            push_message(out, "tool", &output, None, ts);
        }
        _ => {} // reasoning, etc. — skipped
    }
}

/// Join the text of a message's content blocks (input_text / output_text / text).
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| match b.get("type").and_then(Value::as_str) {
                Some("input_text" | "output_text" | "text") => {
                    b.get("text").and_then(Value::as_str).map(str::to_string)
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Best-effort stringification of a JSON value (for tool output).
fn value_to_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A title from the first line of `s`, or None when `s` is blank.
fn first_line(s: &str) -> Option<String> {
    let line = s.trim().lines().next()?;
    Some(truncate_title(line))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parses_rollout_messages() {
        let mut path = std::env::temp_dir();
        path.push(format!("callimachus_codex_{}.jsonl", std::process::id()));
        let sample = r#"{"timestamp":"2026-04-25T22:09:38.623Z","type":"session_meta","payload":{"id":"t1","cwd":"/proj"}}
{"timestamp":"2026-04-25T22:09:39.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"fix the warranty audit"}]}}
{"timestamp":"2026-04-25T22:09:40.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"On it"}]}}
{"timestamp":"2026-04-25T22:09:41.000Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"cmd\":\"ls\"}"}}
"#;
        std::fs::File::create(&path)
            .unwrap()
            .write_all(sample.as_bytes())
            .unwrap();

        let msgs = parse_rollout(&path).unwrap();
        assert_eq!(msgs.len(), 3); // user, assistant, function_call (session_meta ignored)
        assert_eq!(msgs[0].role, "user");
        assert!(msgs[0].text.contains("warranty"));
        assert_eq!(msgs[2].tool_name.as_deref(), Some("shell"));
    }

    /// Pass 2: a rollout file on disk with NO state DB still gets indexed (covers
    /// the VS Code extension / state-version-skew case), with title derived from
    /// the first user message and project from the session_meta cwd.
    #[test]
    fn indexes_rollout_without_state_db() {
        use std::fs::create_dir_all;
        let root = std::env::temp_dir().join(format!("calli_codex_root_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let day = root.join("sessions/2026/04/25");
        create_dir_all(&day).unwrap();
        let roll =
            day.join("rollout-2026-04-25T15-09-15-019dc6b0-998d-7a31-a39d-1229e6b38072.jsonl");
        let sample = r#"{"timestamp":"2026-04-25T22:09:38.623Z","type":"session_meta","payload":{"id":"019dc6b0-998d-7a31-a39d-1229e6b38072","cwd":"/Users/me/proj","source":"vscode"}}
{"timestamp":"2026-04-25T22:09:39.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"index my codex vscode session"}]}}
{"timestamp":"2026-04-25T22:09:40.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}}
"#;
        std::fs::write(&roll, sample).unwrap();

        let dbp = std::env::temp_dir().join(format!("calli_codex_db_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&dbp);
        let mut conn = crate::db::open(&dbp).unwrap();

        let report = scan_in(&mut conn, &root, &mut || {}).unwrap();
        assert_eq!(report.threads_indexed, 1);
        assert_eq!(report.messages_indexed, 2);

        let tid: i64 = conn
            .query_row("SELECT id FROM threads", [], |r| r.get(0))
            .unwrap();
        let detail = crate::search::thread_detail(&conn, tid).unwrap().unwrap();
        assert!(detail
            .title
            .as_deref()
            .unwrap_or_default()
            .contains("codex vscode"));
        assert_eq!(detail.project_path.as_deref(), Some("/Users/me/proj"));

        // Re-scan is a no-op (rollout fingerprint unchanged).
        let again = scan_in(&mut conn, &root, &mut || {}).unwrap();
        assert_eq!(again.threads_indexed, 0);
    }

    #[test]
    fn caps_tool_output_and_skips_the_apps_own_sessions() {
        let root = std::env::temp_dir().join(format!("calli_codex_cap_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let day = root.join("sessions/2026/05/01");
        std::fs::create_dir_all(&day).unwrap();
        let big = "x".repeat(40_000);
        let user_session = serde_json::json!([
            {"type":"session_meta","payload":{"id":"user-1","cwd":"/Users/me/proj"}},
            {"type":"response_item","payload":{"type":"message","role":"user",
                "content":[{"type":"input_text","text":"run the tests"}]}},
            {"type":"response_item","payload":{"type":"function_call_output","output": big}}
        ]);
        let app_cwd = crate::agent::cli_work_dir_path();
        let app_session = serde_json::json!([
            {"type":"session_meta","payload":{"id":"app-1","cwd": app_cwd.to_string_lossy()}},
            {"type":"response_item","payload":{"type":"message","role":"user",
                "content":[{"type":"input_text","text":"distill this"}]}}
        ]);
        for (name, lines) in [
            ("rollout-a.jsonl", user_session),
            ("rollout-b.jsonl", app_session),
        ] {
            let body: String = lines
                .as_array()
                .unwrap()
                .iter()
                .map(|l| format!("{l}\n"))
                .collect();
            std::fs::write(day.join(name), body).unwrap();
        }

        let dbp = std::env::temp_dir().join(format!("calli_codex_cap_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&dbp);
        let mut conn = crate::db::open(&dbp).unwrap();
        let report = scan_in(&mut conn, &root, &mut || {}).unwrap();
        assert_eq!(
            report.threads_indexed, 1,
            "only the user's session is indexed"
        );
        let tool: String = conn
            .query_row("SELECT text FROM messages WHERE role = 'tool'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(tool.chars().count() <= super::super::claude::TOOL_TEXT_CAP_CHARS);
    }

    /// Pass 2 skips rollouts pass 1 registered before reading their headers: the watcher
    /// re-runs this scan on every Codex write. A registered file whose header id differs
    /// from its state-DB id (so the id check alone would not catch it) is indexed once.
    #[test]
    fn pass_two_skips_rollouts_the_state_db_registered() {
        let root = std::env::temp_dir().join(format!("calli_codex_reg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let day = root.join("sessions/2026/05/02");
        std::fs::create_dir_all(&day).unwrap();
        let rollout = |id: &str, text: &str| {
            format!(
                "{}\n{}\n",
                serde_json::json!({"type":"session_meta","payload":{"id": id,"cwd":"/p"}}),
                serde_json::json!({"type":"response_item","payload":{"type":"message",
                    "role":"user","content":[{"type":"input_text","text": text}]}}),
            )
        };
        let registered = day.join("rollout-registered.jsonl");
        std::fs::write(&registered, rollout("header-id", "registered session")).unwrap();
        std::fs::write(
            day.join("rollout-loose.jsonl"),
            rollout("loose-id", "loose session"),
        )
        .unwrap();
        let state = Connection::open(root.join("state_5.sqlite")).unwrap();
        state
            .execute_batch(
                "CREATE TABLE threads (id TEXT, title TEXT, cwd TEXT, git_branch TEXT,
                    created_at INTEGER, updated_at INTEGER, rollout_path TEXT,
                    first_user_message TEXT)",
            )
            .unwrap();
        state
            .execute(
                "INSERT INTO threads VALUES ('db-id', 'From the DB', '/p', NULL, 1, 2, ?1, NULL)",
                [registered.to_string_lossy()],
            )
            .unwrap();
        drop(state);

        let dbp = std::env::temp_dir().join(format!("calli_codex_reg_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&dbp);
        let mut conn = crate::db::open(&dbp).unwrap();
        let report = scan_in(&mut conn, &root, &mut || {}).unwrap();
        assert_eq!(
            report.threads_indexed, 2,
            "the registered and the loose session"
        );
        let mut ids: Vec<String> = conn
            .prepare("SELECT external_id FROM threads ORDER BY external_id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        ids.sort();
        assert_eq!(
            ids,
            vec!["db-id", "loose-id"],
            "no second copy under the header id"
        );
    }

    /// Real-data smoke test. Run with `cargo test -- --ignored real_codex_index --nocapture`.
    #[test]
    #[ignore]
    fn real_codex_index() {
        let mut p = std::env::temp_dir();
        p.push(format!("callimachus_codex_real_{}.db", std::process::id()));
        let mut conn = crate::db::open(&p).unwrap();
        let report = scan(&mut conn, &mut || {}).unwrap();
        eprintln!("{report:?}");
    }
}
