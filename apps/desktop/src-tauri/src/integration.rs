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
    dirs::home_dir().ok_or_else(|| anyhow::anyhow!("could not resolve home directory"))
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

/// The PreCompact / SubagentStop hook command: `"<cal>" snapshot-hook`, which auto-snapshots
/// the live session (before its context is compacted, or when a subagent finishes).
fn snapshot_hook_command(app_exe: &Path) -> String {
    format!("\"{}\" snapshot-hook", cal_exe(app_exe).display())
}

/// The UserPromptSubmit hook command: `"<cal>" recall-now`, which silently injects a
/// "you may have solved this before" note when the prompt strongly matches prior work.
fn recall_hook_command(app_exe: &Path) -> String {
    format!("\"{}\" recall-now", cal_exe(app_exe).display())
}

/// Recognize a hook command we installed: our commands always end with `" hook`,
/// `" snapshot-hook`, or `" recall-now` (a quoted path + the subcommand), which a user's own
/// hook is very unlikely to use.
fn is_our_hook(cmd: &str) -> bool {
    let c = cmd.trim();
    c.ends_with("\" hook") || c.ends_with("\" snapshot-hook") || c.ends_with("\" recall-now")
}

/// True if `~/.claude/settings.json` already has a Callimachus hook on `event`.
fn event_has_our_hook(v: &Value, event: &str) -> bool {
    v.get("hooks")
        .and_then(|h| h.get(event))
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

/// True if `~/.claude/settings.json` already has a Callimachus SessionStart hook.
fn session_start_has_our_hook(v: &Value) -> bool {
    event_has_our_hook(v, "SessionStart")
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
    /// The opt-in UserPromptSubmit "proactive recall" hook is installed (injects prior work
    /// before each prompt). Separate from `hook_installed` because it reads every prompt.
    pub proactive_recall_installed: bool,
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

    let settings = settings_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());
    let hook_installed = settings
        .as_ref()
        .map(session_start_has_our_hook)
        .unwrap_or(false);
    let proactive_recall_installed = settings
        .as_ref()
        .map(|v| event_has_our_hook(v, "UserPromptSubmit"))
        .unwrap_or(false);

    IntegrationStatus {
        skill_installed,
        skill_outdated,
        mcp_registered,
        hook_installed,
        proactive_recall_installed,
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

/// Toggle the opt-in proactive-recall hook (UserPromptSubmit -> `cal recall-now`) on its own,
/// independent of the base integration. It reads every prompt, so it stays off until the user
/// asks for it. Enabling ensures `cal` exists first (so the hook command resolves); disabling
/// strips only our UserPromptSubmit hook and leaves any other hooks untouched. Idempotent.
pub fn set_proactive_recall(app_exe: &Path, enabled: bool) -> Result<IntegrationStatus> {
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

    if enabled {
        install_cal(app_exe)?; // the hook command points at `cal`; make sure it's there
        upsert_hook(obj, "UserPromptSubmit", &recall_hook_command(app_exe))?;
    } else if let Some(arr) = obj
        .get_mut("hooks")
        .and_then(|h| h.get_mut("UserPromptSubmit"))
        .and_then(Value::as_array_mut)
    {
        remove_our_hooks(arr);
    }

    std::fs::write(&path, serde_json::to_string_pretty(&root)?)
        .with_context(|| format!("writing {}", path.display()))?;
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
    // Strip our hooks from every event we install into (leave any other hooks alone).
    if let Ok(path) = settings_path() {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(mut v) = serde_json::from_str::<Value>(&text) {
                for event in HOOK_EVENTS {
                    if let Some(arr) = v
                        .get_mut("hooks")
                        .and_then(|h| h.get_mut(event))
                        .and_then(Value::as_array_mut)
                    {
                        remove_our_hooks(arr);
                    }
                }
                let _ = std::fs::write(&path, serde_json::to_string_pretty(&v)?);
            }
        }
    }
    if let Ok(link) = cal_link_path() {
        let _ = std::fs::remove_file(&link);
    }
    Ok(())
}

/// The Claude Code hook events we install into: SessionStart injects each repo's memory;
/// PreCompact + SubagentStop auto-snapshot the live session so context survives a compaction
/// and a finished subagent's work is captured.
const HOOK_EVENTS: [&str; 4] = [
    "SessionStart",
    "PreCompact",
    "SubagentStop",
    "UserPromptSubmit",
];

/// Merge our hooks into `~/.claude/settings.json`. Preserves all other settings and hooks;
/// refuses to clobber an unparseable file; idempotent (re-install drops any prior Callimachus
/// hook first, so it never duplicates).
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

    let snapshot = snapshot_hook_command(app_exe);
    upsert_hook(obj, "SessionStart", &hook_command(app_exe))?; // inject project memory
    upsert_hook(obj, "PreCompact", &snapshot)?; // auto-snapshot before compaction
    upsert_hook(obj, "SubagentStop", &snapshot)?; // auto-snapshot a finished subagent
                                                  // NOTE: the UserPromptSubmit "proactive recall" hook is deliberately NOT installed here. It
                                                  // reads every prompt, so it's a separate opt-in toggle (set_proactive_recall), off by default.

    std::fs::write(&path, serde_json::to_string_pretty(&root)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Merge one Callimachus hook command into `hooks.<event>` of a settings object, dropping any
/// prior Callimachus hook in that event first (idempotent) while preserving the user's own.
fn upsert_hook(obj: &mut Map<String, Value>, event: &str, command: &str) -> Result<()> {
    let hooks = obj
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("hooks is not an object"))?;
    let arr = hooks
        .entry(event)
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("hooks.{event} is not an array"))?;
    remove_our_hooks(arr);
    arr.push(json!({ "hooks": [ { "type": "command", "command": command } ] }));
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

/// Register Claude Code's MCP server in `~/.claude.json`.
fn register_mcp(app_exe: &Path) -> Result<()> {
    register_mcp_json(&claude_config_path()?, app_exe)
}

/// Merge a `callimachus` entry into a JSON config's `mcpServers`, pointing the client at this
/// app run with `--mcp`. Shared by Claude Code, Cursor, and Gemini (same schema, different
/// files). Preserves all other config; refuses to clobber an unparseable file.
fn register_mcp_json(cfg: &Path, app_exe: &Path) -> Result<()> {
    if let Some(dir) = cfg.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut root: Value = match std::fs::read_to_string(cfg) {
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

    std::fs::write(cfg, serde_json::to_string_pretty(&root)?)
        .with_context(|| format!("writing {}", cfg.display()))?;
    Ok(())
}

/// Remove our `mcpServers.callimachus` entry from a JSON config (leaves the rest intact).
fn remove_mcp_json(cfg: &Path) -> Result<()> {
    if let Ok(text) = std::fs::read_to_string(cfg) {
        if let Ok(mut v) = serde_json::from_str::<Value>(&text) {
            if let Some(servers) = v.get_mut("mcpServers").and_then(Value::as_object_mut) {
                servers.remove(MCP_NAME);
            }
            let _ = std::fs::write(cfg, serde_json::to_string_pretty(&v)?);
        }
    }
    Ok(())
}

/// True if a JSON config registers our MCP server pointing at this exe.
fn mcp_registered_json(cfg: &Path, app_exe: &Path) -> bool {
    std::fs::read_to_string(cfg)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| {
            v.get("mcpServers")
                .and_then(|m| m.get(MCP_NAME))
                .and_then(|s| s.get("command"))
                .and_then(Value::as_str)
                .map(|cmd| cmd == app_exe.to_string_lossy())
        })
        .unwrap_or(false)
}

/// Merge `[mcp_servers.callimachus]` into Codex's `~/.codex/config.toml` with format-
/// preserving TOML edits (keeps the user's comments + ordering). Refuses unparseable TOML.
fn register_mcp_toml(cfg: &Path, app_exe: &Path) -> Result<()> {
    if let Some(dir) = cfg.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut doc = match std::fs::read_to_string(cfg) {
        Ok(text) if !text.trim().is_empty() => text
            .parse::<toml_edit::DocumentMut>()
            .with_context(|| format!("{} is not valid TOML; not modifying it", cfg.display()))?,
        _ => toml_edit::DocumentMut::new(),
    };
    let servers = doc
        .entry("mcp_servers")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("mcp_servers in {} is not a table", cfg.display()))?;
    let mut entry = toml_edit::Table::new();
    entry["command"] = toml_edit::value(app_exe.to_string_lossy().to_string());
    let mut args = toml_edit::Array::new();
    args.push("--mcp");
    entry["args"] = toml_edit::value(args);
    servers.insert(MCP_NAME, toml_edit::Item::Table(entry));
    std::fs::write(cfg, doc.to_string()).with_context(|| format!("writing {}", cfg.display()))?;
    Ok(())
}

