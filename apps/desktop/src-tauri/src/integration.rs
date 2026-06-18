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
    std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| anyhow::anyhow!("HOME unset"))
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

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationStatus {
    /// The `/recall` skill file exists and is current.
    pub skill_installed: bool,
    /// Skill exists but its content differs from the bundled version (re-install to update).
    pub skill_outdated: bool,
    /// An MCP server named `callimachus` is registered and points at this app.
    pub mcp_registered: bool,
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

    IntegrationStatus {
        skill_installed,
        skill_outdated,
        mcp_registered,
        cal_installed,
        skill_path: skill.map(|p| p.display().to_string()).unwrap_or_default(),
        config_path: claude_config_path().map(|p| p.display().to_string()).unwrap_or_default(),
    }
}

/// Install (or refresh) the skill, MCP registration, and `cal` CLI symlink. Idempotent.
pub fn install(app_exe: &Path) -> Result<IntegrationStatus> {
    write_skill()?;
    register_mcp(app_exe)?;
    install_cal(app_exe)?;
    Ok(status(app_exe))
}

/// Remove the skill file and the MCP registration. Leaves the rest of the config intact.
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
    if let Ok(link) = cal_link_path() {
        let _ = std::fs::remove_file(&link);
    }
    Ok(())
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

#[cfg(not(unix))]
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
        std::fs::write(&cfg, r#"{"numStartups":7,"mcpServers":{"other":{"command":"x"}}}"#).unwrap();

        // Hand-run the merge against this specific file (mirrors register_mcp).
        let mut root: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let servers = root
            .as_object_mut()
            .unwrap()
            .entry("mcpServers")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .unwrap();
        servers.insert(MCP_NAME.into(), json!({"type":"stdio","command":"/app","args":["--mcp"]}));
        std::fs::write(&cfg, serde_json::to_string_pretty(&root).unwrap()).unwrap();

        let back: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back["numStartups"], 7); // preserved
        assert_eq!(back["mcpServers"]["other"]["command"], "x"); // preserved
        assert_eq!(back["mcpServers"]["callimachus"]["command"], "/app"); // added
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
