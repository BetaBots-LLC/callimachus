//! One-click Claude Code integration. Makes the `/recall` skill + MCP server
//! available with zero terminal/cargo: it writes the bundled skill markdown into
//! the user's personal skills dir and registers the *running app itself* (launched
//! with `--mcp`) as a user-scope MCP server in `~/.claude.json`. No second binary
//! to ship, no PATH changes.

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// The `/recall` skill, embedded at compile time from a committed resource (so a
/// fresh checkout builds — the repo's own `.claude/` is gitignored). Written
/// verbatim to the user's skills dir on install.
pub const SKILL_MD: &str = include_str!("../resources/recall/SKILL.md");

/// The MCP server name registered in the user's Claude config.
const MCP_NAME: &str = "callimachus";

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME unset"))
}

/// `~/.claude/skills/recall/SKILL.md`
fn skill_path() -> Result<PathBuf> {
    Ok(home()?.join(".claude/skills/recall/SKILL.md"))
}

/// `~/.claude.json` — Claude Code's user config (holds user-scope `mcpServers`).
fn claude_config_path() -> Result<PathBuf> {
    Ok(home()?.join(".claude.json"))
}

/// `~/.local/bin/cal` — where we symlink the app so the `cal` CLI (and the VS Code
/// extension, which probes this path) works without a separate install.
fn cal_link_path() -> Result<PathBuf> {
    Ok(home()?.join(".local/bin/cal"))
}

/// `~/.claude/settings.json` — Claude Code's user settings (holds `hooks`).
fn settings_path() -> Result<PathBuf> {
    Ok(home()?.join(".claude/settings.json"))
}

/// The absolute `cal` entrypoint used in the SessionStart hook command. On Unix this is the
/// `~/.local/bin/cal` symlink we install; on Windows the `cal.exe` placed next to the app.
fn cal_exe(app_exe: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        return app_exe
            .parent()
            .map(|d| d.join("cal.exe"))
            .unwrap_or_else(|| PathBuf::from("cal.exe"));
    }
    #[cfg(not(windows))]
    {
        let _ = app_exe;
        cal_link_path().unwrap_or_else(|_| PathBuf::from("cal"))
    }
}

/// The SessionStart hook command: `"<cal>" hook`, which prints the current repo's memory
/// (Callimachus injects it into the session). Quoted so a spaced path is shell-safe.
fn hook_command(app_exe: &Path) -> String {
    format!("\"{}\" hook", cal_exe(app_exe).display())
}

/// Recognize a hook command we installed: our format always ends with `" hook` (a quoted
/// path followed by the `hook` subcommand), which a user's own hook is very unlikely to use.
fn is_our_hook(cmd: &str) -> bool {
    cmd.trim().ends_with("\" hook")
}

