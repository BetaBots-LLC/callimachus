//! Claude Code indexer. History lives under `~/.claude/projects/<project-slug>/`.
//! The top-level `<session-uuid>.jsonl` files are the main threads; nested
//! `<uuid>/subagents/agent-*.jsonl` files are subagent transcripts. We index all
//! of them recursively. Each file is one thread keyed by its path-relative id
//! (unique even when subagents reuse a session id). We extract user/assistant
//! text, tool calls, and tool results; `thinking` blocks are skipped.

use super::{set_file_state, source_id, upsert_thread, IndexReport, ParsedMessage, ParsedThread};
use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

pub const KIND: &str = "claude_code";

/// `~/.claude/projects`, or None if HOME is unset.
pub fn projects_root() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude").join("projects"))
}

/// Recursively collect every `.jsonl` file under `dir`.
fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
}

/// Walk every project dir, (re)indexing thread files whose mtime/size changed.
pub fn scan(conn: &mut Connection) -> Result<IndexReport> {
    let mut report = IndexReport::default();
    let Some(root) = projects_root() else {
        return Ok(report);
    };
    if !root.is_dir() {
        return Ok(report);
    }
    let sid = source_id(conn, KIND)?;

    let mut files = Vec::new();
    collect_jsonl(&root, &mut files);

    for path in files {
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

/// Index one thread file. Returns Some(message_count) if indexed, None if unchanged.
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
            return Ok(None); // unchanged
        }
    }

    // external_id = path relative to projects root: stable and unique per file.
    let rel = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string();
    // Project fallback: top-level dir name is the cwd-slug (used when a subagent
    // file carries no cwd of its own).
    let project_fallback = rel.split('/').next().map(decode_slug);

    let mut thread = parse_file(path, &rel, project_fallback.as_deref())
        .with_context(|| format!("parsing {path_str}"))?;
    if let Some(t) = thread.as_mut() {
        // Nested `<uuid>/subagents/agent-*.jsonl` files are subagent transcripts.
        t.is_subagent = rel.contains("/subagents/");
    }
    let n = if let Some(t) = thread {
        upsert_thread(conn, sid, &t)?
    } else {
        0
    };
    set_file_state(conn, &path_str, KIND, mtime, size)?;
    Ok(Some(n))
}

/// Decode a project slug (`-Users-me-proj`) back to an approximate cwd
/// (`/Users/me/proj`). Lossy when real directory names contain hyphens; the `cwd`
/// field in the file is authoritative when present.
fn decode_slug(slug: &str) -> String {
    slug.replace('-', "/")
}

/// Parse a `.jsonl` thread file. `external_id` keys the thread; `project_fallback`
/// is used when no `cwd` appears in the file. Returns None if it has no messages.
pub fn parse_file(
    path: &Path,
    external_id: &str,
    project_fallback: Option<&str>,
) -> Result<Option<ParsedThread>> {
    let content = fs::read_to_string(path)?;
    let mut thread = ParsedThread {
        external_id: external_id.to_string(),
        project_path: project_fallback.map(str::to_string),
        ..Default::default()
    };
    let mut first_user_text: Option<String> = None;

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(line) else {
            continue; // tolerate the occasional malformed line
        };
        ingest_line(&mut thread, &obj, &mut first_user_text);
    }

    if thread.messages.is_empty() {
        return Ok(None);
    }
    // Title fallback: first user message, trimmed to a reasonable length.
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

/// Fold a single JSONL line into the thread under construction.
fn ingest_line(thread: &mut ParsedThread, obj: &Value, first_user_text: &mut Option<String>) {
    // A real cwd in the file overrides the slug-derived fallback.
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

    match obj.get("type").and_then(Value::as_str) {
        Some("ai-title") => {
            if let Some(t) = obj.get("aiTitle").and_then(Value::as_str) {
                thread.title = Some(t.to_string());
            }
        }
        Some(role @ ("user" | "assistant")) => {
            let content = obj.get("message").and_then(|m| m.get("content"));
            let before = thread.messages.len();
            extract_messages(thread, role, content, ts);
            if role == "user" && first_user_text.is_none() {
                if let Some(m) = thread.messages.get(before) {
                    if m.role == "user" {
                        *first_user_text = Some(m.text.clone());
                    }
                }
            }
        }
        _ => {}
    }
}

