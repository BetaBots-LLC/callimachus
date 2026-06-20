//! Obsidian-flavored Markdown export for a single thread: YAML frontmatter
//! (title, source, project, dates, tags) followed by role-headed message bodies.
//! Pure string generation — callers (the `cal export` CLI, later a GUI button)
//! decide where to write it.

use crate::search::ThreadDetail;

/// Render a thread as an Obsidian note: YAML frontmatter (with a `[[project]]`
/// graph link) + an optional synthesis section + the full transcript. `synthesis`,
/// when present, is the LLM-extracted decisions/gotchas/TODOs block (already
/// `##`-headed Markdown) and is placed above the transcript — the knowledge layer.
pub fn to_obsidian(detail: &ThreadDetail, synthesis: Option<&str>) -> String {
    let title = detail.title.clone().unwrap_or_else(|| format!("Thread {}", detail.id));
    let created = fmt_date(detail.created_at);
    let updated = fmt_date(detail.updated_at);
    let project = project_basename(detail.project_path.as_deref());

    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("title: {}\n", yaml_str(&title)));
    out.push_str(&format!("source: {}\n", detail.source));
    // A frontmatter wikilink so the note joins the vault graph and Properties.
    if let Some(p) = &project {
        out.push_str(&format!("project: \"[[{p}]]\"\n"));
    }
    if let Some(p) = detail.project_path.as_deref().filter(|p| !p.is_empty()) {
        out.push_str(&format!("project_path: {}\n", yaml_str(p)));
    }
    if let Some(b) = detail.git_branch.as_deref().filter(|b| !b.is_empty()) {
        out.push_str(&format!("branch: {}\n", yaml_str(b)));
    }
    if let Some(c) = &created {
        out.push_str(&format!("created: {c}\n"));
    }
    if let Some(u) = &updated {
        out.push_str(&format!("updated: {u}\n"));
    }
    out.push_str(&format!("external_id: {}\n", yaml_str(&detail.external_id)));
    out.push_str("tags:\n  - callimachus\n");
    out.push_str(&format!("  - {}\n", detail.source));
    out.push_str("---\n\n");

    out.push_str(&format!("# {title}\n\n"));

    // Breadcrumb line, leading with the project wikilink.
    let mut crumbs: Vec<String> = Vec::new();
    if let Some(p) = &project {
        crumbs.push(format!("[[{p}]]"));
    }
    crumbs.push(detail.source.clone());
    if let Some(u) = &updated {
        crumbs.push(u.clone());
    }
    out.push_str(&format!("> {} — indexed by Callimachus\n\n", crumbs.join(" · ")));

    if let Some(s) = synthesis.map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str(s);
        out.push_str("\n\n");
    }

    // Full record under a foldable heading, below the synthesized knowledge.
    out.push_str("## Transcript\n\n");
    for m in &detail.messages {
        let heading = match m.role.as_str() {
            "user" => "🧑 User".to_string(),
            "assistant" => "🤖 Assistant".to_string(),
            "tool" => match m.tool_name.as_deref() {
                Some(name) => format!("🔧 Tool · {name}"),
                None => "🔧 Tool".to_string(),
            },
            other => other.to_string(),
        };
        out.push_str(&format!("### {heading}\n\n{}\n\n", m.text.trim()));
    }

    out
}

/// Basename of a thread's project path — the `[[wikilink]]` target (e.g.
/// `/Users/me/callimachus` -> `callimachus`). None for project-less threads.
fn project_basename(project_path: Option<&str>) -> Option<String> {
    let p = project_path?.trim().trim_end_matches('/');
    if p.is_empty() {
        return None;
    }
    std::path::Path::new(p).file_name()?.to_str().map(str::to_string)
}

/// A filesystem-safe note filename (no extension), unique via the thread id.
pub fn note_filename(detail: &ThreadDetail) -> String {
    let base = detail.title.clone().unwrap_or_else(|| format!("Thread {}", detail.id));
    let safe: String = base
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\n' | '\r' | '\t') { ' ' } else { c })
        .collect();
    let safe = safe.split_whitespace().collect::<Vec<_>>().join(" ");
    let safe = if safe.chars().count() > 80 {
        safe.chars().take(80).collect::<String>()
    } else {
        safe
    };
    format!("{safe} ({} {})", detail.source, detail.id)
}

