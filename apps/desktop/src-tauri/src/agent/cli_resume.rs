//! CLI launchers for indexed threads: "Resume" relaunches the original agent on
//! its native session (Claude Code / Codex), while "Open in <agent>" seeds a fresh
//! session in any agent CLI with the packed transcript. We derive the command from
//! the thread's source + external id, then open it in the user's terminal (macOS
//! Terminal via osascript) so the interactive TUI runs there.

use anyhow::{bail, Result};

/// A resume invocation: the program, its args, and the working dir to run it in.
#[derive(Debug, PartialEq)]
pub struct ResumeCommand {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
}

/// Derive the resume command for a thread. Errors for sources that have no CLI
/// (Cursor is an editor; in_app chats live in this app).
pub fn resume_command(
    source: &str,
    external_id: &str,
    is_subagent: bool,
    project_path: Option<&str>,
) -> Result<ResumeCommand> {
    match source {
        "claude_code" => {
            let session = claude_session_id(external_id, is_subagent);
            Ok(ResumeCommand {
                program: "claude".into(),
                args: vec!["--resume".into(), session],
                cwd: project_path.map(str::to_string),
            })
        }
        "codex" => Ok(ResumeCommand {
            program: "codex".into(),
            args: vec!["resume".into(), external_id.to_string()],
            cwd: project_path.map(str::to_string),
        }),
        "cursor" => bail!("Cursor has no resumable CLI — open the thread in Cursor instead"),
        "in_app" => bail!("This is an in-app chat; continue it in the Chat tab"),
        other => bail!("unknown source: {other}"),
    }
}

/// Claude Code external_id is a path relative to ~/.claude/projects:
///   top-level:  "<slug>/<session-uuid>.jsonl"      -> session is the file stem
///   subagent:   "<slug>/<session-uuid>/subagents/agent-*.jsonl" -> session is the dir uuid
fn claude_session_id(external_id: &str, is_subagent: bool) -> String {
    let parts: Vec<&str> = external_id.split('/').collect();
    if is_subagent {
        // The second segment is the parent session uuid.
        parts.get(1).map(|s| s.to_string()).unwrap_or_default()
    } else {
        parts
            .last()
            .map(|f| f.strip_suffix(".jsonl").unwrap_or(f).to_string())
            .unwrap_or_default()
    }
}

/// Launch the resume command in the user's terminal (macOS).
pub fn launch(
    source: &str,
    external_id: &str,
    is_subagent: bool,
    project_path: Option<&str>,
) -> Result<()> {
    let cmd = resume_command(source, external_id, is_subagent, project_path)?;
    #[cfg(target_os = "macos")]
    {
        let shell = build_shell_command(&cmd);
        let script = format!(
            "tell application \"Terminal\"\n  do script \"{}\"\n  activate\nend tell",
            applescript_escape(&shell)
        );
        std::process::Command::new("osascript")
            .arg("-e")
            .arg(script)
            .spawn()?;
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = cmd;
        bail!("Launching the CLI is currently supported on macOS only")
    }
}

/// Write packed context to a file and open it in a fresh CLI agent session.
/// Works for any source (unlike resume) since it just feeds context.
pub fn launch_with_context(
    program: &str,
    context_md: &str,
    project_path: Option<&str>,
) -> Result<String> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("could not resolve home directory"))?;
    let dir = home.join(".callimachus").join("context");
    std::fs::create_dir_all(&dir)?;
    let ts = chrono::Utc::now().timestamp_millis();
    let file = dir.join(format!("ctx-{ts}.md"));
    std::fs::write(&file, context_md)?;
    let file_str = file.to_string_lossy().to_string();

    let prompt =
        format!("Read the conversation context in @{file_str} and help me continue from it.");
    let cmd = ResumeCommand {
        program: program.to_string(),
        args: vec![prompt],
        cwd: project_path.map(str::to_string),
    };
    #[cfg(target_os = "macos")]
    {
        let shell = build_shell_command(&cmd);
        let script = format!(
            "tell application \"Terminal\"\n  do script \"{}\"\n  activate\nend tell",
            applescript_escape(&shell)
        );
        std::process::Command::new("osascript")
            .arg("-e")
            .arg(script)
            .spawn()?;
        Ok(file_str)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (cmd, file_str);
        bail!("Launching the CLI is currently supported on macOS only")
    }
}

/// Build the `cd <cwd> && <program> <args…>` shell line.
#[cfg(target_os = "macos")]
fn build_shell_command(cmd: &ResumeCommand) -> String {
    let mut parts = Vec::new();
    if let Some(cwd) = &cmd.cwd {
        parts.push(format!("cd {}", shell_quote(cwd)));
    }
    let mut invocation = shell_quote(&cmd.program);
    for a in &cmd.args {
        invocation.push(' ');
        invocation.push_str(&shell_quote(a));
    }
    parts.push(invocation);
    parts.join(" && ")
}

/// Single-quote a value for the shell, escaping embedded single quotes.
#[cfg(target_os = "macos")]
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Escape a string for embedding inside an AppleScript double-quoted literal.
#[cfg(target_os = "macos")]
fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_top_level_session() {
        let c = resume_command(
            "claude_code",
            "-Users-me-proj/abc-123.jsonl",
            false,
            Some("/Users/me/proj"),
        )
        .unwrap();
        assert_eq!(c.program, "claude");
        assert_eq!(c.args, vec!["--resume", "abc-123"]);
        assert_eq!(c.cwd.as_deref(), Some("/Users/me/proj"));
    }

    #[test]
    fn claude_subagent_uses_parent_session() {
        let c = resume_command(
            "claude_code",
            "-Users-me-proj/sess-uuid/subagents/agent-x.jsonl",
            true,
            None,
        )
        .unwrap();
        assert_eq!(c.args, vec!["--resume", "sess-uuid"]);
    }

    #[test]
    fn codex_uses_thread_id() {
        let c = resume_command("codex", "019dc6b0-thread", false, Some("/p")).unwrap();
        assert_eq!(c.program, "codex");
        assert_eq!(c.args, vec!["resume", "019dc6b0-thread"]);
    }

    #[test]
    fn cursor_and_in_app_unsupported() {
        assert!(resume_command("cursor", "x", false, None).is_err());
        assert!(resume_command("in_app", "x", false, None).is_err());
    }

    #[test]
    fn shell_command_quotes_paths_with_spaces() {
        let cmd = ResumeCommand {
            program: "claude".into(),
            args: vec!["--resume".into(), "abc".into()],
            cwd: Some("/Users/me/my project".into()),
        };
        let shell = build_shell_command(&cmd);
        assert_eq!(
            shell,
            "cd '/Users/me/my project' && 'claude' '--resume' 'abc'"
        );
    }

    #[test]
    fn applescript_escapes_quotes() {
        assert_eq!(applescript_escape(r#"a "b" \c"#), r#"a \"b\" \\c"#);
    }
}