/// Turn a message's `content` (string or array of blocks) into ParsedMessages.
fn extract_messages(
    thread: &mut ParsedThread,
    role: &str,
    content: Option<&Value>,
    ts: Option<i64>,
) {
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

    match content {
        Some(Value::String(s)) => push(thread, role, s.clone(), None),
        Some(Value::Array(blocks)) => {
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = block.get("text").and_then(Value::as_str) {
                            push(thread, role, t.to_string(), None);
                        }
                    }
                    Some("tool_use") => {
                        let name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("tool")
                            .to_string();
                        let input = block
                            .get("input")
                            .map(|v| v.to_string())
                            .unwrap_or_default();
                        push(thread, "assistant", format!("{name}: {input}"), Some(name));
                    }
                    Some("tool_result") => {
                        let text = stringify_tool_result(block.get("content"));
                        push(thread, "tool", text, None);
                    }
                    _ => {} // thinking, image, etc. — skipped for now
                }
            }
        }
        _ => {}
    }
}

/// tool_result `content` may be a plain string or an array of text blocks.
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

/// ISO-8601 / RFC-3339 timestamp -> epoch seconds.
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

    const SAMPLE: &str = r#"{"type":"ai-title","aiTitle":"Build the indexer","sessionId":"sess-abc"}
{"type":"user","sessionId":"sess-abc","cwd":"/Users/me/proj","gitBranch":"main","timestamp":"2026-06-01T10:00:00.000Z","message":{"role":"user","content":[{"type":"text","text":"index tauri threads with sqlite fts5"}]}}
{"type":"assistant","sessionId":"sess-abc","timestamp":"2026-06-01T10:00:05.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"Sure, using FTS5"},{"type":"tool_use","name":"Bash","input":{"command":"cargo build"}}]}}
{"type":"user","sessionId":"sess-abc","timestamp":"2026-06-01T10:00:06.000Z","message":{"role":"user","content":[{"type":"tool_result","content":"Finished dev profile"}]}}
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
        let path = write_sample("sample.jsonl");
        let thread = parse_file(&path, "slug/sess-abc.jsonl", Some("/slug"))
            .unwrap()
            .expect("non-empty thread");
        assert_eq!(thread.external_id, "slug/sess-abc.jsonl");
        assert_eq!(thread.title.as_deref(), Some("Build the indexer"));
        // cwd in the file overrides the slug fallback
        assert_eq!(thread.project_path.as_deref(), Some("/Users/me/proj"));
        assert_eq!(thread.git_branch.as_deref(), Some("main"));
        // user text, assistant text, assistant tool_use, tool result = 4 (thinking skipped)
        assert_eq!(thread.messages.len(), 4);
        let tool = thread
            .messages
            .iter()
            .find(|m| m.tool_name.is_some())
            .unwrap();
        assert_eq!(tool.tool_name.as_deref(), Some("Bash"));
        assert!(tool.text.contains("cargo build"));
    }

    #[test]
    fn title_falls_back_to_first_user_message() {
        // No ai-title line -> title derived from first user text.
        let path = temp_path("notitle.jsonl");
        std::fs::write(
            &path,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hello world question"}]}}
"#,
        )
        .unwrap();
        let thread = parse_file(&path, "x.jsonl", None).unwrap().unwrap();
        assert_eq!(thread.title.as_deref(), Some("hello world question"));
    }

    #[test]
    fn index_then_search_roundtrip() {
        let path = write_sample("rt.jsonl");
        let mut conn = crate::db::open(&temp_path("rt.db")).unwrap();
        let sid = source_id(&conn, KIND).unwrap();
        let thread = parse_file(&path, "slug/rt.jsonl", None).unwrap().unwrap();
        upsert_thread(&mut conn, sid, &thread).unwrap();

        // "fts5" appears in the user message and in "Sure, using FTS5" (case-insensitive).
        let hits =
            crate::search::search(&conn, "fts5", &crate::search::SearchFilters::default()).unwrap();
        assert_eq!(hits.len(), 2);
        // snippet() marks matches with the char(1) sentinel (frontend swaps it for <mark>).
        assert!(hits.iter().all(|h| h.snippet.contains('\u{1}')));
        assert!(hits.iter().all(|h| h.source == "claude_code"));

        // Re-indexing the same thread is idempotent (no duplicate messages).
        upsert_thread(&mut conn, sid, &thread).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 4, "re-upsert replaces rather than appends");
    }

    /// Real-data smoke test against the live ~/.claude history. Ignored by default;
    /// run with: `cargo test -- --ignored real_claude_index --nocapture`
    #[test]
    #[ignore]
    fn real_claude_index() {
        let mut conn = crate::db::open(&temp_path("real.db")).unwrap();
        let report = scan(&mut conn).unwrap();
        eprintln!("{report:?}");
        assert!(
            report.threads_indexed > 0,
            "indexed at least one real thread"
        );

        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert!(n > 0, "indexed real messages");

        let hits = crate::search::search(&conn, "tauri", &crate::search::SearchFilters::default())
            .unwrap();
        eprintln!("'tauri' hits: {}", hits.len());
    }
}
