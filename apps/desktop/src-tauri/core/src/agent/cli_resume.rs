//! CLI launchers for indexed threads: "Resume" relaunches the original agent on
//! its native session (Claude Code / Codex / the Cursor CLI), while "Open in <agent>" seeds a fresh
//! session in any agent CLI with the packed transcript. We derive the command from
//! the thread's source + external id, then open it in the user's terminal so the
//! interactive TUI runs there: on macOS the terminal picked in Settings (Terminal.app unless
//! another is chosen and still installed; see [`MAC_TERMINALS`]), a new console
//! window (`cmd /C start`) on Windows, and the first terminal emulator found
//! (`$TERMINAL`, `x-terminal-emulator`, GNOME Terminal, Konsole, ...) on Linux.

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

/// Set in a session launched with a context file that already carries a memory block, so
/// `cal hook` skips its SessionStart injection (one copy, not two). Scoped to the agent
/// command itself: a terminal shell that stays open afterwards must not keep it, or every
/// later fresh session started there would silently get no memory.
pub const SKIP_HOOK_ENV: &str = "CALLIMACHUS_SKIP_HOOK";

/// A resume invocation: the program, its args, and the working dir to run it in.
#[derive(Debug, PartialEq)]
pub struct ResumeCommand {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
}

/// Derive the resume command for a thread. `cursor_cli` says a Cursor thread came from the
/// Cursor CLI's chat store or an agent transcript (`indexer::cursor::is_cli_thread`), which
/// `cursor-agent --resume <id>` reopens; an editor composer has no CLI session to resume.
/// Errors for sources with no resumable CLI (in_app chats live in this app).
pub fn resume_command(
    source: &str,
    external_id: &str,
    is_subagent: bool,
    project_path: Option<&str>,
    cursor_cli: bool,
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
        "cursor" if cursor_cli => Ok(ResumeCommand {
            program: "cursor-agent".into(),
            args: vec!["--resume".into(), external_id.to_string()],
            cwd: project_path.map(str::to_string),
        }),
        "cursor" => bail!(
            "This thread is an editor chat, which the Cursor CLI can't resume. Open it in Cursor instead."
        ),
        "in_app" => bail!("This is an in-app chat; continue it in the Chat tab"),
        other => bail!("unknown source: {other}"),
    }
}

/// Claude Code external_id is a path relative to ~/.claude/projects:
///   top-level:  "<slug>/<session-uuid>.jsonl"      -> session is the file stem
///   subagent:   "<slug>/<session-uuid>/subagents/agent-*.jsonl" -> session is the dir uuid
///
/// Ids are written with '/' (migration 0028 normalized older Windows rows), but a
/// backslash is accepted too so a row indexed by an older Windows build still resumes.
fn claude_session_id(external_id: &str, is_subagent: bool) -> String {
    let parts: Vec<&str> = external_id.split(['/', '\\']).collect();
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

/// Launch the resume command in the user's terminal. `terminal` is the stored choice
/// ([`TERMINAL_KEY`]); `None` is the platform's default.
pub fn launch(
    source: &str,
    external_id: &str,
    is_subagent: bool,
    project_path: Option<&str>,
    cursor_cli: bool,
    terminal: Option<&str>,
) -> Result<()> {
    let cmd = resume_command(source, external_id, is_subagent, project_path, cursor_cli)?;
    // A native resume fires SessionStart with source=resume, which `cal hook` already
    // leaves alone, so no skip flag is needed.
    open_in_terminal(&cmd, false, terminal)
}

/// Write packed context to `~/.callimachus/context/ctx-<ms>.md` and return its path.
pub fn write_context_file(context_md: &str) -> Result<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("could not resolve home directory"))?;
    let dir = home.join(".callimachus").join("context");
    std::fs::create_dir_all(&dir)?;
    let ts = chrono::Utc::now().timestamp_millis();
    let file = dir.join(format!("ctx-{ts}.md"));
    std::fs::write(&file, context_md)?;
    Ok(file)
}