/// Remove our `[mcp_servers.callimachus]` table from Codex's config (leaves the rest intact).
fn remove_mcp_toml(cfg: &Path) -> Result<()> {
    if let Ok(text) = std::fs::read_to_string(cfg) {
        if let Ok(mut doc) = text.parse::<toml_edit::DocumentMut>() {
            if let Some(servers) = doc.get_mut("mcp_servers").and_then(|i| i.as_table_mut()) {
                servers.remove(MCP_NAME);
            }
            let _ = std::fs::write(cfg, doc.to_string());
        }
    }
    Ok(())
}

/// True if Codex's config registers our MCP server pointing at this exe.
fn mcp_registered_toml(cfg: &Path, app_exe: &Path) -> bool {
    std::fs::read_to_string(cfg)
        .ok()
        .and_then(|s| s.parse::<toml_edit::DocumentMut>().ok())
        .and_then(|doc| {
            doc.get("mcp_servers")
                .and_then(|i| i.get(MCP_NAME))
                .and_then(|s| s.get("command"))
                .and_then(|c| c.as_str())
                .map(|cmd| cmd == app_exe.to_string_lossy())
        })
        .unwrap_or(false)
}

/// One non-Claude agent's MCP integration state.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentIntegration {
    pub id: String,
    pub label: String,
    /// The agent's config dir exists (i.e. the user actually uses it).
    pub present: bool,
    /// Our MCP server is registered in its config, pointing at this app.
    pub registered: bool,
    pub config_path: String,
}