/// Quote a YAML scalar if it contains characters that would break a bare value.
fn yaml_str(s: &str) -> String {
    if s.is_empty() || s.contains([':', '#', '"', '\'', '\n']) || s.starts_with([' ', '-', '[', '{']) {
        format!("{:?}", s) // Rust debug quoting is valid YAML double-quoted form
    } else {
        s.to_string()
    }
}

fn fmt_date(epoch: Option<i64>) -> Option<String> {
    epoch
        .and_then(|e| chrono::DateTime::from_timestamp(e, 0))
        .map(|dt| dt.format("%Y-%m-%d").to_string())
}

/// Render a project's aggregated memory as the managed `.callimachus/memory.md`: a header,
/// an optional LLM brief, then decisions / gotchas / open TODOs as bullets tagged with the
/// source thread id. Pure string generation; the caller decides where to write it.
pub fn project_memory_md(
    project: &str,
    mem: &crate::knowledge::ProjectMemory,
    brief: Option<&str>,
) -> String {
    fn section(out: &mut String, title: &str, facts: &[crate::knowledge::MemoryFact]) {
        if facts.is_empty() {
            return;
        }
        out.push_str(&format!("## {title}\n\n"));
        for f in facts {
            out.push_str(&format!("- {} _(thread {})_\n", f.text.trim(), f.thread_id));
        }
        out.push('\n');
    }
    let base = project.rsplit(['/', '\\']).find(|s| !s.is_empty()).unwrap_or(project);
    let mut out = String::new();
    out.push_str(&format!("# Project memory: {base}\n\n"));
    out.push_str(&format!(
        "_Distilled by Callimachus across {} thread(s), {} analyzed. Project: `{}`._\n\n",
        mem.thread_count, mem.distilled_count, project
    ));
    if let Some(b) = brief.map(str::trim).filter(|b| !b.is_empty()) {
        out.push_str(b);
        out.push_str("\n\n");
    }
    section(&mut out, "Decisions", &mem.decisions);
    section(&mut out, "Gotchas", &mem.gotchas);
    section(&mut out, "Open TODOs", &mem.open_todos);
    if mem.decisions.is_empty() && mem.gotchas.is_empty() && mem.open_todos.is_empty() {
        out.push_str("_No distilled knowledge yet. Run Build memory in Callimachus._\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::MessageRow;

    fn detail() -> ThreadDetail {
        ThreadDetail {
            id: 7,
            source: "claude_code".into(),
            external_id: "abc/sess.jsonl".into(),
            title: Some("Add FTS5: search/index".into()),
            project_path: Some("/Users/me/proj".into()),
            git_branch: Some("main".into()),
            created_at: Some(1_780_000_000),
            updated_at: Some(1_780_000_500),
            starred: false,
            tags: vec![],
            messages: vec![
                MessageRow { id: 1, role: "user".into(), text: "how do I add fts5".into(), tool_name: None, ts: None },
                MessageRow { id: 2, role: "assistant".into(), text: "use an external-content table".into(), tool_name: None, ts: None },
                MessageRow { id: 3, role: "tool".into(), text: "ok".into(), tool_name: Some("Bash".into()), ts: None },
            ],
        }
    }

    #[test]
    fn renders_frontmatter_and_body() {
        let md = to_obsidian(&detail(), None);
        assert!(md.starts_with("---\n"));
        assert!(md.contains("source: claude_code"));
        assert!(md.contains("project: \"[[proj]]\"")); // basename -> wikilink
        assert!(md.contains("project_path: /Users/me/proj")); // full path retained
        assert!(md.contains("title: \"Add FTS5: search/index\"")); // has ':' -> quoted
        assert!(md.contains("- callimachus"));
        assert!(md.contains("> [[proj]] · claude_code")); // breadcrumb wikilink
        assert!(md.contains("## Transcript"));
        assert!(md.contains("### 🧑 User"));
        assert!(md.contains("### 🤖 Assistant"));
        assert!(md.contains("### 🔧 Tool · Bash"));
        assert!(md.contains("how do I add fts5"));
    }

    #[test]
    fn places_synthesis_above_transcript() {
        let md = to_obsidian(&detail(), Some("## Decisions\n- used external-content table"));
        let synth = md.find("## Decisions").unwrap();
        let transcript = md.find("## Transcript").unwrap();
        assert!(synth < transcript, "synthesis must sit above the transcript");
        assert!(md.contains("used external-content table"));
    }

    #[test]
    fn filename_is_sanitized_and_unique() {
        let name = note_filename(&detail());
        assert!(!name.contains('/'));
        assert!(name.contains("(claude_code 7)"));
    }
}