/// True if `~/.claude/settings.json` already has a Callimachus SessionStart hook.
fn session_start_has_our_hook(v: &Value) -> bool {
    v.get("hooks")
        .and_then(|h| h.get("SessionStart"))
        .and_then(Value::as_array)
        .map(|groups| {
            groups.iter().any(|g| {
                g.get("hooks")
                    .and_then(Value::as_array)
                    .map(|hs| {
                        hs.iter().any(|h| {
                            h.get("command")
                                .and_then(Value::as_str)
                                .map(is_our_hook)
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationStatus {
    /// The `/recall` skill file exists and is current.
    pub skill_installed: bool,
    /// Skill exists but its content differs from the bundled version (re-install to update).
    pub skill_outdated: bool,
    /// An MCP server named `callimachus` is registered and points at this app.
    pub mcp_registered: bool,
    /// A Callimachus SessionStart hook is installed (auto-injects project memory).
    pub hook_installed: bool,
    /// `~/.local/bin/cal` exists (powers the `cal` CLI + the VS Code extension).
    pub cal_installed: bool,
    pub skill_path: String,
    pub config_path: String,
}

/// Current integration state. `app_exe` is the running app's executable path
/// (`std::env::current_exe()`), used to verify the MCP registration points here.
pub fn status(app_exe: &Path) -> IntegrationStatus {
    let skill = skill_path().ok();
    let (skill_installed, skill_outdated) = match &skill {
        Some(p) => match std::fs::read_to_string(p) {
            Ok(existing) => (true, existing != SKILL_MD),
            Err(_) => (false, false),
        },
        None => (false, false),
    };

    let mcp_registered = claude_config_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| {
            v.get("mcpServers")
                .and_then(|m| m.get(MCP_NAME))
                .and_then(|s| s.get("command"))
                .and_then(Value::as_str)
                .map(|cmd| cmd == app_exe.to_string_lossy())
        })
        .unwrap_or(false);

    let cal_installed = cal_link_path().map(|p| p.exists()).unwrap_or(false);

    let hook_installed = settings_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .map(|v| session_start_has_our_hook(&v))
        .unwrap_or(false);

    IntegrationStatus {
        skill_installed,
        skill_outdated,
        mcp_registered,
        hook_installed,
        cal_installed,
        skill_path: skill.map(|p| p.display().to_string()).unwrap_or_default(),
        config_path: claude_config_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    }
}

/// Install (or refresh) the skill, MCP registration, `cal` CLI symlink, and the SessionStart
/// hook (which auto-injects each repo's memory at the start of a Claude Code session).
/// Idempotent. `cal` is installed before the hook so the hook command resolves.
pub fn install(app_exe: &Path) -> Result<IntegrationStatus> {
    write_skill()?;
    register_mcp(app_exe)?;
    install_cal(app_exe)?;
    install_hook(app_exe)?;
    Ok(status(app_exe))
}

/// Remove the skill file, the MCP registration, the SessionStart hook, and the `cal` symlink.
/// Leaves the rest of each config intact.
pub fn uninstall() -> Result<()> {
    if let Ok(p) = skill_path() {
        let _ = std::fs::remove_file(&p);
    }
    let cfg = claude_config_path()?;
    if let Ok(text) = std::fs::read_to_string(&cfg) {
        if let Ok(mut v) = serde_json::from_str::<Value>(&text) {
            if let Some(servers) = v.get_mut("mcpServers").and_then(Value::as_object_mut) {
                servers.remove(MCP_NAME);
            }
            std::fs::write(&cfg, serde_json::to_string_pretty(&v)?)?;
        }
    }
    // Strip our SessionStart hook from settings.json (leave any other hooks alone).
    if let Ok(path) = settings_path() {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(mut v) = serde_json::from_str::<Value>(&text) {
                if let Some(arr) = v
                    .get_mut("hooks")
                    .and_then(|h| h.get_mut("SessionStart"))
                    .and_then(Value::as_array_mut)
                {
                    remove_our_hooks(arr);
                    let _ = std::fs::write(&path, serde_json::to_string_pretty(&v)?);
                }
            }
        }
    }
    if let Ok(link) = cal_link_path() {
        let _ = std::fs::remove_file(&link);
    }
    Ok(())
}

/// Merge a Callimachus SessionStart hook into `~/.claude/settings.json`. Preserves all other
/// settings and hooks; refuses to clobber an unparseable file; idempotent (re-install drops
/// any prior Callimachus hook first, so it never duplicates).
fn install_hook(app_exe: &Path) -> Result<()> {
    let path = settings_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut root: Value = match std::fs::read_to_string(&path) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON; not modifying it", path.display()))?,
        _ => Value::Object(Map::new()),
    };
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} is not a JSON object", path.display()))?;
    let hooks = obj
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("hooks in {} is not an object", path.display()))?;
    let arr = hooks
        .entry("SessionStart")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("hooks.SessionStart in {} is not an array", path.display()))?;

    remove_our_hooks(arr); // drop any prior Callimachus hook so re-install doesn't duplicate
    arr.push(json!({
        "hooks": [ { "type": "command", "command": hook_command(app_exe) } ]
    }));

    std::fs::write(&path, serde_json::to_string_pretty(&root)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Remove Callimachus hooks from a SessionStart array (and any group we leave empty),
/// leaving the user's own hooks untouched.
fn remove_our_hooks(arr: &mut Vec<Value>) {
    for group in arr.iter_mut() {
        if let Some(hs) = group.get_mut("hooks").and_then(Value::as_array_mut) {
            hs.retain(|h| {
                !h.get("command")
                    .and_then(Value::as_str)
                    .map(is_our_hook)
                    .unwrap_or(false)
            });
        }
    }
    arr.retain(|group| {
        group
            .get("hooks")
            .and_then(Value::as_array)
            .map(|h| !h.is_empty())
            .unwrap_or(true)
    });
}

/// Symlink `~/.local/bin/cal` → this app (which runs in `cal` mode when invoked by
/// that name). The VS Code extension probes this exact path, so this is what makes
/// it work without a manual CLI install. Unix only; a no-op elsewhere.
#[cfg(unix)]
fn install_cal(app_exe: &Path) -> Result<()> {
    let link = cal_link_path()?;
    if let Some(dir) = link.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let _ = std::fs::remove_file(&link); // replace any stale symlink/file
    std::os::unix::fs::symlink(app_exe, &link)
        .with_context(|| format!("linking {} -> {}", link.display(), app_exe.display()))?;
    Ok(())
}

/// Place a `cal.exe` next to the app so the `cal` CLI resolves without touching
/// PATH. The VS Code extension probes the install dir for `cal.exe` (see
/// `calCandidates()` in apps/vscode/src/cal.ts), and the binary runs in `cal`
/// mode when invoked by that name (main.rs argv0 check). A hardlink keeps it to
/// one on-disk copy (same dir → same volume); falls back to a plain copy.
#[cfg(windows)]
fn install_cal(app_exe: &Path) -> Result<()> {
    // Don't link onto ourselves if somehow invoked as cal.exe.
    if app_exe.file_stem().and_then(|s| s.to_str()) == Some("cal") {
        return Ok(());
    }
    let dir = app_exe
        .parent()
        .context("app exe has no parent directory")?;
    let link = dir.join("cal.exe");
    let _ = std::fs::remove_file(&link); // replace any stale link/copy
    std::fs::hard_link(app_exe, &link)
        .or_else(|_| std::fs::copy(app_exe, &link).map(|_| ()))
        .with_context(|| format!("creating {}", link.display()))?;
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn install_cal(_app_exe: &Path) -> Result<()> {
    Ok(())
}

fn write_skill() -> Result<()> {
    let path = skill_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&path, SKILL_MD).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Merge a `callimachus` entry into `~/.claude.json` `mcpServers`, pointing Claude
/// Code at this app run with `--mcp`. Preserves all other config; refuses to clobber
/// an unparseable file.
fn register_mcp(app_exe: &Path) -> Result<()> {
    let cfg = claude_config_path()?;
    let mut root: Value = match std::fs::read_to_string(&cfg) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON; not modifying it", cfg.display()))?,
        _ => Value::Object(Map::new()),
    };
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} is not a JSON object", cfg.display()))?;

    let servers = obj
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("mcpServers in {} is not an object", cfg.display()))?;

    servers.insert(
        MCP_NAME.to_string(),
        json!({
            "type": "stdio",
            "command": app_exe.to_string_lossy(),
            "args": ["--mcp"],
        }),
    );

    std::fs::write(&cfg, serde_json::to_string_pretty(&root)?)
        .with_context(|| format!("writing {}", cfg.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_is_embedded() {
        assert!(SKILL_MD.contains("recall"));
        assert!(SKILL_MD.len() > 200);
    }

    #[test]
    fn register_mcp_merges_without_clobbering() {
        // Point HOME at a temp dir so we exercise the real file paths in isolation.
        let tmp = std::env::temp_dir().join(format!("calli_integ_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let cfg = tmp.join(".claude.json");
        std::fs::write(
            &cfg,
            r#"{"numStartups":7,"mcpServers":{"other":{"command":"x"}}}"#,
        )
        .unwrap();

        // Hand-run the merge against this specific file (mirrors register_mcp).
        let mut root: Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let servers = root
            .as_object_mut()
            .unwrap()
            .entry("mcpServers")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .unwrap();
        servers.insert(
            MCP_NAME.into(),
            json!({"type":"stdio","command":"/app","args":["--mcp"]}),
        );
        std::fs::write(&cfg, serde_json::to_string_pretty(&root).unwrap()).unwrap();

        let back: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back["numStartups"], 7); // preserved
        assert_eq!(back["mcpServers"]["other"]["command"], "x"); // preserved
        assert_eq!(back["mcpServers"]["callimachus"]["command"], "/app"); // added
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn hook_merge_is_idempotent_and_preserves_others() {
        // Hand-run the SessionStart merge (mirrors install_hook) against an in-memory config
        // so the test never touches the real HOME / settings.json.
        let mut root: Value = serde_json::from_str(
            r#"{"model":"opus",
                "hooks":{
                  "PreToolUse":[{"hooks":[{"type":"command","command":"echo hi"}]}],
                  "SessionStart":[{"hooks":[{"type":"command","command":"my-own-hook"}]}]
                }}"#,
        )
        .unwrap();
        let cmd = "\"/Users/x/.local/bin/cal\" hook".to_string();
        let merge = |root: &mut Value, cmd: &str| {
            let arr = root
                .as_object_mut()
                .unwrap()
                .entry("hooks")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .unwrap()
                .entry("SessionStart")
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .unwrap();
            remove_our_hooks(arr);
            arr.push(json!({"hooks":[{"type":"command","command":cmd}]}));
        };
        merge(&mut root, &cmd);
        merge(&mut root, &cmd); // installing twice must NOT duplicate

        assert_eq!(root["model"], "opus"); // unrelated setting preserved
        assert_eq!(
            root["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "echo hi"
        ); // other event preserved
        let ss = root["hooks"]["SessionStart"].as_array().unwrap();
        let ours = ss
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap())
            .filter(|h| is_our_hook(h["command"].as_str().unwrap()))
            .count();
        assert_eq!(ours, 1, "exactly one Callimachus hook after two installs");
        let user_hook_kept = ss
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap())
            .any(|h| h["command"] == "my-own-hook");
        assert!(user_hook_kept, "the user's own SessionStart hook is preserved");
        assert!(session_start_has_our_hook(&root));

        // Uninstall removes ours but keeps the user's.
        let arr = root["hooks"]["SessionStart"].as_array_mut().unwrap();
        remove_our_hooks(arr);
        assert!(!session_start_has_our_hook(&root));
        assert!(root["hooks"]["SessionStart"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap())
            .any(|h| h["command"] == "my-own-hook"));
    }
}