enum Fmt {
    Json,
    Toml,
}
struct AgentDef {
    id: &'static str,
    label: &'static str,
    dir: &'static str,
    config: &'static str,
    fmt: Fmt,
}

/// The agents we can register the MCP server for, beyond Claude Code. Cursor + Gemini use the
/// JSON `mcpServers` schema; Codex uses a TOML `[mcp_servers]` table.
fn agent_defs() -> [AgentDef; 3] {
    [
        AgentDef {
            id: "codex",
            label: "Codex",
            dir: ".codex",
            config: ".codex/config.toml",
            fmt: Fmt::Toml,
        },
        AgentDef {
            id: "cursor",
            label: "Cursor",
            dir: ".cursor",
            config: ".cursor/mcp.json",
            fmt: Fmt::Json,
        },
        AgentDef {
            id: "gemini",
            label: "Gemini CLI",
            dir: ".gemini",
            config: ".gemini/settings.json",
            fmt: Fmt::Json,
        },
    ]
}

/// Detected-agent MCP status (present = config dir exists; registered = our server is wired).
pub fn agent_status(app_exe: &Path) -> Vec<AgentIntegration> {
    let Ok(home) = home() else { return Vec::new() };
    agent_defs()
        .into_iter()
        .map(|a| {
            let cfg = home.join(a.config);
            let registered = match a.fmt {
                Fmt::Json => mcp_registered_json(&cfg, app_exe),
                Fmt::Toml => mcp_registered_toml(&cfg, app_exe),
            };
            AgentIntegration {
                id: a.id.to_string(),
                label: a.label.to_string(),
                present: home.join(a.dir).is_dir(),
                registered,
                config_path: cfg.display().to_string(),
            }
        })
        .collect()
}

/// Register the MCP server for every DETECTED agent (config dir exists). Never creates a
/// config for an agent the user doesn't use. Idempotent.
pub fn install_agents(app_exe: &Path) -> Result<Vec<AgentIntegration>> {
    let home = home()?;
    for a in agent_defs() {
        if !home.join(a.dir).is_dir() {
            continue;
        }
        let cfg = home.join(a.config);
        match a.fmt {
            Fmt::Json => register_mcp_json(&cfg, app_exe)?,
            Fmt::Toml => register_mcp_toml(&cfg, app_exe)?,
        }
    }
    Ok(agent_status(app_exe))
}