/// The first prompt for a session seeded with a context file.
pub fn context_prompt(file: &Path) -> String {
    format!(
        "Read the conversation context in @{} and help me continue from it.",
        file.display()
    )
}

/// Write packed context to a file and open it in a fresh CLI agent session.
/// Works for any source (unlike resume) since it just feeds context. For context that
/// carries a memory block (a snapshot), so the SessionStart hook is skipped; see
/// [`launch_with_context_memory`] when that depends.
pub fn launch_with_context(
    program: &str,
    context_md: &str,
    project_path: Option<&str>,
    terminal: Option<&str>,
) -> Result<String> {
    launch_with_context_memory(program, context_md, project_path, true, terminal)
}

/// [`launch_with_context`], skipping the SessionStart hook's memory injection only when
/// `has_memory_block` says `context_md` already carries one. Otherwise the hook still
/// injects, so the session never starts with no memory at all.
pub fn launch_with_context_memory(
    program: &str,
    context_md: &str,
    project_path: Option<&str>,
    has_memory_block: bool,
    terminal: Option<&str>,
) -> Result<String> {
    let file = write_context_file(context_md)?;
    let cmd = ResumeCommand {
        program: program.to_string(),
        args: vec![context_prompt(&file)],
        cwd: project_path.map(str::to_string),
    };
    open_in_terminal(&cmd, has_memory_block, terminal)?;
    Ok(file.to_string_lossy().into_owned())
}

/// Whether `launch` / `launch_with_context` can open a terminal here.
pub fn launch_supported() -> bool {
    if cfg!(any(target_os = "macos", windows)) {
        return true;
    }
    linux_terminals().iter().any(|(term, _)| on_path(term))
}

/// A working dir to launch in, if it still exists. A moved or deleted project would
/// otherwise abort the launch (`cd` fails, or the spawn itself errors); the agent then
/// starts in the terminal's default directory instead.
fn existing_dir(cwd: Option<&str>) -> Option<&str> {
    cwd.filter(|d| Path::new(d).is_dir())
}

/// `cmd` with its working dir dropped when that dir no longer exists.
#[cfg(unix)]
fn without_missing_cwd(cmd: &ResumeCommand) -> ResumeCommand {
    ResumeCommand {
        program: cmd.program.clone(),
        args: cmd.args.clone(),
        cwd: existing_dir(cmd.cwd.as_deref()).map(str::to_string),
    }
}

/// `app_config` key of the terminal Resume and "Open in CLI" launch in: an id from
/// [`MAC_TERMINALS`], or absent for the default.
pub const TERMINAL_KEY: &str = "cli.terminal";

/// How a macOS terminal is told to run a command in a new window.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(not(unix), allow(dead_code))]
enum MacLaunch {
    /// Terminal.app: AppleScript `do script`.
    TerminalScript,
    /// iTerm2: AppleScript, a new window that types the command into its shell.
    ItermScript,
    /// `open -na <App> --args <these> <shell> -lic <command>`: the emulators that take the
    /// command to run as trailing arguments.
    OpenArgs(&'static [&'static str]),
}

/// A terminal a macOS user can pick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MacTerminal {
    /// Stored under [`TERMINAL_KEY`].
    pub id: &'static str,
    pub label: &'static str,
    /// The bundle name `open -a` and AppleScript address, and the `.app` looked for.
    app: &'static str,
    launch: MacLaunch,
}

