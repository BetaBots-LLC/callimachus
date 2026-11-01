//! Small parsing helpers shared by the source indexers. Each one used to be copied into
//! several indexers, and the copies had started to drift.

use super::ParsedMessage;
use serde_json::Value;
use std::path::Path;

/// Longest title derived from message text, in chars (an ellipsis marks the cut).
const TITLE_CHARS: usize = 80;

/// A thread title from free text (usually the first user message): trimmed, and cut at
/// [`TITLE_CHARS`] with an ellipsis.
pub fn truncate_title(s: &str) -> String {
    let s = s.trim();
    // Byte length bounds char count, so short titles skip the char walk.
    if s.len() <= TITLE_CHARS || s.chars().count() <= TITLE_CHARS {
        return s.to_string();
    }
    let mut out: String = s.chars().take(TITLE_CHARS).collect();
    out.push('…');
    out
}

/// An RFC 3339 / ISO 8601 timestamp as epoch seconds.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.timestamp())
}

/// The text of a content value that is either a plain string or an array of `{text}`
/// parts (Anthropic tool results, Continue messages), with parts joined by newlines.
/// Parts without text (images, tool calls) are dropped.
pub fn join_text_parts(content: Option<&Value>) -> String {
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

/// A JSON file's parsed contents, or None when it is missing, unreadable or malformed.
pub fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Append a message with `text` trimmed, skipping it when nothing is left: an empty row
/// would only add noise to search and the incremental prefix check.
pub fn push_message(
    out: &mut Vec<ParsedMessage>,
    role: &str,
    text: &str,
    tool_name: Option<String>,
    ts: Option<i64>,
) {
    let text = text.trim();
    if !text.is_empty() {
        out.push(ParsedMessage {
            role: role.to_string(),
            text: text.to_string(),
            tool_name,
            ts,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_are_trimmed_and_cut_on_char_boundaries() {
        assert_eq!(truncate_title("  fix the build \n"), "fix the build");
        let exact = "a".repeat(TITLE_CHARS);
        assert_eq!(truncate_title(&exact), exact, "at the limit: kept whole");
        let long = "é".repeat(TITLE_CHARS + 5);
        let t = truncate_title(&long);
        assert_eq!(t.chars().count(), TITLE_CHARS + 1);
        assert!(t.ends_with('…'));
        // Multi-byte text under the char limit but over it in bytes is kept whole.
        let wide = "é".repeat(TITLE_CHARS);
        assert_eq!(truncate_title(&wide), wide);
    }

    #[test]
    fn parses_rfc3339_with_offsets() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:10Z"), Some(10));
        assert_eq!(parse_rfc3339("1970-01-01T01:00:10+01:00"), Some(10));
        assert_eq!(parse_rfc3339("yesterday"), None);
    }

    #[test]
    fn joins_string_or_text_parts() {
        let v =
            serde_json::json!([{"type": "text", "text": "a"}, {"type": "image"}, {"text": "b"}]);
        assert_eq!(join_text_parts(Some(&v)), "a\nb");
        assert_eq!(join_text_parts(Some(&serde_json::json!("plain"))), "plain");
        assert_eq!(join_text_parts(Some(&serde_json::json!(3))), "");
        assert_eq!(join_text_parts(None), "");
    }

    #[test]
    fn push_message_trims_and_drops_blank_text() {
        let mut out = Vec::new();
        push_message(&mut out, "user", "  hi  ", None, Some(1));
        push_message(&mut out, "user", " \n ", None, None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "hi");
        assert_eq!(out[0].ts, Some(1));
    }
}
