//! Pack an indexed thread (or a set of search hits) into one LLM-ready context
//! blob: a markdown transcript wrapped in an XML envelope, with a token-budget
//! ladder so large threads degrade gracefully instead of blowing the window.
//!
//! Budget ladder (char-based; ~4 chars/token):
//!   1. full transcript            (if it fits)
//!   2. drop tool calls / output   (keep just user+assistant)
//!   3. head/tail elision          (keep the first/last N turns, mark the gap)
//! LLM summarization of the elided middle is a future step; elision is the floor.

use crate::search;
use anyhow::Result;
use rusqlite::Connection;

/// Default budget: ~12k tokens.
pub const DEFAULT_BUDGET_CHARS: usize = 48_000;

fn render_message(m: &search::MessageRow) -> String {
    match (&m.tool_name, m.role.as_str()) {
        (Some(tool), _) => format!("### tool: {tool}\n{}\n", m.text),
        (None, "tool") => format!("### tool result\n{}\n", m.text),
        (None, role) => format!("### {role}\n{}\n", m.text),
    }
}

fn is_tool(m: &search::MessageRow) -> bool {
    m.tool_name.is_some() || m.role == "tool"
}

fn envelope(detail: &search::ThreadDetail, body: &str, note: Option<&str>) -> String {
    let title = detail.title.as_deref().unwrap_or("Untitled");
    let project = detail.project_path.as_deref().unwrap_or("");
    let mut head = format!(
        "<reference_thread source=\"{}\" title=\"{}\" project=\"{}\">",
        detail.source,
        title.replace('"', "'"),
        project.replace('"', "'"),
    );
    if let Some(n) = note {
        head.push_str(&format!("\n<!-- {n} -->"));
    }
    format!("{head}\n{body}\n</reference_thread>")
}

/// Build a context blob for one thread under `budget_chars`.
pub fn pack_thread(
    conn: &Connection,
    thread_id: i64,
    budget_chars: usize,
) -> Result<Option<String>> {
    let Some(detail) = search::thread_detail(conn, thread_id)? else {
        return Ok(None);
    };

    // 1. Full.
    let full: String = detail
        .messages
        .iter()
        .map(render_message)
        .collect::<Vec<_>>()
        .join("\n");
    if envelope(&detail, &full, None).len() <= budget_chars {
        return Ok(Some(envelope(&detail, &full, None)));
    }

    // 2. Drop tool noise.
    let no_tools: Vec<&search::MessageRow> =
        detail.messages.iter().filter(|m| !is_tool(m)).collect();
    let no_tools_body = no_tools
        .iter()
        .map(|m| render_message(m))
        .collect::<Vec<_>>()
        .join("\n");
    if envelope(&detail, &no_tools_body, Some("tool calls/output omitted")).len() <= budget_chars {
        return Ok(Some(envelope(
            &detail,
            &no_tools_body,
            Some("tool calls/output omitted"),
        )));
    }

    // 3. Head/tail elision over the tool-free turns, with each kept turn truncated
    //    so the total stays under budget. A final hard cap guarantees the ceiling.
    let n = no_tools.len();
    let keep = 6usize;
    let (head, tail) = if n > keep * 2 {
        (&no_tools[..keep], &no_tools[n - keep..])
    } else {
        (&no_tools[..n.min(keep)], &[][..])
    };
    let per_msg = (budget_chars / (keep * 2 + 1)).max(300);
    let render_capped = |m: &search::MessageRow| {
        let mut t = m.text.clone();
        if t.chars().count() > per_msg {
            t = format!("{}…", t.chars().take(per_msg).collect::<String>());
        }
        format!("### {}\n{}\n", m.role, t)
    };
    let mut body = head
        .iter()
        .map(|m| render_capped(m))
        .collect::<Vec<_>>()
        .join("\n");
    let elided = n.saturating_sub(head.len() + tail.len());
    if elided > 0 {
        body.push_str(&format!("\n\n### … {elided} turns elided …\n\n"));
    }
    if !tail.is_empty() {
        body.push('\n');
        body.push_str(
            &tail
                .iter()
                .map(|m| render_capped(m))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    let note = format!("trimmed to fit budget; {elided} middle turns elided, tools omitted");
    let mut out = envelope(&detail, &body, Some(&note));
    if out.len() > budget_chars {
        // Final hard cap (char-safe), preserving the closing tag.
        let target = budget_chars.saturating_sub(64);
        let truncated: String = body.chars().take(target).collect();
        out = envelope(
            &detail,
            &format!("{truncated}\n… (truncated) …"),
            Some(&note),
        );
    }
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(conn: &Connection, n_turns: usize, big: bool) -> i64 {
        conn.execute(
            "INSERT INTO threads (source_id, external_id, title, project_path) VALUES (1, 'c1', 'My Thread', '/p')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        for i in 0..n_turns {
            let text = if big {
                "x".repeat(5000)
            } else {
                format!("turn {i} content")
            };
            conn.execute(
                "INSERT INTO messages (thread_id, seq, role, text) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    tid,
                    i as i64,
                    if i % 2 == 0 { "user" } else { "assistant" },
                    text
                ],
            )
            .unwrap();
        }
        // a tool message that should be dropped first under budget
        conn.execute(
            "INSERT INTO messages (thread_id, seq, role, text, tool_name) VALUES (?1, 9999, 'assistant', 'ls -la', 'Bash')",
            [tid],
        )
        .unwrap();
        tid
    }

    #[test]
    fn full_when_small() {
        let conn = crate::db::open(std::path::Path::new(":memory:")).unwrap();
        let tid = seed(&conn, 4, false);
        let out = pack_thread(&conn, tid, DEFAULT_BUDGET_CHARS)
            .unwrap()
            .unwrap();
        assert!(out.starts_with("<reference_thread"));
        assert!(out.contains("My Thread"));
        assert!(out.contains("### tool: Bash")); // tools kept when it fits
    }

    #[test]
    fn drops_tools_then_elides_when_large() {
        let conn = crate::db::open(std::path::Path::new(":memory:")).unwrap();
        let tid = seed(&conn, 40, true); // ~200k chars of body
        let out = pack_thread(&conn, tid, 30_000).unwrap().unwrap();
        assert!(out.len() <= 30_000 + 500, "len {}", out.len());
        assert!(
            !out.contains("### tool: Bash"),
            "tools should be dropped under budget"
        );
        assert!(out.contains("turns elided"));
    }
}