/// The terminals offered on macOS, Terminal.app (the default, always present) first. The
/// flags are each emulator's own, as on Linux ([`KNOWN_TERMINALS`]): Ghostty and Alacritty
/// take `-e`, WezTerm a `start --` subcommand, kitty the command bare. Verified by launching:
/// Terminal and Ghostty 1.4. iTerm2, WezTerm, kitty and Alacritty follow their documentation.
/// Warp is left out: it has no way to open a window on a command.
pub const MAC_TERMINALS: &[MacTerminal] = &[
    MacTerminal {
        id: "terminal",
        label: "Terminal",
        app: "Terminal",
        launch: MacLaunch::TerminalScript,
    },
    MacTerminal {
        id: "ghostty",
        label: "Ghostty",
        app: "Ghostty",
        launch: MacLaunch::OpenArgs(&["-e"]),
    },
    MacTerminal {
        id: "iterm",
        label: "iTerm2",
        app: "iTerm",
        launch: MacLaunch::ItermScript,
    },
    MacTerminal {
        id: "wezterm",
        label: "WezTerm",
        app: "WezTerm",
        launch: MacLaunch::OpenArgs(&["start", "--"]),
    },
    MacTerminal {
        id: "kitty",
        label: "kitty",
        app: "kitty",
        launch: MacLaunch::OpenArgs(&[]),
    },
    MacTerminal {
        id: "alacritty",
        label: "Alacritty",
        app: "Alacritty",
        launch: MacLaunch::OpenArgs(&["-e"]),
    },
];

/// Whether `id` names a terminal that can be stored under [`TERMINAL_KEY`].
pub fn is_known_terminal(id: &str) -> bool {
    MAC_TERMINALS.iter().any(|t| t.id == id)
}

/// The terminals installed on this Mac, for the Settings picker; empty elsewhere (Linux
/// reads `$TERMINAL`, Windows opens a console). Terminal.app is always listed.
pub fn installed_terminals() -> Vec<&'static MacTerminal> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    let home = dirs::home_dir();
    installed_terminals_with(|app| {
        let bundle = format!("{app}.app");
        [
            Some(PathBuf::from("/Applications")),
            Some(PathBuf::from("/System/Applications/Utilities")),
            home.as_ref().map(|h| h.join("Applications")),
        ]
        .into_iter()
        .flatten()
        .any(|dir| dir.join(&bundle).is_dir())
    })
}

/// [`installed_terminals`] with the "is this app here" probe injected.
fn installed_terminals_with(has_app: impl Fn(&str) -> bool) -> Vec<&'static MacTerminal> {
    MAC_TERMINALS
        .iter()
        .filter(|t| t.launch == MacLaunch::TerminalScript || has_app(t.app))
        .collect()
}

/// The terminal to launch in: the stored choice while it is still installed, else
/// Terminal.app. A choice that was uninstalled must not turn Resume into an error.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn pick_terminal(choice: Option<&str>, installed: &[&'static MacTerminal]) -> &'static MacTerminal {
    choice
        .and_then(|id| installed.iter().copied().find(|t| t.id == id))
        .unwrap_or(&MAC_TERMINALS[0])
}

/// The process that opens `shell` (a POSIX line from [`build_shell_command`]) in a new
/// window of `terminal`: the program and its argv. `user_shell` is the login shell the
/// `OpenArgs` emulators run it in.
#[cfg(unix)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn mac_launch_argv(terminal: &MacTerminal, shell: &str, user_shell: &str) -> (String, Vec<String>) {
    let osascript = |script: String| ("osascript".to_string(), vec!["-e".to_string(), script]);
    match terminal.launch {
        MacLaunch::TerminalScript => osascript(format!(
            "tell application \"Terminal\"\n  do script \"{}\"\n  activate\nend tell",
            applescript_escape(shell)
        )),
        // Typed into the new window's own shell (not passed as its command), so the window
        // stays open on a shell when the agent exits, as Terminal's `do script` does.
        MacLaunch::ItermScript => osascript(format!(
            "tell application \"iTerm\"\n  activate\n  set w to (create window with default profile)\n  tell current session of w to write text \"{}\"\nend tell",
            applescript_escape(shell)
        )),
        // These run the command instead of a shell, so it gets one: a login, interactive
        // shell (the agent CLIs are on the PATH the user's rc files set, which a bare `sh`
        // launched from the Dock's environment does not have), which then stays open on a
        // login shell so the agent's last output remains readable.
        MacLaunch::OpenArgs(flags) => {
            let mut args = vec![
                "-na".to_string(),
                terminal.app.to_string(),
                "--args".to_string(),
            ];
            args.extend(flags.iter().map(|f| f.to_string()));
            args.extend([
                user_shell.to_string(),
                "-lic".to_string(),
                format!("{shell}; exec {} -l", shell_quote(user_shell)),
            ]);
            ("open".to_string(), args)
        }
    }
}