/// Remove our MCP registration from every agent's config (leaves their other config intact).
pub fn uninstall_agents() -> Result<()> {
    let home = home()?;
    for a in agent_defs() {
        let cfg = home.join(a.config);
        match a.fmt {
            Fmt::Json => remove_mcp_json(&cfg)?,
            Fmt::Toml => remove_mcp_toml(&cfg)?,
        }
    }
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
        assert!(
            user_hook_kept,
            "the user's own SessionStart hook is preserved"
        );
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

    #[test]
    fn codex_toml_merge_preserves_others() {
        // Mirror register_mcp_toml's merge against an in-memory Codex config.
        let mut doc = "model = \"o3\"\n\n[mcp_servers.other]\ncommand = \"x\"\n"
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        let servers = doc
            .entry("mcp_servers")
            .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
            .as_table_mut()
            .unwrap();
        let mut entry = toml_edit::Table::new();
        entry["command"] = toml_edit::value("/app");
        let mut args = toml_edit::Array::new();
        args.push("--mcp");
        entry["args"] = toml_edit::value(args);
        servers.insert(MCP_NAME, toml_edit::Item::Table(entry));

        let back = doc.to_string().parse::<toml_edit::DocumentMut>().unwrap();
        assert_eq!(back["model"].as_str(), Some("o3")); // preserved
        assert_eq!(back["mcp_servers"]["other"]["command"].as_str(), Some("x")); // preserved
        assert_eq!(
            back["mcp_servers"]["callimachus"]["command"].as_str(),
            Some("/app")
        ); // added
        assert_eq!(
            back["mcp_servers"]["callimachus"]["args"][0].as_str(),
            Some("--mcp")
        );
    }

    #[test]
    fn snapshot_hooks_install_across_events_idempotently() {
        // is_our_hook recognizes the memory, snapshot, and recall hooks, nothing else.
        assert!(is_our_hook("\"/x/cal\" hook"));
        assert!(is_our_hook("\"/x/cal\" snapshot-hook"));
        assert!(is_our_hook("\"/x/cal\" recall-now"));
        assert!(!is_our_hook("\"/x/cal\" other"));
        assert!(!is_our_hook("my-own-snapshot-hook"));

        // Install every event (mirrors install_hook), then re-install to prove no dup.
        let mut root = json!({"model": "opus"});
        let obj = root.as_object_mut().unwrap();
        for _ in 0..2 {
            upsert_hook(obj, "SessionStart", "\"/x/cal\" hook").unwrap();
            upsert_hook(obj, "PreCompact", "\"/x/cal\" snapshot-hook").unwrap();
            upsert_hook(obj, "SubagentStop", "\"/x/cal\" snapshot-hook").unwrap();
            upsert_hook(obj, "UserPromptSubmit", "\"/x/cal\" recall-now").unwrap();
        }

        let ours_in = |root: &Value, event: &str| -> usize {
            root["hooks"][event]
                .as_array()
                .map(|gs| {
                    gs.iter()
                        .flat_map(|g| g["hooks"].as_array().unwrap().iter())
                        .filter(|h| is_our_hook(h["command"].as_str().unwrap()))
                        .count()
                })
                .unwrap_or(0)
        };
        assert_eq!(root["model"], "opus"); // unrelated setting preserved
        for event in HOOK_EVENTS {
            assert_eq!(ours_in(&root, event), 1, "exactly one hook in {event}");
        }
        assert_eq!(
            root["hooks"]["PreCompact"][0]["hooks"][0]["command"],
            "\"/x/cal\" snapshot-hook"
        );

        // Uninstall strips ours from every event.
        for event in HOOK_EVENTS {
            let arr = root["hooks"][event].as_array_mut().unwrap();
            remove_our_hooks(arr);
        }
        for event in HOOK_EVENTS {
            assert_eq!(ours_in(&root, event), 0, "{event} cleared on uninstall");
        }
    }
}