#[cfg(target_os = "macos")]
fn open_in_terminal(cmd: &ResumeCommand, skip_hook: bool, terminal: Option<&str>) -> Result<()> {
    let shell = build_shell_command(&without_missing_cwd(cmd), skip_hook);
    let user_shell = std::env::var("SHELL")
        .ok()
        .filter(|s| Path::new(s).is_file())
        .unwrap_or_else(|| "/bin/zsh".to_string());
    let terminal = pick_terminal(terminal, &installed_terminals());
    let (program, args) = mac_launch_argv(terminal, &shell, &user_shell);
    std::process::Command::new(program).args(args).spawn()?;
    Ok(())
}

#[cfg(windows)]
fn open_in_terminal(cmd: &ResumeCommand, skip_hook: bool, _terminal: Option<&str>) -> Result<()> {
    use std::os::windows::process::CommandExt;
    // Hide the short-lived outer `cmd /C`; `start` still opens a normal console window.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut c = std::process::Command::new("cmd");
    c.raw_arg(windows_start_line(cmd, skip_hook))
        .creation_flags(CREATE_NO_WINDOW);
    // `start` runs the new console in the current directory. The skip flag is NOT set on our
    // environment: the console's `cmd /K` would inherit it and keep it after the agent exits.
    // windows_start_line sets and clears it around the agent command instead.
    if let Some(dir) = existing_dir(cmd.cwd.as_deref()) {
        c.current_dir(dir);
    }
    c.spawn()?;
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_in_terminal(cmd: &ResumeCommand, skip_hook: bool, _terminal: Option<&str>) -> Result<()> {
    // Keep the terminal open after the agent exits, so its last output stays readable.
    let script = format!(
        "{}; exec \"${{SHELL:-sh}}\"",
        build_shell_command(&without_missing_cwd(cmd), skip_hook)
    );
    for (term, flag) in linux_terminals() {
        let mut c = std::process::Command::new(&term);
        if let Some(flag) = flag {
            c.arg(flag);
        }
        c.args(["sh", "-c", &script]);
        match c.spawn() {
            Ok(_) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        }
    }
    bail!(
        "No terminal emulator found. Set $TERMINAL to your terminal's command and try \
         again. A terminal Callimachus doesn't know is started as `$TERMINAL -e <command>`, \
         like xterm."
    )
}

/// Known Linux terminal emulators, in the order they're tried, with the flag that makes the
/// rest of the argv the command to run. They disagree: gnome-terminal's `-e` takes a single
/// string (`--` passes an argv), xfce4-terminal wants `-x`, wezterm a `start` subcommand, and
/// kitty takes the command bare.
const KNOWN_TERMINALS: &[(&str, Option<&str>)] = &[
    ("x-terminal-emulator", Some("-e")),
    ("gnome-terminal", Some("--")),
    ("konsole", Some("-e")),
    ("xfce4-terminal", Some("-x")),
    ("kitty", None),
    ("alacritty", Some("-e")),
    ("wezterm", Some("start")),
    ("xterm", Some("-e")),
];

/// Terminal emulators to try on Linux, in order, with the flag that makes the rest of the
/// argv the command to run. `$TERMINAL` comes first.
fn linux_terminals() -> Vec<(String, Option<&'static str>)> {
    let mut out = Vec::new();
    if let Ok(t) = std::env::var("TERMINAL") {
        let t = t.trim();
        if !t.is_empty() {
            out.push((t.to_string(), terminal_flag(t)));
        }
    }
    for (term, flag) in KNOWN_TERMINALS {
        out.push((term.to_string(), *flag));
    }
    out
}

/// The command flag for a `$TERMINAL` value (a name or a path): the known emulator's own
/// flag, else `-e`, which most others (xterm and its descendants) accept. A wrong flag still
/// spawns successfully, so the fallback list would never be tried; this has to be right.
fn terminal_flag(terminal: &str) -> Option<&'static str> {
    let name = Path::new(terminal)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(terminal);
    KNOWN_TERMINALS
        .iter()
        .find(|(known, _)| *known == name)
        .map_or(Some("-e"), |(_, flag)| *flag)
}

/// Whether `program` (a bare name or a path) resolves to a file.
fn on_path(program: &str) -> bool {
    let p = Path::new(program);
    if p.components().count() > 1 {
        return p.is_file();
    }
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

/// Build the `cd <cwd> && [SKIP=1] <program> <args…>` POSIX shell line.
#[cfg(unix)]
fn build_shell_command(cmd: &ResumeCommand, skip_hook: bool) -> String {
    let mut parts = Vec::new();
    if let Some(cwd) = &cmd.cwd {
        parts.push(format!("cd {}", shell_quote(cwd)));
    }
    let mut invocation = String::new();
    if skip_hook {
        invocation.push_str(SKIP_HOOK_ENV);
        invocation.push_str("=1 ");
    }
    invocation.push_str(&shell_quote(&cmd.program));
    for a in &cmd.args {
        invocation.push(' ');
        invocation.push_str(&shell_quote(a));
    }
    parts.push(invocation);
    parts.join(" && ")
}

/// Single-quote a value for the shell, escaping embedded single quotes.
#[cfg(unix)]
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Escape a string for embedding inside an AppleScript double-quoted literal.
#[cfg(unix)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The raw `cmd.exe` argument line for a Windows launch:
/// `/C start "Callimachus" cmd /K <program> <args…>`. `start`'s first quoted argument is
/// the window title (without it, a quoted program would be taken as the title), and
/// `cmd /K` keeps the console open after the agent exits. Built as one raw line because
/// `start` and `cmd /K` parse quotes themselves.
///
/// With `skip_hook`, the agent command becomes `set SKIP=1&& <agent> & set SKIP=`, so the
/// flag reaches only the agent and is cleared in the console's shell once it exits. The `&`s
/// are caret-escaped so the outer `cmd /C` hands them to the new console's `cmd /K` instead
/// of running them itself. (No space before `&&`: it would become part of the value.)
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_start_line(cmd: &ResumeCommand, skip_hook: bool) -> String {
    let mut line = String::from("/C start \"Callimachus\" cmd /K ");
    if skip_hook {
        line.push_str(&format!("set {SKIP_HOOK_ENV}=1^&^& "));
    }
    line.push_str(&windows_quote(&cmd.program));
    for a in &cmd.args {
        line.push(' ');
        line.push_str(&windows_quote(a));
    }
    if skip_hook {
        line.push_str(&format!(" ^& set {SKIP_HOOK_ENV}="));
    }
    line
}

/// Quote one argument for a `cmd /K` command line: wrapped in double quotes when it holds
/// whitespace or a cmd metacharacter (which are literal inside quotes). Embedded quotes
/// can't be escaped reliably for cmd, so they are dropped; our arguments (a program name,
/// a session id, a prompt with a file path) never contain one.
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_quote(arg: &str) -> String {
    let arg = arg.replace('"', "");
    let needs = arg.is_empty()
        || arg
            .chars()
            .any(|c| c.is_whitespace() || "&|<>^()@,;=".contains(c));
    if needs {
        format!("\"{arg}\"")
    } else {
        arg
    }
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
            false,
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
            false,
        )
        .unwrap();
        assert_eq!(c.args, vec!["--resume", "sess-uuid"]);
    }

    #[test]
    fn claude_ids_with_windows_separators_still_resume() {
        let top =
            resume_command("claude_code", "C--proj\\abc-123.jsonl", false, None, false).unwrap();
        assert_eq!(top.args, vec!["--resume", "abc-123"]);
        let sub = resume_command(
            "claude_code",
            "C--proj\\sess-uuid\\subagents\\agent-x.jsonl",
            true,
            None,
            false,
        )
        .unwrap();
        assert_eq!(sub.args, vec!["--resume", "sess-uuid"]);
    }

    #[test]
    fn codex_uses_thread_id() {
        let c = resume_command("codex", "019dc6b0-thread", false, Some("/p"), false).unwrap();
        assert_eq!(c.program, "codex");
        assert_eq!(c.args, vec!["resume", "019dc6b0-thread"]);
    }

    #[test]
    fn cursor_cli_sessions_resume_and_editor_chats_do_not() {
        let c = resume_command("cursor", "d7a61f15", false, Some("/p"), true).unwrap();
        assert_eq!(c.program, "cursor-agent");
        assert_eq!(c.args, vec!["--resume", "d7a61f15"]);
        assert_eq!(c.cwd.as_deref(), Some("/p"));
        let err = resume_command("cursor", "x", false, None, false).unwrap_err();
        assert!(err.to_string().contains("Open it in Cursor"));
        assert!(resume_command("in_app", "x", false, None, true).is_err());
    }

    #[cfg(unix)] // build_shell_command is the POSIX launcher (macOS + Linux)
    #[test]
    fn shell_command_quotes_paths_with_spaces() {
        let cmd = ResumeCommand {
            program: "claude".into(),
            args: vec!["--resume".into(), "abc".into()],
            cwd: Some("/Users/me/my project".into()),
        };
        let shell = build_shell_command(&cmd, false);
        assert_eq!(
            shell,
            "cd '/Users/me/my project' && 'claude' '--resume' 'abc'"
        );
    }

    #[cfg(unix)]
    #[test]
    fn context_launch_skips_the_startup_hook() {
        let cmd = ResumeCommand {
            program: "claude".into(),
            args: vec!["Read @/tmp/ctx.md".into()],
            cwd: Some("/p".into()),
        };
        assert_eq!(
            build_shell_command(&cmd, true),
            "cd '/p' && CALLIMACHUS_SKIP_HOOK=1 'claude' 'Read @/tmp/ctx.md'"
        );
    }

    #[test]
    fn windows_line_titles_the_console_and_quotes_the_prompt() {
        let cmd = ResumeCommand {
            program: "claude".into(),
            args: vec![context_prompt(Path::new(
                r"C:\Users\me\.callimachus\context\ctx-1.md",
            ))],
            cwd: None,
        };
        assert_eq!(
            windows_start_line(&cmd, false),
            r#"/C start "Callimachus" cmd /K claude "Read the conversation context in @C:\Users\me\.callimachus\context\ctx-1.md and help me continue from it.""#
        );
        // The skip flag is set for the agent only and cleared once it exits, so a fresh
        // `claude` started later in the same console still gets its memory.
        assert_eq!(
            windows_start_line(&cmd, true),
            r#"/C start "Callimachus" cmd /K set CALLIMACHUS_SKIP_HOOK=1^&^& claude "Read the conversation context in @C:\Users\me\.callimachus\context\ctx-1.md and help me continue from it." ^& set CALLIMACHUS_SKIP_HOOK="#
        );
        assert_eq!(windows_quote("--resume"), "--resume");
        assert_eq!(windows_quote("a&b"), "\"a&b\"");
        assert_eq!(windows_quote(""), "\"\"");
    }

    #[test]
    fn terminal_candidates_cover_common_emulators() {
        let list = linux_terminals();
        assert!(list
            .iter()
            .any(|(t, f)| t == "gnome-terminal" && *f == Some("--")));
        assert!(list.iter().any(|(t, _)| t == "xterm"));
    }

    #[test]
    fn terminal_env_uses_the_known_emulators_flag() {
        assert_eq!(terminal_flag("gnome-terminal"), Some("--"));
        assert_eq!(terminal_flag("/usr/bin/gnome-terminal"), Some("--"));
        assert_eq!(terminal_flag("wezterm"), Some("start"));
        assert_eq!(terminal_flag("kitty"), None);
        assert_eq!(terminal_flag("xfce4-terminal"), Some("-x"));
        assert_eq!(
            terminal_flag("some-term"),
            Some("-e"),
            "unknown terminals get -e"
        );
    }

    #[cfg(unix)]
    #[test]
    fn applescript_escapes_quotes() {
        assert_eq!(applescript_escape(r#"a "b" \c"#), r#"a \"b\" \\c"#);
    }

    fn mac(id: &str) -> &'static MacTerminal {
        MAC_TERMINALS.iter().find(|t| t.id == id).unwrap()
    }

    #[test]
    fn terminal_app_is_always_offered_and_the_rest_only_when_installed() {
        let none = installed_terminals_with(|_| false);
        assert_eq!(none.iter().map(|t| t.id).collect::<Vec<_>>(), ["terminal"]);
        let some = installed_terminals_with(|app| app == "Ghostty");
        assert_eq!(
            some.iter().map(|t| t.id).collect::<Vec<_>>(),
            ["terminal", "ghostty"]
        );
        assert!(is_known_terminal("ghostty"));
        assert!(!is_known_terminal("warp"));
    }

    #[test]
    fn an_uninstalled_or_unknown_choice_falls_back_to_terminal_app() {
        let installed = installed_terminals_with(|app| app == "Ghostty");
        assert_eq!(pick_terminal(Some("ghostty"), &installed).id, "ghostty");
        assert_eq!(pick_terminal(Some("iterm"), &installed).id, "terminal");
        assert_eq!(pick_terminal(Some("nonsense"), &installed).id, "terminal");
        assert_eq!(pick_terminal(None, &installed).id, "terminal");
    }

    #[cfg(unix)]
    #[test]
    fn ghostty_runs_the_command_in_a_login_shell_that_stays_open() {
        let (program, args) = mac_launch_argv(mac("ghostty"), "cd '/p' && 'claude'", "/bin/zsh");
        assert_eq!(program, "open");
        assert_eq!(
            args,
            [
                "-na",
                "Ghostty",
                "--args",
                "-e",
                "/bin/zsh",
                "-lic",
                "cd '/p' && 'claude'; exec '/bin/zsh' -l",
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn each_emulator_gets_its_own_command_flag() {
        let flags = |id| {
            let (_, args) = mac_launch_argv(mac(id), "x", "/bin/zsh");
            args[3..args.len() - 3].to_vec()
        };
        assert_eq!(flags("wezterm"), ["start", "--"]);
        assert!(flags("kitty").is_empty());
        assert_eq!(flags("alacritty"), ["-e"]);
    }

    #[cfg(unix)]
    #[test]
    fn the_applescript_terminals_get_an_escaped_command() {
        let shell = r#"'claude' 'say "hi"'"#;
        let (program, args) = mac_launch_argv(mac("terminal"), shell, "/bin/zsh");
        assert_eq!(program, "osascript");
        assert!(args[1].contains(r#"do script "'claude' 'say \"hi\"'""#));
        let (_, args) = mac_launch_argv(mac("iterm"), shell, "/bin/zsh");
        assert!(args[1].contains(r#"write text "'claude' 'say \"hi\"'""#));
        assert!(args[1].contains("tell application \"iTerm\""));
    }
}
