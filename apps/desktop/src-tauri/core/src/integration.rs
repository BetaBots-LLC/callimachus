//! One-click agent integration. Makes the `/recall` skill, the MCP server, the memory hooks and
//! the `cal` CLI available with no terminal setup: it writes the bundled skill into the user's
//! personal skills dir, registers the *running app itself* (launched with `--mcp`) as a
//! user-scope MCP server in `~/.claude.json` (and in the other detected agents' configs), merges
//! Claude Code hooks into `settings.json`, and links `cal` to the app. No second binary to ship.
//!
//! Every config edit is a read-modify-write that keeps the user's other settings, refuses to
//! touch a file it can't read or parse, skips the write when nothing changed, and replaces the
//! file atomically (temp file + rename), so a concurrently running agent never reads a
//! half-written config.

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

/// The `/recall` skill, embedded at compile time from a committed resource (so a fresh checkout
/// builds; the repo's own `.claude/` is gitignored). Written to the user's skills dir on install,
/// with a machine-specific note about where `cal` lives (see [`render_skill`]).
pub const SKILL_MD: &str = include_str!("../../resources/recall/SKILL.md");

/// The MCP server name registered in each agent's config.
const MCP_NAME: &str = "callimachus";

// ---------------------------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------------------------

fn home() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| anyhow!("could not resolve home directory"))
}

/// Claude Code's config dir: `$CLAUDE_CONFIG_DIR` when set (multi-account setups relocate it),
/// else `~/.claude`. Holds `settings.json` (hooks), `skills/` and `projects/` (transcripts).
///
/// A GUI app launched from Finder or the Dock doesn't inherit shell-exported variables, so a
/// Settings override (`paths.claude_config_dir`, applied at startup) wins when set; otherwise
/// the variable applies when it reaches this process (terminal launches, hooks, the MCP
/// server). This is the indexer's resolver, so the hooks and skill land in the dir whose
/// transcripts get indexed.
pub fn claude_config_dir() -> Result<PathBuf> {
    crate::indexer::claude::config_dir().ok_or_else(|| anyhow!("could not resolve home directory"))
}

/// Codex's home: the Settings override (`paths.codex_home`), else `$CODEX_HOME`, else
/// `~/.codex`. The indexer's resolver, so the MCP entry goes where the indexed Codex lives.
pub fn codex_home() -> Result<PathBuf> {
    crate::indexer::codex::codex_root().ok_or_else(|| anyhow!("could not resolve home directory"))
}

/// `<claude config dir>/skills/recall/SKILL.md`
fn skill_path() -> Result<PathBuf> {
    Ok(claude_config_dir()?
        .join("skills")
        .join("recall")
        .join("SKILL.md"))
}

/// `~/.agents/skills/recall/SKILL.md`: the shared user skills dir Codex, Gemini CLI and other
/// agents scan besides their own. Written and refreshed even while the Claude Code plugin
/// replaces Claude's copy; removed with it on uninstall.
fn shared_skill_path() -> Result<PathBuf> {
    Ok(home()?
        .join(".agents")
        .join("skills")
        .join("recall")
        .join("SKILL.md"))
}

/// `<claude config dir>/settings.json`: Claude Code's user settings (holds `hooks`).
fn settings_path() -> Result<PathBuf> {
    Ok(claude_config_dir()?.join("settings.json"))
}

/// `.claude.json`: Claude Code's user config (holds user-scope `mcpServers`).
fn claude_config_path() -> Result<PathBuf> {
    let relocated = crate::indexer::claude::config_dir_override()
        .map(OsString::from)
        .or_else(|| std::env::var_os("CLAUDE_CONFIG_DIR"));
    Ok(claude_config_path_from(relocated, &home()?))
}

/// `~/.claude.json` normally stays in `$HOME` even when `CLAUDE_CONFIG_DIR` moves the rest. But
/// if the relocated dir already holds its own `.claude.json`, that's the file this Claude Code
/// reads, so register there instead of in a file it never opens.
fn claude_config_path_from(config_dir_env: Option<OsString>, home: &Path) -> PathBuf {
    if let Some(dir) = config_dir_env.filter(|v| !v.is_empty()) {
        let relocated =
            crate::indexer::claude::expand_tilde(PathBuf::from(dir)).join(".claude.json");
        if relocated.is_file() {
            return relocated;
        }
    }
    home.join(".claude.json")
}

/// Where the `cal` entry point lives. Unix: the `~/.local/bin/cal` symlink (the VS Code
/// extension probes this exact path). Windows: a `cal.exe` next to the app, which the extension
/// also probes.
fn cal_path(exe: &Path) -> Result<PathBuf> {
    if cfg!(windows) {
        exe.parent()
            .map(|d| d.join("cal.exe"))
            .context("app exe has no parent directory")
    } else {
        Ok(home()?.join(".local").join("bin").join("cal"))
    }
}

// ---------------------------------------------------------------------------------------------
// A stable path for the running app
// ---------------------------------------------------------------------------------------------

/// Shown when macOS runs the app from a temporary location.
const MOVE_TO_APPLICATIONS: &str = "Move Callimachus to /Applications first. It's running from \
     a disk image or a quarantined download (macOS App Translocation), a temporary location that \
     goes away once the app quits, so agents couldn't launch it later. Drag it into Applications, \
     open it from there, then try again.";

/// The path agents should launch: what goes into MCP entries, hook commands and the `cal` link.
///
/// `current` is `std::env::current_exe()`, which can be temporary:
/// - Linux AppImage: the binary runs from a randomized mount (`/tmp/.mount_XXXXXX/...`) that
///   disappears when the app exits; the runtime exports the real file as `$APPIMAGE`.
/// - macOS: an app opened from a DMG (`/Volumes/<dmg>/Callimachus.app`) or quarantined in
///   Downloads (`.../AppTranslocation/<uuid>/...`). Those can't be fixed from here, so this
///   errors with a "move it to /Applications" message instead.
/// - Windows: the install path is stable as-is.
pub fn stable_app_exe(current: &Path) -> Result<PathBuf> {
    if cfg!(target_os = "linux") {
        let appimage = std::env::var_os("APPIMAGE").map(PathBuf::from);
        let appdir = std::env::var_os("APPDIR").map(PathBuf::from);
        linux_stable_exe(current, appimage.as_deref(), appdir.as_deref())
    } else if cfg!(target_os = "macos") {
        if is_macos_ephemeral(current) {
            bail!(MOVE_TO_APPLICATIONS);
        }
        Ok(current.to_path_buf())
    } else {
        Ok(current.to_path_buf())
    }
}

/// True when `current` runs from inside an AppImage: its `.mount_*` dir, or the `$APPDIR` the
/// runtime set (also used by `--appimage-extract-and-run`).
fn in_appimage(current: &Path, appdir: Option<&Path>) -> bool {
    current
        .components()
        .any(|c| c.as_os_str().to_string_lossy().starts_with(".mount_"))
        || appdir.is_some_and(|d| d.is_absolute() && current.starts_with(d))
}

/// Whether this process runs from inside an AppImage, so `$APPIMAGE` / `$ARGV0` describe it.
/// Every child of an AppImage app (a terminal in Cursor's AppImage, say) inherits those
/// variables, so their presence alone says nothing about this process.
pub fn running_from_appimage() -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    let appdir = std::env::var_os("APPDIR").map(PathBuf::from);
    std::env::current_exe().is_ok_and(|exe| in_appimage(&exe, appdir.as_deref()))
}

fn linux_stable_exe(
    current: &Path,
    appimage: Option<&Path>,
    appdir: Option<&Path>,
) -> Result<PathBuf> {
    if !in_appimage(current, appdir) {
        // A deb/rpm/tarball binary. An inherited $APPIMAGE belongs to whichever AppImage app
        // started us, and must never be registered as Callimachus.
        return Ok(current.to_path_buf());
    }
    if let Some(image) = appimage.filter(|p| p.is_absolute() && p.is_file()) {
        return Ok(image.to_path_buf());
    }
    let in_appimage_mount = current
        .components()
        .any(|c| c.as_os_str().to_string_lossy().starts_with(".mount_"));
    if in_appimage_mount {
        bail!(
            "Callimachus is running from a temporary AppImage mount ({}). Launch the .AppImage \
             file itself, then try again.",
            current.display()
        );
    }
    Ok(current.to_path_buf())
}

fn is_macos_ephemeral(current: &Path) -> bool {
    is_macos_ephemeral_with(current, volume_is_read_only)
}

/// [`is_macos_ephemeral`] with the read-only probe injected (tests can't mount a disk image).
fn is_macos_ephemeral_with(current: &Path, read_only: impl Fn(&Path) -> bool) -> bool {
    if current
        .components()
        .any(|c| c.as_os_str() == "AppTranslocation")
    {
        return true;
    }
    // A disk image puts the bundle at the volume root: /Volumes/<name>/<App>.app/... An external
    // drive can hold an app there too, permanently, so the shape alone isn't enough: a mounted
    // DMG is read-only (Tauri builds compressed UDZO images), an external drive is not. An app
    // deeper on a drive (e.g. /Volumes/SSD/Applications/...) is always fine.
    let mut parts = current.components();
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (
            Some(Component::RootDir),
            Some(Component::Normal(volumes)),
            Some(Component::Normal(volume)),
            Some(Component::Normal(bundle)),
        ) if volumes == "Volumes" && bundle.to_string_lossy().ends_with(".app") => {
            read_only(&Path::new("/Volumes").join(volume))
        }
        _ => false,
    }
}

/// Whether the volume mounted at `volume` is read-only, as a mounted disk image is. Asks the
/// mount table (`/sbin/mount`) instead of probing the volume, so nothing on it is opened (no
/// removable-volume privacy prompt). Without an answer it says read-only: the safe side, which
/// only asks the user to move the app. Runs only for an app at a volume root.
fn volume_is_read_only(volume: &Path) -> bool {
    let table = std::process::Command::new("/sbin/mount")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    table
        .and_then(|t| mount_flags_read_only(&t, volume))
        .unwrap_or(true)
}

/// From macOS `mount` output (`/dev/disk4s1 on /Volumes/X (apfs, local, read-only, ...)`),
/// whether `volume` is mounted read-only. `None` when it isn't listed.
fn mount_flags_read_only(table: &str, volume: &Path) -> Option<bool> {
    let needle = format!(" on {} (", volume.display());
    table.lines().find_map(|line| {
        let start = line.find(&needle)? + needle.len();
        let end = line.rfind(')').filter(|&e| e >= start)?;
        Some(line[start..end].split(',').any(|f| f.trim() == "read-only"))
    })
}

/// The stable path when there is one, else `current` plus the reason it isn't stable.
fn resolve_exe(current: &Path) -> (PathBuf, Option<String>) {
    match stable_app_exe(current) {
        Ok(p) => (p, None),
        Err(e) => (current.to_path_buf(), Some(format!("{e:#}"))),
    }
}

// ---------------------------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationStatus {
    /// The `/recall` skill file exists.
    pub skill_installed: bool,
    /// Skill exists but its content differs from what this version writes (re-install to update).
    pub skill_outdated: bool,
    /// An MCP server named `callimachus` is registered and points at this app.
    pub mcp_registered: bool,
    /// A Callimachus SessionStart hook is installed (auto-injects project memory).
    pub hook_installed: bool,
    /// The opt-in UserPromptSubmit "proactive recall" hook is installed (injects prior work
    /// before each prompt). Separate from `hook_installed` because it reads every prompt.
    pub proactive_recall_installed: bool,
    /// The opt-in error-playbook hooks are installed (PostToolUse / PostToolUseFailure on
    /// Bash: inject the fix of a recurring error a commit fixed before). Separate because
    /// they run after every shell command.
    pub error_recall_installed: bool,
    /// The `cal` CLI entry point exists and is ours (`~/.local/bin/cal`, or `cal.exe` on
    /// Windows). Powers the `cal` CLI and the VS Code extension.
    pub cal_installed: bool,
    /// The app runs from a permanent location, so it can be registered with agents.
    pub exe_stable: bool,
    /// Claude Code's config dir exists on this machine, so the Claude Code row is worth
    /// showing at all (the setup checklist hides it otherwise).
    pub present: bool,
    /// Why it can't be registered from where it runs (e.g. "Move Callimachus to /Applications
    /// first"). Install refuses while this is set.
    pub exe_problem: Option<String>,
    /// Why `cal` isn't installed (another program owns `~/.local/bin/cal`, or Windows couldn't
    /// write `cal.exe` into a per-machine install dir). Everything else still works without it.
    pub cal_problem: Option<String>,
    /// Advice when `cal` is installed but a bare `cal` won't reach it from the user's shell
    /// (e.g. `~/.local/bin` isn't on PATH and `/usr/bin/cal`, the calendar, wins).
    pub path_hint: Option<String>,
    pub skill_path: String,
    pub config_path: String,
    /// The Callimachus Claude Code plugin (`callimachus@<marketplace>`) is enabled in Claude
    /// Code's user settings and our `cal` is installed, so the plugin provides the skill, the
    /// MCP server and the base hooks, and the app installs only `cal` (which the plugin's
    /// hooks run). See [`plugin_takes_over`].
    pub plugin_managed: bool,
}

/// Current integration state. `app_exe` is the running app's executable path
/// (`std::env::current_exe()`); it's resolved to the stable path internally.
///
/// Never blocks on the PATH probe: the first call starts it in the background and reports no
/// `path_hint` until it's done.
pub fn status(app_exe: &Path) -> IntegrationStatus {
    build_status(app_exe, None, false)
}

fn build_status(
    app_exe: &Path,
    cal_error: Option<String>,
    wait_for_path: bool,
) -> IntegrationStatus {
    let (exe, exe_problem) = resolve_exe(app_exe);
    let cal = cal_state(&exe);

    let skill = skill_path().ok();
    let expected_skill = render_skill(&cli_entrypoint(&exe, &cal));
    let (skill_installed, skill_outdated) = match skill.as_ref().map(std::fs::read_to_string) {
        Some(Ok(existing)) => (true, existing != expected_skill),
        _ => (false, false),
    };

    let config = claude_config_path().ok();
    let mcp_registered = config
        .as_deref()
        .is_some_and(|c| mcp_registered_json(c, &exe));

    let settings = settings_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());
    // Only while it can take over (see [`plugin_takes_over`]): without `cal` the app keeps
    // its own install, and Settings says so through `cal_problem`.
    let plugin_managed = settings.as_ref().is_some_and(plugin_manages) && cal.installed;
    let hook_installed = settings
        .as_ref()
        .is_some_and(|v| event_has_our_hook(v, SESSION_START_HOOK.event, &exe));
    let proactive_recall_installed = settings
        .as_ref()
        .is_some_and(|v| event_has_our_hook(v, PROACTIVE_RECALL_HOOK.event, &exe));
    let error_recall_installed = settings
        .as_ref()
        .is_some_and(|v| event_has_our_hook(v, ERROR_RECALL_HOOK.event, &exe));

    let path_hint = path_hint(&cal, &exe, wait_for_path);
    let cal_problem = if cal.installed {
        None
    } else {
        cal_error.or(cal.problem)
    };

    IntegrationStatus {
        present: claude_config_dir().map(|d| d.is_dir()).unwrap_or(false),
        skill_installed,
        skill_outdated,
        mcp_registered,
        hook_installed,
        proactive_recall_installed,
        error_recall_installed,
        cal_installed: cal.installed,
        exe_stable: exe_problem.is_none(),
        exe_problem,
        cal_problem,
        path_hint,
        skill_path: skill.map(|p| p.display().to_string()).unwrap_or_default(),
        config_path: config.map(|p| p.display().to_string()).unwrap_or_default(),
        plugin_managed,
    }
}

// ---------------------------------------------------------------------------------------------
// The Claude Code plugin
// ---------------------------------------------------------------------------------------------

/// How Claude Code records the Callimachus plugin in `enabledPlugins`: `<plugin>@<marketplace>`,
/// both named `callimachus` (plugins/claude-code and .claude-plugin/marketplace.json).
#[cfg(test)]
const PLUGIN_ID: &str = "callimachus@callimachus";

/// The plugin part of [`PLUGIN_ID`]. The same plugin installed from a fork's marketplace or a
/// directory listing (`callimachus@claude-plugins-official`) carries the same hooks and server,
/// so any marketplace counts.
const PLUGIN_NAME: &str = "callimachus";

/// True when Claude Code's user settings enable the Callimachus plugin and the plugin can stand
/// in for the app's own skill, MCP entry and base hooks.
///
/// Both at once would double up: the plugin's skill is namespaced (`/callimachus:recall`) next
/// to the personal `/recall`, both hooks would inject the same memory, and both MCP servers
/// would start, because Claude Code dedupes a plugin server against a configured one by its
/// command, and `npx -y callimachus-mcp` is not `<app> --mcp`
/// (https://code.claude.com/docs/en/mcp, "Scope hierarchy and deduplication").
///
/// Unix only: the plugin's hooks run `~/.local/bin/cal` through `sh`, which a Windows install
/// has neither of (`cal.exe` sits next to the app, and hooks fall back to PowerShell without
/// Git Bash), so on Windows the app keeps installing everything itself.
fn plugin_manages(settings: &Value) -> bool {
    cfg!(unix) && plugin_enabled_in(settings)
}

/// `enabledPlugins["callimachus@<any marketplace>"] == true` in a parsed settings document.
fn plugin_enabled_in(settings: &Value) -> bool {
    settings
        .get("enabledPlugins")
        .and_then(Value::as_object)
        .is_some_and(|m| {
            m.iter().any(|(id, on)| {
                id.split_once('@')
                    .is_some_and(|(name, _)| name == PLUGIN_NAME)
                    && on.as_bool() == Some(true)
            })
        })
}

/// [`plugin_manages`] for the user's `settings.json`. An unreadable or missing file means no.
pub fn claude_plugin_managed() -> bool {
    settings_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .is_some_and(|v| plugin_manages(&v))
}

/// Whether the plugin takes over from the app for `exe`: enabled, and the `cal` link its hooks
/// run (`~/.local/bin/cal`) is ours and live. Without that link the plugin's guarded hooks
/// exit silently (or run another program's `cal`), so handing over would leave Claude Code
/// with no memory at all; the app keeps its own install and Settings shows the `cal` problem.
fn plugin_takes_over(exe: &Path) -> bool {
    claude_plugin_managed() && cal_state(exe).installed
}

/// Claude Code's files the hand-over edits, as paths (tests point them at a temp dir).
struct ClaudeFiles {
    settings: PathBuf,
    skill: PathBuf,
    config: PathBuf,
}

fn claude_files() -> Result<ClaudeFiles> {
    Ok(ClaudeFiles {
        settings: settings_path()?,
        skill: skill_path()?,
        config: claude_config_path()?,
    })
}

/// File name of the record of what [`hand_over_in`] removed, next to the index.
const PLUGIN_HANDOVER_MARKER: &str = "plugin-handover.json";

/// What the app removed when the plugin took over, so turning the plugin off puts back exactly
/// that and nothing the user deleted by hand. Every field defaults, so any version parses it.
#[derive(Debug, Default, PartialEq, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct HandedOver {
    /// Base hook events (`SessionStart`, `PreCompact`, `SessionEnd`) that held our hook.
    hooks: Vec<String>,
    /// The untouched `/recall` skill in Claude Code's skills dir.
    skill: bool,
    /// The app's `mcpServers.callimachus` entry (`<app> --mcp`).
    mcp: bool,
}

impl HandedOver {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Fold in an earlier record: a second hand-over (the user re-ran Install) finds less to
    /// remove and must not forget what the first one took.
    fn merge(&mut self, earlier: HandedOver) {
        for event in earlier.hooks {
            if !self.hooks.contains(&event) {
                self.hooks.push(event);
            }
        }
        self.skill |= earlier.skill;
        self.mcp |= earlier.mcp;
    }
}

/// `<index dir>/plugin-handover.json`, beside the other integration markers.
fn plugin_handover_marker_path() -> PathBuf {
    crate::db::default_index_path()
        .parent()
        .map(|d| d.join(PLUGIN_HANDOVER_MARKER))
        .unwrap_or_else(|| PathBuf::from(PLUGIN_HANDOVER_MARKER))
}

fn read_handover(path: &Path) -> Result<Option<HandedOver>> {
    let Some(text) = read_config(path)? else {
        return Ok(None);
    };
    serde_json::from_str(&text)
        .map(Some)
        .with_context(|| format!("{} is not a valid marker", path.display()))
}

/// The app's own pieces the plugin would replace, as installed right now.
fn handover_candidates(files: &ClaudeFiles, exe: &Path) -> Result<HandedOver> {
    let parse = |path: &Path| -> Result<Option<Value>> {
        read_config(path)?
            .map(|text| parse_json_object(&text, path))
            .transpose()
    };
    let settings = parse(&files.settings)?;
    let config = parse(&files.config)?;
    Ok(HandedOver {
        hooks: BASE_HOOKS
            .iter()
            .filter(|spec| {
                settings
                    .as_ref()
                    .is_some_and(|v| event_has_our_hook(v, spec.event, exe))
            })
            .map(|spec| spec.event.to_string())
            .collect(),
        skill: read_config(&files.skill)?.is_some_and(|text| is_untouched_skill(&text)),
        mcp: is_app_mcp_server(config.as_ref().and_then(|v| v.get("mcpServers"))),
    })
}

/// Remove what the plugin now provides, where the app put it: our base hooks (SessionStart,
/// PreCompact, SessionEnd, and retired ones; the opt-in proactive-recall and error-playbook
/// hooks stay, since the plugin does not ship them), the `/recall` skill when nobody edited it, and the MCP entry
/// the app registered (`<app> --mcp`; an entry the user wrote by hand stays theirs). The
/// record goes to `marker` first, so a removal cut short still restores. Returns whether
/// anything changed.
fn hand_over_in(files: &ClaudeFiles, marker: &Path, exe: &Path) -> Result<bool> {
    let mut found = handover_candidates(files, exe)?;
    if !found.is_empty() {
        if let Some(earlier) = read_handover(marker).unwrap_or(None) {
            found.merge(earlier);
        }
        write_atomic(marker, serde_json::to_string_pretty(&found)?.as_bytes())?;
    }
    let mut changed = update_json_file(&files.settings, false, |obj| {
        strip_base_hooks(obj, exe);
        Ok(())
    })?;
    if read_config(&files.skill)?.is_some_and(|text| is_untouched_skill(&text)) {
        remove_if_exists(&files.skill)?;
        // An empty `skills/recall/` would still show up in a listing of the user's skills.
        if let Some(dir) = files.skill.parent() {
            let _ = std::fs::remove_dir(dir); // fails, harmlessly, when not empty
        }
        changed = true;
    }
    changed |= remove_app_mcp_json(&files.config)?;
    Ok(changed)
}

/// The reverse of [`hand_over_in`] once the plugin is off: put back what `marker` says the
/// app removed, aimed at `exe`, then drop the marker. A piece already there again (the user
/// re-ran Install, or wrote their own skill or MCP entry) is left as it is. Returns whether
/// anything changed.
fn take_back_in(files: &ClaudeFiles, marker: &Path, exe: &Path, cli: &Path) -> Result<bool> {
    let Some(handed) = read_handover(marker)? else {
        return Ok(false);
    };
    let specs: Vec<&HookSpec> = BASE_HOOKS
        .into_iter()
        .filter(|spec| handed.hooks.iter().any(|e| e == spec.event))
        .collect();
    let mut changed = false;
    if !specs.is_empty() {
        changed |= update_json_file(&files.settings, true, |obj| {
            for spec in &specs {
                if our_hook_command(obj, spec.event, exe).is_none() {
                    upsert_hook(obj, spec, &spec_command(exe, spec, spec.subcommand), exe)?;
                }
            }
            Ok(())
        })?;
    }
    if handed.skill && read_config(&files.skill)?.is_none() {
        write_skill_at(&files.skill, cli)?;
        changed = true;
    }
    if handed.mcp {
        let present = read_config(&files.config)?
            .map(|text| parse_json_object(&text, &files.config))
            .transpose()?
            .is_some_and(|v| v.get("mcpServers").and_then(|m| m.get(MCP_NAME)).is_some());
        if !present {
            register_mcp_json(&files.config, exe)?;
            changed = true;
        }
    }
    remove_if_exists(marker)?;
    Ok(changed)
}

/// The plugin state the last hand-over sync acted on in this process (None before the first).
static PLUGIN_SYNCED: std::sync::Mutex<Option<bool>> = std::sync::Mutex::new(None);

fn note_plugin_synced(takes_over: bool) {
    if let Ok(mut last) = PLUGIN_SYNCED.lock() {
        *last = Some(takes_over);
    }
}

/// Hand over to the plugin, or take back from it, to match `takes_over`.
fn sync_plugin_handover(exe: &Path, takes_over: bool) -> Result<bool> {
    note_plugin_synced(takes_over);
    let files = claude_files()?;
    let marker = plugin_handover_marker_path();
    if takes_over {
        hand_over_in(&files, &marker, exe)
    } else {
        let cli = cli_entrypoint(exe, &cal_state(exe));
        take_back_in(&files, &marker, exe, &cli)
    }
}

/// The watcher's minute tick: the user can turn the plugin on or off (`/plugin`) while the app
/// keeps running, and until the next launch both sets of hooks would inject memory and both
/// MCP servers start, or, the other way, nothing would. So re-read `enabledPlugins` (one small
/// file) and, only when the answer changed since the last sync, hand over or take back. A
/// failed sync is not retried until the plugin state changes again or the app relaunches, so
/// a broken settings.json costs one log line, not one a minute.
pub fn plugin_handover_tick(current_exe: &Path) -> Result<bool> {
    let Ok(exe) = stable_app_exe(current_exe) else {
        return Ok(false);
    };
    let takes_over = plugin_takes_over(&exe);
    let unchanged = PLUGIN_SYNCED
        .lock()
        .map(|last| *last == Some(takes_over))
        .unwrap_or(true);
    if unchanged {
        return Ok(false);
    }
    sync_plugin_handover(&exe, takes_over)
}

/// Strip our hooks from every event except the opt-in ones (UserPromptSubmit, and
/// PostToolUse / PostToolUseFailure).
fn strip_base_hooks(obj: &mut Map<String, Value>, exe: &Path) {
    for event in ALL_HOOK_EVENTS.into_iter().filter(|e| !is_opt_in_event(e)) {
        strip_our_hooks(obj, event, exe);
    }
}

// ---------------------------------------------------------------------------------------------
// Install / uninstall / refresh
// ---------------------------------------------------------------------------------------------

/// Install (or refresh) the skill, the MCP registration, the `cal` CLI and the Claude Code hooks
/// (SessionStart injects each repo's memory; PreCompact snapshots the session before
/// compaction and keeps its summary; SessionEnd queues the session for auto-distill).
/// Idempotent. With the Claude Code plugin enabled ([`plugin_takes_over`]) it installs only
/// `cal` and the shared `~/.agents/skills/` copy of the skill, and removes the app's own
/// copies of what the plugin provides.
///
/// Refuses to run from a temporary location (see [`stable_app_exe`]). A `cal` failure is
/// reported in `cal_problem` rather than aborting halfway: the hooks and MCP entry launch the app
/// directly, so nothing else depends on `cal`.
///
/// `cal` only ever goes into `~/.local/bin`. A system-wide link (`/usr/local/bin/cal`) would
/// shadow the calendar, `cal(1)`, for every account on the machine, so when `~/.local/bin`
/// doesn't win the PATH lookup the returned status carries a `path_hint` instead.
pub fn install(app_exe: &Path) -> Result<IntegrationStatus> {
    let exe = stable_app_exe(app_exe)?;
    let cal_error = install_cal(&exe).err().map(|e| format!("{e:#}"));
    let cli = cli_entrypoint(&exe, &cal_state(&exe));
    // Codex and Gemini CLI read this copy whoever serves Claude Code.
    write_shared_skill(&cli)?;
    if plugin_takes_over(&exe) {
        // The plugin brings the skill, the MCP server and the hooks; they run `cal`.
        sync_plugin_handover(&exe, true)?;
        return Ok(build_status(app_exe, cal_error, true));
    }
    // A full install supersedes any earlier hand-over record.
    remove_if_exists(&plugin_handover_marker_path())?;
    note_plugin_synced(false);
    write_skill_at(&skill_path()?, &cli)?;
    register_mcp_json(&claude_config_path()?, &exe)?;
    install_hook(&exe)?;
    Ok(build_status(app_exe, cal_error, true))
}

/// Toggle the opt-in proactive-recall hook (UserPromptSubmit -> `recall-now`) on its own,
/// independent of the base integration. It reads every prompt, so it stays off until the user
/// asks for it. Disabling strips only our UserPromptSubmit hook. Idempotent.
pub fn set_proactive_recall(app_exe: &Path, enabled: bool) -> Result<IntegrationStatus> {
    let path = settings_path()?;
    if enabled {
        let exe = stable_app_exe(app_exe)?;
        let command = hook_command(&exe, PROACTIVE_RECALL_HOOK.subcommand);
        update_json_file(&path, true, |obj| {
            upsert_hook(obj, &PROACTIVE_RECALL_HOOK, &command, &exe)
        })?;
    } else {
        let (exe, _) = resolve_exe(app_exe);
        update_json_file(&path, false, |obj| {
            strip_our_hooks(obj, PROACTIVE_RECALL_HOOK.event, &exe);
            Ok(())
        })?;
    }
    Ok(status(app_exe))
}

/// Toggle the opt-in error-playbook hooks (PostToolUse and PostToolUseFailure on Bash ->
/// `recall-error`) on their own, independent of the base integration. They run after every
/// shell command Claude runs, so they stay off until the user asks. Both events, because a
/// command that exits non-zero arrives as PostToolUseFailure, and one that prints an error
/// but exits 0 (`cargo test 2>&1 | tail`) as PostToolUse. Disabling strips only those.
/// Idempotent.
pub fn set_error_recall(app_exe: &Path, enabled: bool) -> Result<IntegrationStatus> {
    set_error_recall_in(&settings_path()?, app_exe, enabled)?;
    Ok(status(app_exe))
}

fn set_error_recall_in(path: &Path, app_exe: &Path, enabled: bool) -> Result<bool> {
    if enabled {
        let exe = stable_app_exe(app_exe)?;
        update_json_file(path, true, |obj| {
            for spec in ERROR_RECALL_HOOKS {
                upsert_hook(obj, spec, &hook_command(&exe, spec.subcommand), &exe)?;
            }
            Ok(())
        })
    } else {
        let (exe, _) = resolve_exe(app_exe);
        update_json_file(path, false, |obj| {
            for spec in ERROR_RECALL_HOOKS {
                strip_our_hooks(obj, spec.event, &exe);
            }
            Ok(())
        })
    }
}

/// Remove the skill file, the Claude Code MCP registration, every Callimachus hook (including
/// ones older versions installed), and our `cal` link(s). Leaves the rest of each config intact
/// and never removes a `cal` that isn't ours. Tries every step, then reports any failures.
pub fn uninstall() -> Result<()> {
    let mut errors = Vec::new();
    if let Err(e) = uninstall_pieces() {
        errors.push(format!("{e:#}"));
    }
    // After a deliberate uninstall, the next Install adds SessionEnd itself, so the once-only
    // record has nothing left to guard. (The Windows uninstaller path keeps it: a restore from
    // its marker must not re-add a SessionEnd hook the user had deleted.)
    if let Err(e) = remove_if_exists(&session_end_marker_path()) {
        errors.push(format!("SessionEnd marker: {e:#}"));
    }
    combine_errors(errors)
}

/// [`uninstall`] without the SessionEnd marker cleanup.
fn uninstall_pieces() -> Result<()> {
    let exe = current_stable_exe();
    let mut errors = Vec::new();
    let mut attempt = |what: &str, result: Result<()>| {
        if let Err(e) = result {
            errors.push(format!("{what}: {e:#}"));
        }
    };
    attempt("skill", skill_path().and_then(|p| remove_if_exists(&p)));
    attempt(
        "shared skill",
        shared_skill_path().and_then(|p| remove_untouched_skill(&p)),
    );
    attempt(
        "Claude Code MCP entry",
        claude_config_path().and_then(|c| remove_mcp_json(&c)),
    );
    attempt(
        "hooks",
        settings_path().and_then(|p| {
            update_json_file(&p, false, |obj| {
                for event in ALL_HOOK_EVENTS {
                    strip_our_hooks(obj, event, &exe);
                }
                Ok(())
            })
            .map(|_| ())
        }),
    );
    attempt("cal", remove_cal());
    combine_errors(errors)
}

// ---------------------------------------------------------------------------------------------
// Uninstaller marker (survives a reinstall)
// ---------------------------------------------------------------------------------------------

/// File name of the record [`uninstall_all_with_marker`] leaves next to the index.
const REMOVED_MARKER: &str = "integration-removed.json";

/// What [`uninstall_all_with_marker`] found installed, so the next launch can put back exactly
/// that. Every field defaults, so a marker from an older or newer build still parses.
#[derive(Debug, Default, PartialEq, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct RemovedIntegration {
    /// Base Claude Code hook events that held our hook (`SessionStart`, `PreCompact`,
    /// `SessionEnd`).
    claude_hooks: Vec<String>,
    /// The opt-in UserPromptSubmit hook.
    proactive_recall: bool,
    /// The opt-in PostToolUse / PostToolUseFailure error-playbook hooks.
    error_recall: bool,
    /// `mcpServers.callimachus` in Claude Code's config.
    claude_mcp: bool,
    /// Ids (see [`agent_targets`]) of the other agents whose config held our MCP entry.
    agents: Vec<String>,
    /// Ids (`codex`, `gemini`) of the agents whose hooks file held our memory hooks.
    agent_hooks: Vec<String>,
    /// The `cal` entry point.
    cal: bool,
    /// The `/recall` skill.
    skill: bool,
}

impl RemovedIntegration {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Fold in an earlier marker: a second uninstall run before any launch (a failed install
    /// retried) finds nothing left and must not erase the first one's record.
    fn merge(&mut self, earlier: RemovedIntegration) {
        for event in earlier.claude_hooks {
            if !self.claude_hooks.contains(&event) {
                self.claude_hooks.push(event);
            }
        }
        for id in earlier.agents {
            if !self.agents.contains(&id) {
                self.agents.push(id);
            }
        }
        for id in earlier.agent_hooks {
            if !self.agent_hooks.contains(&id) {
                self.agent_hooks.push(id);
            }
        }
        self.proactive_recall |= earlier.proactive_recall;
        self.error_recall |= earlier.error_recall;
        self.claude_mcp |= earlier.claude_mcp;
        self.cal |= earlier.cal;
        self.skill |= earlier.skill;
    }
}

/// `<index dir>/integration-removed.json`. The data dir outlives the app's uninstaller.
fn removed_marker_path() -> PathBuf {
    let index = crate::db::default_index_path();
    index
        .parent()
        .map(|d| d.join(REMOVED_MARKER))
        .unwrap_or_else(|| PathBuf::from(REMOVED_MARKER))
}

fn read_marker(path: &Path) -> Result<Option<RemovedIntegration>> {
    let Some(text) = read_config(path)? else {
        return Ok(None);
    };
    serde_json::from_str(&text)
        .map(Some)
        .with_context(|| format!("{} is not a valid marker", path.display()))
}

/// What's installed right now, for the marker. `exe` is the app being removed.
fn detect_installed(exe: &Path) -> RemovedIntegration {
    let settings = settings_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());
    let has_hook = |event: &str| {
        settings
            .as_ref()
            .is_some_and(|v| event_has_our_hook(v, event, exe))
    };
    let has_mcp_json = |cfg: &Path| {
        std::fs::read_to_string(cfg)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .is_some_and(|v| v.get("mcpServers").and_then(|m| m.get(MCP_NAME)).is_some())
    };
    let has_mcp_toml = |cfg: &Path| {
        std::fs::read_to_string(cfg)
            .ok()
            .and_then(|s| s.parse::<toml_edit::DocumentMut>().ok())
            .is_some_and(|doc| {
                doc.get("mcp_servers")
                    .and_then(|i| i.get(MCP_NAME))
                    .is_some()
            })
    };
    let agents = agent_targets()
        .unwrap_or_default()
        .into_iter()
        .filter(|a| match a.fmt {
            Fmt::Json => has_mcp_json(&a.config),
            Fmt::Toml => has_mcp_toml(&a.config),
        })
        .map(|a| a.id.to_string())
        .collect();
    let agent_hooks = hook_targets()
        .unwrap_or_default()
        .iter()
        .filter(|t| agent_hooks_present(t, exe))
        .map(|t| t.id.to_string())
        .collect();
    RemovedIntegration {
        claude_hooks: BASE_HOOKS
            .iter()
            .filter(|spec| has_hook(spec.event))
            .map(|spec| spec.event.to_string())
            .collect(),
        proactive_recall: has_hook(PROACTIVE_RECALL_HOOK.event),
        error_recall: has_hook(ERROR_RECALL_HOOK.event),
        claude_mcp: claude_config_path().is_ok_and(|c| has_mcp_json(&c)),
        agents,
        agent_hooks,
        cal: cal_state(exe).installed,
        skill: skill_path().is_ok_and(|p| p.is_file()),
    }
}

/// What `--uninstall-integration` (run by the Windows uninstaller) does: [`uninstall`] plus
/// [`uninstall_agents`], after recording in `<index dir>/integration-removed.json` which pieces
/// were there.
///
/// Why the record: Tauri's NSIS installer runs the old uninstaller (without `/UPDATE`) whenever a
/// newer or older setup.exe is run by hand, so this removal also happens on an ordinary manual
/// upgrade. The next launch calls [`restore_from_marker`] and puts back exactly what was removed.
/// The marker is written before anything is removed, so a removal cut short still restores.
pub fn uninstall_all_with_marker() -> Result<()> {
    let exe = current_stable_exe();
    let path = removed_marker_path();
    let mut found = detect_installed(&exe);
    let mut errors = Vec::new();
    match read_marker(&path) {
        Ok(Some(earlier)) => found.merge(earlier),
        Ok(None) => {}
        Err(e) => errors.push(format!("previous marker: {e:#}")),
    }
    if !found.is_empty() {
        let json = serde_json::to_string_pretty(&found)?;
        if let Err(e) = write_atomic(&path, json.as_bytes()) {
            errors.push(format!("marker: {e:#}"));
        }
    }
    if let Err(e) = uninstall_pieces() {
        errors.push(format!("{e:#}"));
    }
    if let Err(e) = uninstall_agents() {
        errors.push(format!("other agents: {e:#}"));
    }
    // Windows hook commands carry no missing-binary guard, so hooks left behind by a removed
    // app would error in Codex and Gemini CLI at every session start.
    if let Err(e) = uninstall_all_agent_hooks(&exe) {
        errors.push(format!("{e:#}"));
    }
    combine_errors(errors)
}

/// Undo [`uninstall_all_with_marker`] after a reinstall: when its marker exists, re-install
/// exactly the pieces it lists against the running app, then delete it. Returns whether a marker
/// was applied. Waits (keeps the marker) while the app runs from a temporary location.
///
/// The marker is deleted even when a piece fails (a `cal.exe` that a per-machine install can't
/// write, say), so a permanent failure isn't retried at every launch; the errors are returned
/// for the log and Settings still offers Install.
pub fn restore_from_marker(current_exe: &Path) -> Result<bool> {
    let path = removed_marker_path();
    if !path.exists() {
        return Ok(false);
    }
    let Ok(exe) = stable_app_exe(current_exe) else {
        return Ok(false);
    };
    let removed = match read_marker(&path) {
        Ok(Some(r)) => r,
        Ok(None) => return Ok(false),
        Err(e) => {
            let _ = remove_if_exists(&path); // unreadable: nothing to restore from, ever
            return Err(e);
        }
    };

    let mut errors = Vec::new();
    let mut attempt = |what: &str, result: Result<()>| {
        if let Err(e) = result {
            errors.push(format!("{what}: {e:#}"));
        }
    };
    // `cal` first: the skill names whichever CLI entry point exists.
    if removed.cal {
        attempt("cal", install_cal(&exe));
    }
    // No plugin check here: the uninstaller that leaves this marker runs only on Windows,
    // where the plugin never takes over, and anywhere else the plugin sync that
    // `refresh_installed` runs right after this hands the pieces over again.
    if removed.skill {
        let cli = cli_entrypoint(&exe, &cal_state(&exe));
        attempt(
            "skill",
            skill_path()
                .and_then(|p| write_skill_at(&p, &cli))
                .and_then(|()| write_shared_skill(&cli)),
        );
    }
    if removed.claude_mcp {
        attempt(
            "Claude Code MCP entry",
            claude_config_path().and_then(|c| register_mcp_json(&c, &exe)),
        );
    }
    let mut hooks: Vec<&HookSpec> = BASE_HOOKS
        .into_iter()
        .filter(|spec| removed.claude_hooks.iter().any(|e| e == spec.event))
        .collect();
    if removed.proactive_recall {
        hooks.push(&PROACTIVE_RECALL_HOOK);
    }
    if removed.error_recall {
        hooks.extend(ERROR_RECALL_HOOKS);
    }
    if !hooks.is_empty() {
        attempt(
            "hooks",
            settings_path().and_then(|p| {
                update_json_file(&p, true, |obj| apply_hooks(obj, &hooks, &exe)).map(|_| ())
            }),
        );
    }
    match agent_targets() {
        Ok(agents) => {
            // Only agents still in use; a config dir the user removed meanwhile stays gone.
            for a in agents
                .into_iter()
                .filter(|a| a.dir.is_dir() && removed.agents.iter().any(|id| id == a.id))
            {
                let result = match a.fmt {
                    Fmt::Json => register_mcp_json(&a.config, &exe),
                    Fmt::Toml => register_mcp_toml(&a.config, &exe),
                };
                attempt(a.label, result);
            }
        }
        Err(e) => attempt("agents", Err(e)),
    }
    match hook_targets() {
        Ok(targets) => {
            for t in targets
                .into_iter()
                .filter(|t| t.dir.is_dir() && removed.agent_hooks.iter().any(|id| id == t.id))
            {
                attempt(
                    t.label,
                    install_hooks_into(&t.file, t.specs, &exe).map(|_| ()),
                );
            }
        }
        Err(e) => attempt("agent hooks", Err(e)),
    }
    attempt("marker", remove_if_exists(&path));
    combine_errors(errors).map(|()| true)
}

/// Bring an existing installation up to date. Meant to run once at app startup, on a background
/// thread. It only touches pieces the user already installed, and never creates new config
/// (except to undo the Windows uninstaller's removal, see [`restore_from_marker`]):
///
/// - Rewrites our hook commands in place (the missing-binary guard; a dead or `cal`-link target
///   moves to this app), adds a matcher or timeout only where an older version left one out,
///   and drops the retired SubagentStop hook. A matcher or timeout the user changed is kept, and
///   a hook pointing at another live Callimachus binary (a dev build, a second install) keeps
///   its target.
/// - Re-points MCP entries whose registered app path no longer exists (an AppImage mount from a
///   previous run, an app that was moved).
/// - Re-points a dangling `cal` link (Unix) or refreshes a stale `cal.exe` copy (Windows).
/// - Re-renders the skill only if it's an untouched bundled text (this version's or an older
///   one's); an edited skill is left for the "skill outdated" prompt.
///
/// Does nothing while the app runs from a temporary location. Returns whether anything changed.
pub fn refresh_installed(current_exe: &Path) -> Result<bool> {
    let Ok(exe) = stable_app_exe(current_exe) else {
        return Ok(false); // never re-point anything at a temporary path
    };

    let mut changed = false;
    let mut errors = Vec::new();
    let mut record = |what: &str, result: Result<bool>| match result {
        Ok(c) => changed |= c,
        Err(e) => errors.push(format!("{what}: {e:#}")),
    };

    // Before the refresh, so restored pieces also get it.
    record("restore after reinstall", restore_from_marker(current_exe));
    // The login-shell PATH probe sources the user's rc files (slow inits, keychain or TCC
    // prompts), and only the `cal` PATH hint reads it, so skip it for users without `cal`.
    if cfg!(unix) && cal_state(&exe).installed {
        warm_shell_path(); // so the settings screen's first status() has a PATH answer
    }

    record(
        "hooks",
        settings_path()
            .and_then(|p| update_json_file(&p, false, |obj| refresh_hooks_in(obj, &exe))),
    );
    // `cal` before the plugin check: a dangling link re-pointed here is what the plugin's
    // hooks run, and what decides whether it can take over.
    record("cal", refresh_cal(&exe));
    // With the Claude Code plugin enabled, the app's copies would double-inject memory and
    // start a second MCP server, so they go; once it is off again, they come back. The
    // watcher's minute tick repeats this when the plugin state changes while the app runs.
    let plugin = plugin_takes_over(&exe);
    record("Claude Code plugin", sync_plugin_handover(&exe, plugin));
    if !plugin {
        record("SessionEnd hook", add_session_end_hook_once(&exe));
        record(
            "Claude Code MCP entry",
            claude_config_path().and_then(|c| refresh_mcp_json(&c, &exe)),
        );
    }
    match agent_targets() {
        Ok(agents) => {
            for agent in agents {
                let result = match agent.fmt {
                    Fmt::Json => refresh_mcp_json(&agent.config, &exe),
                    Fmt::Toml => refresh_mcp_toml(&agent.config, &exe),
                };
                record(agent.label, result);
            }
        }
        Err(e) => record("agents", Err(e)),
    }
    record("Codex and Gemini CLI hooks", refresh_agent_hooks(&exe));
    // The shared copy is refreshed in plugin mode too: the plugin replaces only Claude's.
    let cli = cli_entrypoint(&exe, &cal_state(&exe));
    record(
        "shared skill",
        shared_skill_path().and_then(|p| refresh_skill_at(&p, &cli)),
    );
    if !plugin {
        record(
            "skill",
            skill_path().and_then(|p| refresh_skill_at(&p, &cli)),
        );
    }

    combine_errors(errors).map(|()| changed)
}

fn combine_errors(errors: Vec<String>) -> Result<()> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(errors.join("; ")))
    }
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

// ---------------------------------------------------------------------------------------------
// Skill
// ---------------------------------------------------------------------------------------------

/// The skill line after which the machine-specific `cal` note goes.
const SKILL_CLI_ANCHOR: &str = "**Fallback: the `cal` CLI**";

/// The bundled skill plus a note with the absolute path of this machine's CLI entry point.
/// Agents told to run a bare `cal` otherwise hit the system calendar, `cal(1)`, whenever
/// `~/.local/bin` isn't early on PATH (as on stock macOS).
fn render_skill(cli: &Path) -> String {
    let Some(start) = SKILL_MD.find(SKILL_CLI_ANCHOR) else {
        return SKILL_MD.to_string();
    };
    let line_end = SKILL_MD[start..]
        .find('\n')
        .map(|i| start + i + 1)
        .unwrap_or(SKILL_MD.len());
    let path = cli.display();
    let note = format!(
        "\nOn this machine the CLI is `{path}`. If a bare `cal` isn't found, or prints a calendar \
         / says \"is neither a month number\" (that's the system `cal(1)` winning on PATH), call \
         it by its full path instead, e.g. `\"{path}\" search \"vector index\" -y`.\n"
    );
    format!("{}{note}{}", &SKILL_MD[..line_end], &SKILL_MD[line_end..])
}

/// The command agents should run for the CLI: the `cal` entry point when it's installed, else
/// the app itself (it runs the same subcommands).
fn cli_entrypoint(exe: &Path, cal: &CalState) -> PathBuf {
    if cal.installed {
        cal.path.clone()
    } else {
        exe.to_path_buf()
    }
}

/// The `~/.agents/skills/` copy, written independently of Claude Code's (which the plugin
/// replaces). An edited one (the user's own `recall` skill there) is left alone.
fn write_shared_skill(cli: &Path) -> Result<()> {
    let shared = shared_skill_path()?;
    match read_config(&shared)? {
        Some(existing) if !is_untouched_skill(&existing) => Ok(()),
        _ => write_skill_at(&shared, cli),
    }
}

fn write_skill_at(path: &Path, cli: &Path) -> Result<()> {
    let rendered = render_skill(cli);
    if read_config(path)?.as_deref() == Some(rendered.as_str()) {
        return Ok(());
    }
    write_atomic(path, rendered.as_bytes())
}

/// Remove a skill copy only if it is one of ours, untouched: the shared dir is not ours alone,
/// and a `recall` skill the user wrote there must survive an uninstall.
fn remove_untouched_skill(path: &Path) -> Result<()> {
    match read_config(path)? {
        Some(text) if is_untouched_skill(&text) => remove_if_exists(path),
        _ => Ok(()),
    }
}

/// FNV-1a (64-bit) of every SKILL.md this app has shipped, LF line endings. Older versions wrote
/// the bundled text as is, so an upgraded install holds one of these byte for byte.
///
/// When resources/recall/SKILL.md changes, add the new text's hash here (the
/// `every_shipped_skill_is_recognized` test fails until you do and prints it), so the next
/// version still recognizes this one's copy as untouched.
const SHIPPED_SKILL_HASHES: &[u64] = &[
    0xd6e258e329780d31, // 4fa22ed
    0x8c7c2be54179941b, // fc74698
    0xec43f5d8cfd77fe2, // 8013e6a
    0x99848911a7d65652, // 1d71c8c
    0x3193db540aed0067, // 6d1100d
    0x1bcddf4e180eb040, // f4394bf
    0x5047d4f99f7147e9, // fe1902e
    0xd572be1865981676, // version: 1.1.0 (first with the rendered CLI note)
    0x29e126dbe1770847, // version: 1.2.0 (Batch D: prompts, fallbacks, digest note)
    0x57df09b3095b9add, // version: 1.2.1 (Batch G: recall results under `facts` when structured)
    0xd32ed8411a5ed529, // version: 1.2.2 (Batch H: facts marked (repo), threads_for_file with a line, `cal why`)
];

/// Stable across builds and platforms, unlike `DefaultHasher`. Only identifies known texts.
fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// Marks the line [`render_skill`] inserts. Keep it recognizable across versions.
const SKILL_CLI_NOTE: &str = "\nOn this machine the CLI is `";

/// `text` without the machine-specific note [`render_skill`] added, if it has one.
fn strip_cli_note(text: &str) -> Option<String> {
    let start = text.find(SKILL_CLI_NOTE)?;
    let line_end = text[start + 1..].find('\n').map(|i| start + 1 + i + 1)?;
    Some(format!("{}{}", &text[..start], &text[line_end..]))
}

/// A skill file nobody edited: a bundled SKILL.md from this or an earlier version, as written
/// raw (older versions) or rendered for some CLI path (this version onward).
fn is_untouched_skill(text: &str) -> bool {
    let text = text.replace("\r\n", "\n");
    let base = strip_cli_note(&text).unwrap_or(text);
    base == SKILL_MD.replace("\r\n", "\n")
        || SHIPPED_SKILL_HASHES.contains(&fnv1a64(base.as_bytes()))
}

/// Bring an untouched copy of a bundled skill (this version's or an older one's) to this
/// version's rendered text. A skill the user edited is left for the "skill outdated" prompt in
/// Settings. Never creates it.
fn refresh_skill_at(path: &Path, cli: &Path) -> Result<bool> {
    let Some(existing) = read_config(path)? else {
        return Ok(false);
    };
    if existing == render_skill(cli) || !is_untouched_skill(&existing) {
        return Ok(false);
    }
    write_skill_at(path, cli)?;
    Ok(true)
}

// ---------------------------------------------------------------------------------------------
// Claude Code hooks
// ---------------------------------------------------------------------------------------------

/// One Claude Code hook that Callimachus installs.
struct HookSpec {
    event: &'static str,
    /// The `cal` subcommand the hook runs.
    subcommand: &'static str,
    /// Group matcher, if any.
    matcher: Option<&'static str>,
    /// Seconds before Claude Code cancels the hook. Its default is 600, long enough to stall a
    /// session start for minutes if the index were ever locked.
    timeout_secs: u64,
    /// The agent's `timeout` field is in milliseconds (Gemini CLI), not seconds.
    millis: bool,
    /// The hook entry's `name` (Gemini CLI lists, enables and disables hooks by it).
    name: Option<&'static str>,
    /// The agent runs hook commands through PowerShell on Windows (Gemini CLI:
    /// `powershell -NoProfile -NonInteractive -Command <cmd>`, gemini-cli `shell-utils.ts`
    /// `getShellConfiguration`), where a command starting with a quoted path is a string
    /// expression and the subcommand after it a syntax error ("Unexpected token 'hook'").
    /// Those commands get the call operator: `& "<exe>" hook --for gemini`.
    powershell: bool,
}

impl HookSpec {
    /// The `timeout` value as the agent reads it.
    fn timeout_value(&self) -> u64 {
        if self.millis {
            self.timeout_secs * 1000
        } else {
            self.timeout_secs
        }
    }
}

/// Injects the repo's memory. Skips `resume`: the earlier injection is still in the resumed
/// transcript, so it would only duplicate. `clear` and `compact` need it again, because the
/// context was dropped or summarized away.
const SESSION_START_HOOK: HookSpec = HookSpec {
    event: "SessionStart",
    subcommand: "hook",
    matcher: Some("startup|clear|compact"),
    timeout_secs: 10,
    millis: false,
    name: None,
    powershell: false,
};

/// Snapshots the live session before its context is compacted.
const PRE_COMPACT_HOOK: HookSpec = HookSpec {
    event: "PreCompact",
    subcommand: "snapshot-hook",
    matcher: None,
    timeout_secs: 10,
    millis: false,
    name: None,
    powershell: false,
};

/// Moves the session that just ended to the front of the auto-distill queue (`cal
/// session-end`: one small write). Claude Code runs SessionEnd hooks in the background with
/// a 1.5 s budget shared by all of them, raised to a hook's own timeout (max 60), so 5 s
/// covers a slow disk without holding anything up.
const SESSION_END_HOOK: HookSpec = HookSpec {
    event: "SessionEnd",
    subcommand: "session-end",
    matcher: None,
    timeout_secs: 5,
    millis: false,
    name: None,
    powershell: false,
};

/// Opt-in: injects "you may have solved this before" notes. Toggled by `set_proactive_recall`.
const PROACTIVE_RECALL_HOOK: HookSpec = HookSpec {
    event: "UserPromptSubmit",
    subcommand: "recall-now",
    matcher: None,
    timeout_secs: 15,
    millis: false,
    name: None,
    powershell: false,
};

/// Opt-in: after a Bash command whose output carries an error a linked commit fixed before
/// (a resolved `issue_clusters` row), injects that fix's playbook line. Toggled by
/// `set_error_recall`. It runs after every shell command, so the timeout is short: output
/// without an error line exits before the index is opened.
const ERROR_RECALL_HOOK: HookSpec = HookSpec {
    event: "PostToolUse",
    subcommand: "recall-error",
    matcher: Some("Bash"),
    timeout_secs: 5,
    millis: false,
    name: None,
    powershell: false,
};

/// [`ERROR_RECALL_HOOK`] for a command that failed: Claude Code reports a non-zero exit as
/// PostToolUseFailure (its `error` field holds the output), not PostToolUse.
const ERROR_RECALL_FAILURE_HOOK: HookSpec = HookSpec {
    event: "PostToolUseFailure",
    subcommand: "recall-error",
    matcher: Some("Bash"),
    timeout_secs: 5,
    millis: false,
    name: None,
    powershell: false,
};

const ERROR_RECALL_HOOKS: [&HookSpec; 2] = [&ERROR_RECALL_HOOK, &ERROR_RECALL_FAILURE_HOOK];

/// Events that hold only the opt-in hooks: kept through a plugin hand-over, re-shaped by
/// Install only when the user already opted in.
fn is_opt_in_event(event: &str) -> bool {
    event == PROACTIVE_RECALL_HOOK.event || ERROR_RECALL_HOOKS.iter().any(|s| s.event == event)
}

/// Codex's SessionStart hook (`~/.codex/hooks.json`). Codex's matcher is a regex over the
/// same sources as Claude Code's (`startup|resume|clear|compact|fork`), and its `timeout` is
/// in seconds.
const CODEX_SESSION_START_HOOK: HookSpec = HookSpec {
    event: "SessionStart",
    subcommand: "hook --for codex",
    matcher: Some("startup|clear|compact"),
    timeout_secs: 10,
    millis: false,
    name: None,
    powershell: false,
};

/// Codex's PreCompact hook: a rolling snapshot of the session (its payload has no summary).
const CODEX_PRE_COMPACT_HOOK: HookSpec = HookSpec {
    event: "PreCompact",
    subcommand: "snapshot-hook --for codex",
    matcher: None,
    timeout_secs: 10,
    millis: false,
    name: None,
    powershell: false,
};

/// Codex's SessionEnd hook. Codex gives SessionEnd 1 s by default and at most 3 s
/// (developers.openai.com/codex/hooks, checked 2026-11-06); the write takes milliseconds.
const CODEX_SESSION_END_HOOK: HookSpec = HookSpec {
    event: "SessionEnd",
    subcommand: "session-end --for codex",
    matcher: None,
    timeout_secs: 3,
    millis: false,
    name: None,
    powershell: false,
};

/// Gemini CLI's SessionStart hook (`~/.gemini/settings.json`). Lifecycle matchers there are
/// exact strings, not regexes, so `*` takes every source and `cal hook` itself stays silent
/// on `resume` (the resumed history already holds the earlier injection). Timeouts are in
/// milliseconds.
const GEMINI_SESSION_START_HOOK: HookSpec = HookSpec {
    event: "SessionStart",
    subcommand: "hook --for gemini",
    matcher: Some("*"),
    timeout_secs: 10,
    millis: true,
    name: Some("callimachus-memory"),
    powershell: true,
};

/// Gemini CLI's SessionEnd hook. Gemini does not wait for it, so the timeout only bounds a
/// stuck process.
const GEMINI_SESSION_END_HOOK: HookSpec = HookSpec {
    event: "SessionEnd",
    subcommand: "session-end --for gemini",
    matcher: Some("*"),
    timeout_secs: 5,
    millis: true,
    name: Some("callimachus-session-end"),
    powershell: true,
};

const CODEX_HOOKS: [&HookSpec; 3] = [
    &CODEX_SESSION_START_HOOK,
    &CODEX_PRE_COMPACT_HOOK,
    &CODEX_SESSION_END_HOOK,
];
const GEMINI_HOOKS: [&HookSpec; 2] = [&GEMINI_SESSION_START_HOOK, &GEMINI_SESSION_END_HOOK];

/// What `install()` sets up. Proactive recall stays a separate opt-in because it reads every
/// prompt.
const BASE_HOOKS: [&HookSpec; 3] = [&SESSION_START_HOOK, &PRE_COMPACT_HOOK, &SESSION_END_HOOK];

/// Events older versions installed into and this one doesn't. Our hooks there are stripped on
/// install and refresh.
const LEGACY_HOOK_EVENTS: [&str; 1] = ["SubagentStop"];

/// Every event that can hold a Callimachus hook, current or legacy (what uninstall sweeps).
const ALL_HOOK_EVENTS: [&str; 7] = [
    "SessionStart",
    "PreCompact",
    "SessionEnd",
    "UserPromptSubmit",
    "PostToolUse",
    "PostToolUseFailure",
    "SubagentStop",
];

/// The command for one of our hooks. It runs the app binary itself with a `cal` subcommand
/// (main.rs dispatches on it), so hooks don't depend on the `cal` link.
fn hook_command(exe: &Path, subcommand: &str) -> String {
    hook_command_for(&exe.to_string_lossy(), subcommand, cfg!(windows))
}

/// [`hook_command`] in the shell `spec`'s agent runs it under (see [`HookSpec::powershell`]).
fn spec_command(exe: &Path, spec: &HookSpec, subcommand: &str) -> String {
    spec_command_for(&exe.to_string_lossy(), spec, subcommand, cfg!(windows))
}

fn spec_command_for(exe: &str, spec: &HookSpec, subcommand: &str, windows: bool) -> String {
    if windows && spec.powershell {
        // Still ends with `"<exe>" <subcommand>`, the shape `our_hook_subcommand` and
        // `hook_target` recognize, so refresh and uninstall find it like any other.
        format!("& \"{exe}\" {subcommand}")
    } else {
        hook_command_for(exe, subcommand, windows)
    }
}

/// On Unix the command is guarded, `[ ! -x "<exe>" ] || "<exe>" hook`: once the app is moved
/// or deleted the hook exits 0 without doing anything, instead of showing a hook error at every
/// session start. When the binary exists, the hook's own exit status still comes through.
/// Windows keeps the plain quoted command, since hooks may run under cmd or PowerShell there.
fn hook_command_for(exe: &str, subcommand: &str, windows: bool) -> String {
    if windows {
        format!("\"{exe}\" {subcommand}")
    } else {
        let quoted = sh_double_quote(exe);
        format!("[ ! -x {quoted} ] || {quoted} {subcommand}")
    }
}

/// Double-quote for POSIX sh, escaping the characters that stay special inside double quotes.
fn sh_double_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if matches!(c, '"' | '\\' | '$' | '`') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// The subcommands our hooks run, with the `--for` forms the Codex and Gemini CLI hooks use.
/// No entry is a suffix of another after a space, so a command matches exactly one.
const HOOK_SUBCOMMANDS: [&str; 10] = [
    "hook",
    "snapshot-hook",
    "recall-now",
    "recall-error",
    "session-end",
    "hook --for codex",
    "snapshot-hook --for codex",
    "session-end --for codex",
    "hook --for gemini",
    "session-end --for gemini",
];

/// The subcommand of a command shaped like ours. Every form we've written (`"<cal>" hook`, the
/// guarded `[ ! -x "<exe>" ] || "<exe>" hook`, and the Windows `"<exe>" hook`) ends with a
/// closing quote plus one of [`HOOK_SUBCOMMANDS`].
fn our_hook_subcommand(cmd: &str) -> Option<&'static str> {
    let c = cmd.trim();
    HOOK_SUBCOMMANDS.into_iter().find(|sub| {
        c.strip_suffix(sub)
            .and_then(|rest| rest.strip_suffix(' '))
            .is_some_and(|rest| rest.ends_with('"'))
    })
}

/// Recognize a hook command we installed: our shape, and it runs this app, a `cal` entry point,
/// or a binary named for Callimachus (another install, a dev build). The shape alone isn't
/// enough: startup refresh rewrites what this matches, and a user's own `"/opt/tool" hook` must
/// never be taken over or deleted.
fn is_our_hook(cmd: &str, exe: &Path) -> bool {
    our_hook_subcommand(cmd).is_some()
        && hook_target(cmd, !cfg!(windows))
            .is_some_and(|t| t == exe || is_cal_name(&t) || mentions_callimachus(&t))
}

/// The binary a hook command runs: its last double-quoted string (`unescape` undoes
/// [`sh_double_quote`]; Windows commands are written unescaped).
fn hook_target(cmd: &str, unescape: bool) -> Option<PathBuf> {
    let mut last: Option<String> = None;
    let mut current: Option<String> = None;
    let mut chars = cmd.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(s) = current.as_mut() {
            if c == '"' {
                last = current.take();
            } else if c == '\\' && unescape && matches!(chars.peek(), Some('"' | '\\' | '$' | '`'))
            {
                s.extend(chars.next());
            } else {
                s.push(c);
            }
        } else if c == '"' {
            current = Some(String::new());
        }
    }
    last.filter(|s| !s.is_empty()).map(PathBuf::from)
}

/// The command of the first Callimachus hook on `event`, if any.
fn our_hook_command(obj: &Map<String, Value>, event: &str, exe: &Path) -> Option<String> {
    obj.get("hooks")?
        .get(event)?
        .as_array()?
        .iter()
        .filter_map(|g| g.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter_map(|h| h.get("command").and_then(Value::as_str))
        .find(|c| is_our_hook(c, exe))
        .map(str::to_string)
}

/// True if a settings document has a Callimachus hook on `event`.
fn event_has_our_hook(v: &Value, event: &str, exe: &Path) -> bool {
    v.as_object()
        .is_some_and(|obj| our_hook_command(obj, event, exe).is_some())
}

/// `hooks.<event>` of a settings object, created if missing.
fn event_array<'a>(obj: &'a mut Map<String, Value>, event: &str) -> Result<&'a mut Vec<Value>> {
    obj.entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| anyhow!("hooks is not an object"))?
        .entry(event)
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| anyhow!("hooks.{event} is not an array"))
}

/// Merge one Callimachus hook into `hooks.<event>`, dropping any prior Callimachus hook there
/// first (idempotent) and keeping the user's own.
fn upsert_hook(
    obj: &mut Map<String, Value>,
    spec: &HookSpec,
    command: &str,
    exe: &Path,
) -> Result<()> {
    let arr = event_array(obj, spec.event)?;
    remove_our_hooks(arr, exe);
    let mut group = Map::new();
    if let Some(matcher) = spec.matcher {
        group.insert("matcher".into(), json!(matcher));
    }
    let mut hook = Map::new();
    if let Some(name) = spec.name {
        hook.insert("name".into(), json!(name));
    }
    hook.insert("type".into(), json!("command"));
    hook.insert("command".into(), json!(command));
    hook.insert("timeout".into(), json!(spec.timeout_value()));
    group.insert("hooks".into(), json!([hook]));
    arr.push(Value::Object(group));
    Ok(())
}

/// Remove Callimachus hooks from an event's group array, plus any group that removal left empty.
/// The user's own hooks and groups are untouched. Returns how many hooks were removed.
fn remove_our_hooks(arr: &mut Vec<Value>, exe: &Path) -> usize {
    let mut removed = 0;
    arr.retain_mut(|group| {
        let Some(hooks) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            return true;
        };
        let before = hooks.len();
        hooks.retain(|h| {
            !h.get("command")
                .and_then(Value::as_str)
                .is_some_and(|c| is_our_hook(c, exe))
        });
        let n = before - hooks.len();
        removed += n;
        !(n > 0 && hooks.is_empty())
    });
    removed
}

/// Strip our hooks from `hooks.<event>`. If that empties the event (or the whole `hooks`
/// object), the now-empty key goes too, so uninstall leaves no `"SubagentStop": []` behind.
fn strip_our_hooks(obj: &mut Map<String, Value>, event: &str, exe: &Path) -> bool {
    let Some(hooks) = obj.get_mut("hooks").and_then(Value::as_object_mut) else {
        return false;
    };
    let Some(arr) = hooks.get_mut(event).and_then(Value::as_array_mut) else {
        return false;
    };
    let removed = remove_our_hooks(arr, exe);
    let event_now_empty = arr.is_empty();
    if removed == 0 {
        return false;
    }
    // `retain` rather than `remove`: it keeps key order even if serde_json's preserve_order
    // feature is ever turned on (where `remove` swaps the last key into the hole).
    if event_now_empty {
        hooks.retain(|k, _| k != event);
    }
    if hooks.is_empty() {
        obj.retain(|k, _| k != "hooks");
    }
    true
}

/// The base hooks in their current shape, plus the opt-in hooks (proactive recall, the
/// error playbooks) re-shaped if the user already opted in. Retired hooks are stripped.
fn apply_base_hooks(obj: &mut Map<String, Value>, exe: &Path) -> Result<()> {
    apply_hooks(obj, &BASE_HOOKS, exe)?;
    if our_hook_command(obj, PROACTIVE_RECALL_HOOK.event, exe).is_some() {
        let command = hook_command(exe, PROACTIVE_RECALL_HOOK.subcommand);
        upsert_hook(obj, &PROACTIVE_RECALL_HOOK, &command, exe)?;
    }
    // Either event opted in means both: an older hand-edit or a half-applied toggle
    // must not leave failing commands (or exit-0 ones) without the playbook.
    if ERROR_RECALL_HOOKS
        .iter()
        .any(|spec| our_hook_command(obj, spec.event, exe).is_some())
    {
        for spec in ERROR_RECALL_HOOKS {
            upsert_hook(obj, spec, &hook_command(exe, spec.subcommand), exe)?;
        }
    }
    Ok(())
}

/// `specs` in their current shape, aimed at `exe`; retired hooks are stripped.
fn apply_hooks(obj: &mut Map<String, Value>, specs: &[&HookSpec], exe: &Path) -> Result<()> {
    for spec in specs {
        upsert_hook(obj, spec, &spec_command(exe, spec, spec.subcommand), exe)?;
    }
    for event in LEGACY_HOOK_EVENTS {
        strip_our_hooks(obj, event, exe);
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Transcript retention (`cleanupPeriodDays`)
// ---------------------------------------------------------------------------------------------

/// How long Claude Code keeps session transcripts when `cleanupPeriodDays` is unset. The
/// settings reference lists the key's default as unset; the effective number is stated at
/// https://code.claude.com/docs/en/data-usage ("store session transcripts locally ... under
/// `~/.claude/projects/` for 30 days by default") and
/// https://code.claude.com/docs/en/claude-directory#cleaned-up-automatically ("The default
/// is 30 days and the minimum is 1; setting `0` fails with a validation error"), checked
/// 2026-11-06.
pub const CLAUDE_DEFAULT_CLEANUP_DAYS: i64 = 30;

/// The settings key Claude Code's transcript sweep reads.
const CLEANUP_KEY: &str = "cleanupPeriodDays";

/// `cleanupPeriodDays` from the user's Claude Code settings (`<claude config dir>/
/// settings.json`, so `CLAUDE_CONFIG_DIR` and the Settings override apply), or None when it
/// is unset. Project and managed settings can override it per repo or per organization;
/// this reads the user-level value, the one Settings can change.
pub fn claude_cleanup_period_days() -> Result<Option<i64>> {
    cleanup_days_in(&settings_path()?)
}

fn cleanup_days_in(path: &Path) -> Result<Option<i64>> {
    let Some(text) = read_config(path)? else {
        return Ok(None);
    };
    let root = parse_json_object(&text, path)?;
    Ok(root.get(CLEANUP_KEY).and_then(Value::as_i64))
}

/// Set `cleanupPeriodDays` in the user's Claude Code settings (None removes the key, back to
/// Claude Code's default), through the same atomic merge as the hooks: every other setting
/// is kept and an unparseable file is left alone.
pub fn set_claude_cleanup_period_days(days: Option<i64>) -> Result<()> {
    set_cleanup_days_in(&settings_path()?, days)
}

fn set_cleanup_days_in(path: &Path, days: Option<i64>) -> Result<()> {
    if let Some(d) = days {
        if d < 1 {
            bail!("Transcript retention must be at least 1 day (Claude Code rejects {d})");
        }
    }
    update_json_file(path, days.is_some(), |obj| {
        match days {
            Some(d) => {
                obj.insert(CLEANUP_KEY.into(), Value::from(d));
            }
            None => {
                obj.remove(CLEANUP_KEY);
            }
        }
        Ok(())
    })?;
    Ok(())
}

/// Merge our hooks into `settings.json`. Keeps all other settings and hooks, refuses to clobber
/// an unreadable or unparseable file, and is idempotent.
fn install_hook(exe: &Path) -> Result<()> {
    update_json_file(&settings_path()?, true, |obj| apply_base_hooks(obj, exe))?;
    Ok(())
}

/// Startup migration for hooks the user already has (see [`refresh_installed`]). Edits each of
/// our hooks in place rather than re-creating it, so whatever the user tuned survives every
/// launch: the command is rewritten (guard form, dead or `cal`-link target moved to `exe`), and
/// `timeout` / `matcher` are added only where an older version left them out. Missing hooks are
/// not added back, and the user's own hooks are never touched. The full reshape is Install's job.
fn refresh_hooks_in(obj: &mut Map<String, Value>, exe: &Path) -> Result<()> {
    refresh_specs_in(
        obj,
        &[
            &SESSION_START_HOOK,
            &PRE_COMPACT_HOOK,
            &SESSION_END_HOOK,
            &PROACTIVE_RECALL_HOOK,
            &ERROR_RECALL_HOOK,
            &ERROR_RECALL_FAILURE_HOOK,
        ],
        exe,
    );
    for event in LEGACY_HOOK_EVENTS {
        strip_our_hooks(obj, event, exe);
    }
    Ok(())
}

/// [`refresh_group`] over every group of each spec's event (shared by Claude Code's
/// `settings.json`, Codex's `hooks.json` and Gemini CLI's `settings.json`, which use the
/// same `hooks.<event>[].hooks[]` shape).
fn refresh_specs_in(obj: &mut Map<String, Value>, specs: &[&HookSpec], exe: &Path) {
    let Some(events) = obj.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    for spec in specs {
        let Some(groups) = events.get_mut(spec.event).and_then(Value::as_array_mut) else {
            continue;
        };
        for group in groups.iter_mut().filter_map(Value::as_object_mut) {
            refresh_group(group, spec, exe);
        }
    }
}

/// `<index dir>/session-end-hook-added`: SessionEnd was offered to this install once.
const SESSION_END_MARKER: &str = "session-end-hook-added";

/// `<index dir>/session-end-hook-added`, beside the index like the uninstaller's record.
fn session_end_marker_path() -> PathBuf {
    crate::db::default_index_path()
        .parent()
        .map(|d| d.join(SESSION_END_MARKER))
        .unwrap_or_else(|| PathBuf::from(SESSION_END_MARKER))
}

/// The SessionEnd hook arrived after the other base hooks, and refresh never adds a missing
/// hook (so a hook the user deleted stays deleted). Without this, an existing install would
/// only get it from a fresh Install click. So once per install, add it next to our
/// SessionStart hook, then leave a marker in the data dir: a user who removes it afterwards
/// keeps it removed.
fn add_session_end_hook_once(exe: &Path) -> Result<bool> {
    let marker = session_end_marker_path();
    if marker.exists() {
        return Ok(false);
    }
    let path = settings_path()?;
    let mut outcome = SessionEndAdd::NoInstall;
    let changed = update_json_file(&path, false, |obj| {
        outcome = add_session_end_hook_in(obj, exe);
        Ok(())
    })?;
    // Only once the hook is there (added now, or already present): before an Install there is
    // nothing to join, and Install itself adds the hook; while SessionStart runs another
    // binary, the newest one adds it on its own launch.
    if matches!(outcome, SessionEndAdd::Added | SessionEndAdd::Present) {
        if let Some(dir) = marker.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&marker, b"").with_context(|| format!("writing {}", marker.display()))?;
    }
    Ok(changed)
}

/// What [`add_session_end_hook_in`] did.
#[derive(Debug, PartialEq, Eq)]
enum SessionEndAdd {
    Added,
    /// Our SessionEnd hook is already there.
    Present,
    /// No SessionStart hook of ours to join.
    NoInstall,
    /// Our SessionStart hook runs another live Callimachus binary. That binary may predate
    /// `session-end` (the installed release while a dev build runs, or any build after a
    /// downgrade), and an older main.rs treats an unknown subcommand as a GUI launch, which
    /// would open a window at every session end. So leave it to that binary's own launch.
    OtherBinary,
}

/// [`add_session_end_hook_once`] on a parsed settings object: adds our SessionEnd hook when
/// our SessionStart hook is present, runs `exe`, and SessionEnd holds none of ours.
fn add_session_end_hook_in(obj: &mut Map<String, Value>, exe: &Path) -> SessionEndAdd {
    let Some(start) = our_hook_command(obj, SESSION_START_HOOK.event, exe) else {
        return SessionEndAdd::NoInstall;
    };
    if our_hook_command(obj, SESSION_END_HOOK.event, exe).is_some() {
        return SessionEndAdd::Present;
    }
    if refresh_target(&start, exe) != exe {
        return SessionEndAdd::OtherBinary;
    }
    let command = hook_command(exe, SESSION_END_HOOK.subcommand);
    match upsert_hook(obj, &SESSION_END_HOOK, &command, exe) {
        Ok(()) => SessionEndAdd::Added,
        // A malformed hooks value: leave the marker unwritten so a later launch retries.
        Err(_) => SessionEndAdd::NoInstall,
    }
}

/// [`refresh_hooks_in`] for one hook group.
fn refresh_group(group: &mut Map<String, Value>, spec: &HookSpec, exe: &Path) {
    let Some(hooks) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
        return;
    };
    let mut ours = 0;
    for hook in hooks.iter_mut().filter_map(Value::as_object_mut) {
        let Some(cmd) = hook.get("command").and_then(Value::as_str) else {
            continue;
        };
        if !is_our_hook(cmd, exe) {
            continue;
        }
        ours += 1;
        // Keep the subcommand that's there; only the binary and the quoting are ours to fix.
        let sub = our_hook_subcommand(cmd).unwrap_or(spec.subcommand);
        let command = spec_command(&refresh_target(cmd, exe), spec, sub);
        hook.insert("command".into(), json!(command));
        hook.entry("timeout").or_insert(json!(spec.timeout_value()));
    }
    // A matcher scopes the whole group, so only add one to a group that holds nothing but ours.
    let all_ours = ours > 0 && ours == hooks.len();
    if let Some(matcher) = spec.matcher.filter(|_| all_ours) {
        group.entry("matcher").or_insert(json!(matcher));
    }
}

/// Which binary a refreshed hook should run. A hook pointing at another live Callimachus binary
/// keeps it, so a release build and a dev build don't fight over the hooks at every launch. A
/// dead target, or the old `cal`-link form, moves to `exe`.
fn refresh_target(existing_command: &str, exe: &Path) -> PathBuf {
    match hook_target(existing_command, !cfg!(windows)) {
        Some(t) if t != exe && t.is_file() && !is_cal_name(&t) => t,
        _ => exe.to_path_buf(),
    }
}

fn is_cal_name(p: &Path) -> bool {
    p.file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("cal"))
}

// ---------------------------------------------------------------------------------------------
// Config file I/O
// ---------------------------------------------------------------------------------------------

/// Read a config file. A missing file is `None`; any other failure (permissions, a file that
/// isn't UTF-8, such as one written as UTF-16 by Windows PowerShell) is an error, never "empty",
/// so a file we can't read is never replaced with a fresh one.
fn read_config(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}; not modifying it", path.display())),
    }
}

/// How many times a read-modify-write starts over when the file changes underneath it.
const WRITE_ATTEMPTS: usize = 3;

/// The shared read-modify-write loop. `render` maps the current text (`None`: no file) to the
/// new text, or to `None` for "nothing to change". With `create == false` a missing file is left
/// alone. Just before replacing the file it reads it again: if another process (a running
/// Claude Code, say) wrote it in the meantime, the merge starts over from the new contents
/// rather than overwriting that write. Returns whether the file was written.
fn update_text_file(
    path: &Path,
    create: bool,
    mut render: impl FnMut(Option<&str>) -> Result<Option<String>>,
) -> Result<bool> {
    for _ in 0..WRITE_ATTEMPTS {
        let current = read_config(path)?;
        if current.is_none() && !create {
            return Ok(false);
        }
        let Some(next) = render(current.as_deref())? else {
            return Ok(false);
        };
        if read_config(path)? != current {
            continue;
        }
        write_atomic(path, next.as_bytes())?;
        return Ok(true);
    }
    bail!(
        "{} kept changing while Callimachus was updating it; try again",
        path.display()
    )
}

fn parse_json_object(text: &str, path: &Path) -> Result<Value> {
    if text.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let value: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        // Gemini CLI strips comments from its settings.json on load, so a file can be fine
        // for its agent and still not JSON. Rewriting it would drop the user's comments, so
        // say that is the reason instead of a bare parse error.
        Err(_) if serde_json::from_str::<Value>(&strip_json_comments(text)).is_ok() => bail!(
            "{} has comments, which Callimachus can't keep when it edits the file; remove \
             them, or add the Callimachus entries by hand. Not modifying it",
            path.display()
        ),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("{} is not valid JSON; not modifying it", path.display()))
        }
    };
    if !value.is_object() {
        bail!("{} is not a JSON object; not modifying it", path.display());
    }
    Ok(value)
}

/// `text` with `//` line comments and `/* */` block comments removed (outside strings), the
/// JSON-with-comments Gemini CLI accepts in its settings.json. Newlines inside a block comment
/// are kept so a later parse error still points at the right line.
pub(crate) fn strip_json_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match c {
                '\\' => out.extend(chars.next()),
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_string = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut prev = '\0';
                for c in chars.by_ref() {
                    if prev == '*' && c == '/' {
                        break;
                    }
                    if c == '\n' {
                        out.push('\n');
                    }
                    prev = c;
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// Read-modify-write a JSON config whose root is an object. Skips the write when `edit` leaves
/// the parsed document unchanged.
///
/// Note: serde_json is built without `preserve_order`, so a write re-sorts the file's keys.
fn update_json_file(
    path: &Path,
    create: bool,
    mut edit: impl FnMut(&mut Map<String, Value>) -> Result<()>,
) -> Result<bool> {
    update_text_file(path, create, |text| {
        let mut root = parse_json_object(text.unwrap_or(""), path)?;
        let before = root.clone();
        let obj = root
            .as_object_mut()
            .ok_or_else(|| anyhow!("{} is not a JSON object", path.display()))?;
        edit(obj)?;
        if text.is_some() && root == before {
            return Ok(None);
        }
        Ok(Some(serde_json::to_string_pretty(&root)?))
    })
}

/// Read-modify-write a TOML config with format-preserving edits (keeps comments and ordering).
fn update_toml_file(
    path: &Path,
    create: bool,
    mut edit: impl FnMut(&mut toml_edit::DocumentMut) -> Result<()>,
) -> Result<bool> {
    update_text_file(path, create, |text| {
        let mut doc = match text {
            Some(t) if !t.trim().is_empty() => {
                t.parse::<toml_edit::DocumentMut>().with_context(|| {
                    format!("{} is not valid TOML; not modifying it", path.display())
                })?
            }
            _ => toml_edit::DocumentMut::new(),
        };
        edit(&mut doc)?;
        let out = doc.to_string();
        if text == Some(out.as_str()) {
            return Ok(None);
        }
        Ok(Some(out))
    })
}

/// Distinguishes temp files from concurrent writes within this process.
static TMP_SEQ: AtomicUsize = AtomicUsize::new(0);

/// Replace `path` with `contents` atomically: write a temp file in the same directory, fsync it,
/// carry over the original's permissions (e.g. 0600 on `~/.claude.json`; new files get 0600 on
/// Unix), then rename it over the original. Readers see the old file or the new one, never a
/// truncated one.
///
/// A symlinked config (one kept in a dotfiles repo) is written through: the link's target is
/// replaced and the link itself stays.
fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;

    let target = match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
        }
        _ => path.to_path_buf(),
    };
    let dir = target
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let name = target
        .file_name()
        .with_context(|| format!("{} has no file name", target.display()))?
        .to_string_lossy()
        .into_owned();
    let tmp = dir.join(format!(
        "{name}.callimachus-{}-{}.tmp",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let original_permissions = std::fs::metadata(&target).ok().map(|m| m.permissions());

    let result = (|| -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600); // owner-only until the original's mode is applied
        }
        let mut file = options.open(&tmp)?;
        file.write_all(contents)?;
        if let Some(permissions) = original_permissions {
            file.set_permissions(permissions)?;
        }
        file.sync_all()?;
        drop(file);
        rename_replacing(&tmp, &target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.with_context(|| format!("writing {}", target.display()))
}

/// `rename(from, to)`, replacing `to`. On Windows the replace fails with "access denied" while
/// another process (an antivirus scan, a search indexer, an agent mid-read) briefly holds the
/// target open, so retry a few times before giving up. The original stays intact either way.
fn rename_replacing(from: &Path, to: &Path) -> std::io::Result<()> {
    let attempts = if cfg!(windows) { 5 } else { 1 };
    let mut result = std::fs::rename(from, to);
    for _ in 1..attempts {
        match &result {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                std::thread::sleep(std::time::Duration::from_millis(50));
                result = std::fs::rename(from, to);
            }
            _ => break,
        }
    }
    result
}

// ---------------------------------------------------------------------------------------------
// MCP registration
// ---------------------------------------------------------------------------------------------

/// Merge a `callimachus` entry into a JSON config's `mcpServers`, pointing the client at this
/// app run with `--mcp`. Shared by Claude Code, Cursor and Gemini (same schema, different
/// files). Keeps all other config and refuses to clobber an unparseable file.
fn register_mcp_json(cfg: &Path, exe: &Path) -> Result<()> {
    update_json_file(cfg, true, |obj| {
        let servers = obj
            .entry("mcpServers")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .ok_or_else(|| anyhow!("mcpServers in {} is not an object", cfg.display()))?;
        servers.insert(
            MCP_NAME.to_string(),
            json!({
                "type": "stdio",
                "command": exe.to_string_lossy(),
                "args": ["--mcp"],
            }),
        );
        Ok(())
    })?;
    Ok(())
}

/// Remove our `mcpServers.callimachus` entry from a JSON config (leaves the rest intact; no
/// write at all when the entry isn't there).
fn remove_mcp_json(cfg: &Path) -> Result<()> {
    update_json_file(cfg, false, |obj| {
        if let Some(servers) = obj.get_mut("mcpServers").and_then(Value::as_object_mut) {
            servers.retain(|k, _| k != MCP_NAME);
        }
        Ok(())
    })?;
    Ok(())
}

/// Remove `mcpServers.callimachus` only when it is an app registration (any path, run with
/// exactly `--mcp`, the shape [`register_mcp_json`] writes). Returns whether the file changed.
fn remove_app_mcp_json(cfg: &Path) -> Result<bool> {
    update_json_file(cfg, false, |obj| {
        let ours = is_app_mcp_server(obj.get("mcpServers"));
        if let Some(servers) = obj.get_mut("mcpServers").and_then(Value::as_object_mut) {
            if ours {
                servers.retain(|k, _| k != MCP_NAME);
            }
        }
        Ok(())
    })
}

/// An `mcpServers` map whose `callimachus` is an app registration (run with exactly `--mcp`).
fn is_app_mcp_server(servers: Option<&Value>) -> bool {
    servers
        .and_then(|m| m.get(MCP_NAME))
        .and_then(|s| s.get("args"))
        .is_some_and(|a| *a == json!(["--mcp"]))
}

/// True if a JSON config registers our MCP server pointing at this exe.
fn mcp_registered_json(cfg: &Path, exe: &Path) -> bool {
    std::fs::read_to_string(cfg)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| {
            v.get("mcpServers")
                .and_then(|m| m.get(MCP_NAME))
                .and_then(|s| s.get("command"))
                .and_then(Value::as_str)
                .map(|cmd| cmd == exe.to_string_lossy())
        })
        .unwrap_or(false)
}

/// A `callimachus` MCP entry whose absolute command no longer exists and that `refresh_installed`
/// re-points at the app.
#[derive(Debug, PartialEq)]
enum DeadRegistration {
    /// One this app registered: an app path run with exactly `--mcp`. Only the command changes.
    App,
    /// A formerly bundled `callimachus-mcp` binary run with no args, typically added by hand
    /// (the deb/rpm `/usr/bin/callimachus-mcp`, or the copy in the macOS .app's MacOS dir).
    /// Those packages no longer ship it, so the entry gains `--mcp` along with the app path.
    StandaloneServer,
}

/// Classify an MCP entry under our name. Anything that is not dead and recognizably ours, such
/// as `npx -y callimachus-mcp` or a path that still exists, is the user's choice and is left alone.
fn dead_registration<'a>(
    command: Option<&str>,
    mut args: impl Iterator<Item = Option<&'a str>>,
) -> Option<DeadRegistration> {
    let p = Path::new(command?);
    if !p.is_absolute() || p.exists() {
        return None;
    }
    match (args.next(), args.next()) {
        (Some(Some("--mcp")), None) => Some(DeadRegistration::App),
        (None, _) if p.file_stem() == Some("callimachus-mcp".as_ref()) => {
            Some(DeadRegistration::StandaloneServer)
        }
        _ => None,
    }
}

/// Re-point a dead `mcpServers.callimachus` command at `exe`. Never creates the entry.
fn refresh_mcp_json(cfg: &Path, exe: &Path) -> Result<bool> {
    update_json_file(cfg, false, |obj| {
        let Some(entry) = obj
            .get_mut("mcpServers")
            .and_then(|m| m.get_mut(MCP_NAME))
            .and_then(Value::as_object_mut)
        else {
            return Ok(());
        };
        let args: Vec<Option<&str>> = entry
            .get("args")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(Value::as_str).collect())
            .unwrap_or_default();
        let dead = dead_registration(
            entry.get("command").and_then(Value::as_str),
            args.into_iter(),
        );
        if let Some(kind) = dead {
            entry.insert("command".into(), json!(exe.to_string_lossy()));
            if kind == DeadRegistration::StandaloneServer {
                entry.insert("args".into(), json!(["--mcp"]));
            }
        }
        Ok(())
    })
}

/// Merge `[mcp_servers.callimachus]` into Codex's `config.toml` with format-preserving TOML
/// edits (keeps the user's comments and ordering). Refuses unparseable TOML.
fn register_mcp_toml(cfg: &Path, exe: &Path) -> Result<()> {
    update_toml_file(cfg, true, |doc| {
        // Table-like: an inline `mcp_servers = { ... }` is valid TOML too (and inserting a table
        // into it becomes an inline table).
        let servers = doc
            .entry("mcp_servers")
            .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
            .as_table_like_mut()
            .ok_or_else(|| anyhow!("mcp_servers in {} is not a table", cfg.display()))?;
        let mut entry = toml_edit::Table::new();
        entry["command"] = toml_edit::value(exe.to_string_lossy().to_string());
        let mut args = toml_edit::Array::new();
        args.push("--mcp");
        entry["args"] = toml_edit::value(args);
        servers.insert(MCP_NAME, toml_edit::Item::Table(entry));
        Ok(())
    })?;
    Ok(())
}

/// Remove our `[mcp_servers.callimachus]` table from Codex's config (leaves the rest intact).
fn remove_mcp_toml(cfg: &Path) -> Result<()> {
    update_toml_file(cfg, false, |doc| {
        // Table-like, as in register_mcp_toml: an inline table must not be silently skipped.
        if let Some(servers) = doc
            .get_mut("mcp_servers")
            .and_then(|i| i.as_table_like_mut())
        {
            servers.remove(MCP_NAME);
        }
        Ok(())
    })?;
    Ok(())
}

/// True if Codex's config registers our MCP server pointing at this exe.
fn mcp_registered_toml(cfg: &Path, exe: &Path) -> bool {
    std::fs::read_to_string(cfg)
        .ok()
        .and_then(|s| s.parse::<toml_edit::DocumentMut>().ok())
        .and_then(|doc| {
            doc.get("mcp_servers")
                .and_then(|i| i.get(MCP_NAME))
                .and_then(|s| s.get("command"))
                .and_then(|c| c.as_str())
                .map(|cmd| cmd == exe.to_string_lossy())
        })
        .unwrap_or(false)
}

/// Re-point a dead `[mcp_servers.callimachus]` command at `exe`. Never creates the entry.
fn refresh_mcp_toml(cfg: &Path, exe: &Path) -> Result<bool> {
    update_toml_file(cfg, false, |doc| {
        let Some(entry) = doc
            .get_mut("mcp_servers")
            .and_then(|i| i.get_mut(MCP_NAME))
            .and_then(|i| i.as_table_like_mut())
        else {
            return Ok(());
        };
        let args: Vec<Option<&str>> = entry
            .get("args")
            .and_then(|a| a.as_array())
            .map(|a| a.iter().map(|v| v.as_str()).collect())
            .unwrap_or_default();
        let dead = dead_registration(
            entry.get("command").and_then(|c| c.as_str()),
            args.into_iter(),
        );
        if let Some(kind) = dead {
            entry.insert(
                "command",
                toml_edit::value(exe.to_string_lossy().to_string()),
            );
            if kind == DeadRegistration::StandaloneServer {
                let mut args = toml_edit::Array::new();
                args.push("--mcp");
                entry.insert("args", toml_edit::value(args));
            }
        }
        Ok(())
    })
}

// ---------------------------------------------------------------------------------------------
// The `cal` entry point
// ---------------------------------------------------------------------------------------------

/// What occupies a `cal` link location.
#[derive(Debug, PartialEq)]
enum CalSlot {
    Missing,
    /// A Callimachus link (to this app or another copy of it). `live`: its target exists.
    Ours {
        target: PathBuf,
        live: bool,
    },
    /// Something else, described for the user.
    Foreign(String),
}

/// Look at `link` without following it. Ours means a symlink to `exe` or to a path naming
/// Callimachus (another install or a dev build); a regular file or any other link is someone
/// else's and must not be replaced.
fn inspect_cal_slot(link: &Path, exe: &Path) -> CalSlot {
    match std::fs::symlink_metadata(link) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => CalSlot::Missing,
        Err(e) => CalSlot::Foreign(format!("it couldn't be inspected: {e}")),
        Ok(meta) if meta.file_type().is_symlink() => match std::fs::read_link(link) {
            Ok(target)
                if target == exe
                    || (mentions_callimachus(&target)
                        && !target
                            .components()
                            .any(|c| c.as_os_str() == "callimachus-mcp")) =>
            {
                let live = link.exists();
                CalSlot::Ours { target, live }
            }
            Ok(target) => CalSlot::Foreign(format!("it links to {}", target.display())),
            Err(e) => CalSlot::Foreign(format!("its link couldn't be read: {e}")),
        },
        Ok(_) => CalSlot::Foreign("it's a regular file".into()),
    }
}

fn mentions_callimachus(p: &Path) -> bool {
    p.to_string_lossy()
        .to_ascii_lowercase()
        .contains("callimachus")
}

fn foreign_cal_message(link: &Path, what: &str) -> String {
    format!(
        "{} belongs to another program ({what}), so Callimachus left it alone. Hooks, MCP and the \
         /recall skill don't need it; to get the `cal` command, remove or rename that file and \
         reinstall.",
        link.display()
    )
}

/// The `cal` entry point as it stands on disk.
struct CalState {
    path: PathBuf,
    /// Exists and runs this app (or another Callimachus copy).
    installed: bool,
    problem: Option<String>,
}

fn cal_state(exe: &Path) -> CalState {
    let path = match cal_path(exe) {
        Ok(p) => p,
        Err(e) => {
            return CalState {
                path: PathBuf::new(),
                installed: false,
                problem: Some(format!("{e:#}")),
            }
        }
    };
    if cfg!(windows) {
        let installed = path.is_file();
        // The install-time error isn't kept between status calls, so explain the common
        // permanent cause from the location alone.
        let problem = (!installed && is_under_program_files(&path)).then(|| {
            format!(
                "Callimachus is installed for all users under Program Files, where the app can't \
                 create {}. The MCP server, hooks and /recall skill still work; only the `cal` \
                 terminal command is unavailable.",
                path.display()
            )
        });
        return CalState {
            path,
            installed,
            problem,
        };
    }
    match inspect_cal_slot(&path, exe) {
        CalSlot::Missing => CalState {
            path,
            installed: false,
            problem: None,
        },
        // A dangling link counts as not installed; refresh or reinstall re-points it.
        CalSlot::Ours { live, .. } => CalState {
            path,
            installed: live,
            problem: None,
        },
        CalSlot::Foreign(what) => {
            let problem = Some(foreign_cal_message(&path, &what));
            CalState {
                path,
                installed: false,
                problem,
            }
        }
    }
}

/// A path inside `%ProgramFiles%` (a per-machine MSI install), compared case-insensitively.
fn is_under_program_files(p: &Path) -> bool {
    let path = p.to_string_lossy().to_lowercase();
    ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"]
        .iter()
        .filter_map(std::env::var_os)
        .map(|dir| dir.to_string_lossy().trim_end_matches('\\').to_lowercase())
        .any(|dir| !dir.is_empty() && path.starts_with(&format!("{dir}\\")))
}

/// Symlink `~/.local/bin/cal` to the app (which runs in `cal` mode when invoked by that name).
#[cfg(unix)]
fn install_cal(exe: &Path) -> Result<()> {
    link_cal_at(&cal_path(exe)?, exe)
}

/// Point a `cal` link at `exe`, replacing only a missing link or one that's already ours.
#[cfg(unix)]
fn link_cal_at(link: &Path, exe: &Path) -> Result<()> {
    match inspect_cal_slot(link, exe) {
        CalSlot::Foreign(what) => bail!(foreign_cal_message(link, &what)),
        CalSlot::Ours { target, live: true } if target == exe => return Ok(()),
        CalSlot::Ours { .. } => {
            std::fs::remove_file(link).with_context(|| format!("replacing {}", link.display()))?
        }
        CalSlot::Missing => {}
    }
    if let Some(dir) = link.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::os::unix::fs::symlink(exe, link)
        .with_context(|| format!("linking {} -> {}", link.display(), exe.display()))
}

/// Place a `cal.exe` next to the app so the CLI and the VS Code extension (which probes the
/// install dir, see `calCandidates()` in apps/vscode/src/cal.ts) can find it. A hard link keeps
/// one on-disk copy (same dir, same volume), falling back to a plain copy. A per-machine install
/// under Program Files isn't writable by the app, which is reported, not fatal.
#[cfg(windows)]
fn install_cal(exe: &Path) -> Result<()> {
    if is_cal_name(exe) {
        return Ok(()); // don't link onto ourselves if somehow invoked as cal.exe
    }
    let link = cal_path(exe)?;
    if link.is_file() && !windows_cal_is_stale(exe, &link) {
        return Ok(());
    }
    let _ = std::fs::remove_file(&link);
    std::fs::hard_link(exe, &link)
        .or_else(|_| std::fs::copy(exe, &link).map(|_| ()))
        .map_err(|e| {
            anyhow!(
                "couldn't create {} ({e}). That's expected for a per-machine install under \
                 Program Files. The MCP server, hooks and /recall skill still work; only the \
                 `cal` terminal command is unavailable.",
                link.display()
            )
        })
}

/// A `cal.exe` copy left behind by an update (a hard link shares the app's size and mtime; a
/// copy keeps them too, since Windows copies preserve the write time).
#[cfg(windows)]
fn windows_cal_is_stale(exe: &Path, cal: &Path) -> bool {
    let (Ok(a), Ok(b)) = (std::fs::metadata(exe), std::fs::metadata(cal)) else {
        return true;
    };
    a.len() != b.len() || a.modified().ok() != b.modified().ok()
}

#[cfg(not(any(unix, windows)))]
fn install_cal(_exe: &Path) -> Result<()> {
    Ok(())
}

/// Re-point a dangling `~/.local/bin/cal` link at `exe`. Never creates one.
#[cfg(unix)]
fn refresh_cal(exe: &Path) -> Result<bool> {
    let link = cal_path(exe)?;
    if let CalSlot::Ours { live: false, .. } = inspect_cal_slot(&link, exe) {
        link_cal_at(&link, exe)?;
        return Ok(true);
    }
    Ok(false)
}

/// Refresh a `cal.exe` that an update left stale. Never creates one.
#[cfg(windows)]
fn refresh_cal(exe: &Path) -> Result<bool> {
    let cal = cal_path(exe)?;
    if cal.is_file() && windows_cal_is_stale(exe, &cal) {
        install_cal(exe)?;
        return Ok(true);
    }
    Ok(false)
}

#[cfg(not(any(unix, windows)))]
fn refresh_cal(_exe: &Path) -> Result<bool> {
    Ok(false)
}

/// Remove our `cal` link; a `cal` that belongs to something else stays.
#[cfg(unix)]
fn remove_cal() -> Result<()> {
    let exe = current_stable_exe();
    let link = cal_path(&exe)?;
    if let CalSlot::Ours { .. } = inspect_cal_slot(&link, &exe) {
        remove_if_exists(&link)?;
    }
    Ok(())
}

/// Remove the `cal.exe` next to the app, so the uninstaller can also remove the install dir.
#[cfg(windows)]
fn remove_cal() -> Result<()> {
    let exe = current_stable_exe();
    if is_cal_name(&exe) {
        return Ok(()); // can't delete the running binary
    }
    remove_if_exists(&cal_path(&exe)?)
}

#[cfg(not(any(unix, windows)))]
fn remove_cal() -> Result<()> {
    Ok(())
}

/// The running app's stable path, for calls (uninstall) that don't receive one.
fn current_stable_exe() -> PathBuf {
    std::env::current_exe()
        .map(|c| resolve_exe(&c).0)
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// PATH hint
// ---------------------------------------------------------------------------------------------

/// Prefix that marks the PATH line in the shell probe's output.
const SHELL_PATH_MARKER: &str = "__CALLIMACHUS_PATH__=";

/// The user's interactive shell PATH, probed once per process.
static SHELL_PATH: OnceLock<Option<Vec<PathBuf>>> = OnceLock::new();

/// The shell PATH dirs. `wait == false` never blocks: it starts the probe in the background and
/// returns `None` until it has finished.
fn shell_path_dirs(wait: bool) -> Option<&'static [PathBuf]> {
    if wait {
        return SHELL_PATH.get_or_init(query_shell_path).as_deref();
    }
    match SHELL_PATH.get() {
        Some(dirs) => dirs.as_deref(),
        None => {
            warm_shell_path();
            None
        }
    }
}

fn warm_shell_path() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        std::thread::spawn(|| {
            SHELL_PATH.get_or_init(query_shell_path);
        });
    });
}

/// Ask the user's shell for its PATH. A GUI app's own PATH is launchd's minimal one, which says
/// nothing about what agents (and the user's terminal) will resolve. Interactive + login, like a
/// terminal, because many people only extend PATH in ~/.zshrc or ~/.bashrc. stdin is closed so
/// an rc-file prompt can't wait for input, a marker skips any banner the rc files print, and a
/// timeout bounds a slow or hung shell.
fn query_shell_path() -> Option<Vec<PathBuf>> {
    use std::io::Read;
    use std::process::{Command, Stdio};

    let shell = std::env::var_os("SHELL").filter(|s| !s.is_empty())?;
    let mut child = Command::new(shell)
        .arg("-ilc")
        .arg(format!("printf '%s' '{SHELL_PATH_MARKER}'; printenv PATH"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = String::new();
        let _ = stdout.read_to_string(&mut out);
        let _ = tx.send(out);
    });
    let out = match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(out) => out,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };
    let _ = child.wait();
    parse_shell_path(&out)
}

fn parse_shell_path(output: &str) -> Option<Vec<PathBuf>> {
    let line = output.lines().find_map(|l| {
        l.split_once(SHELL_PATH_MARKER)
            .map(|(_, rest)| rest.trim().to_string())
    })?;
    let dirs: Vec<PathBuf> = std::env::split_paths(&line)
        .filter(|d| d.is_absolute())
        .collect();
    (!dirs.is_empty()).then_some(dirs)
}

/// Index of the first PATH dir that holds a `cal`.
fn first_cal_dir(dirs: &[PathBuf]) -> Option<usize> {
    dirs.iter().position(|d| d.join("cal").is_file())
}

/// `candidate` runs this app: it resolves to the same file, or it's one of our live links.
fn is_our_cal(candidate: &Path, exe: &Path) -> bool {
    let same_file = match (std::fs::canonicalize(candidate), std::fs::canonicalize(exe)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    same_file
        || matches!(
            inspect_cal_slot(candidate, exe),
            CalSlot::Ours { live: true, .. }
        )
}

/// `~/...` for display.
fn tildify(p: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| p.strip_prefix(h).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => p.display().to_string(),
    }
}

/// `$HOME/...` for a shell snippet the user pastes.
fn shell_home_form(p: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| p.strip_prefix(h).ok()) {
        Some(rest) => format!("$HOME/{}", rest.display()),
        None => p.display().to_string(),
    }
}

/// Advice when a bare `cal` in the user's shell (`dirs` is its PATH) won't reach our `cal` at
/// `cal`. `None` when it resolves to us.
fn path_hint_for(dirs: &[PathBuf], cal: &Path, exe: &Path, home: Option<&Path>) -> Option<String> {
    let link_dir = cal.parent()?;
    let first = first_cal_dir(dirs);
    if let Some(i) = first {
        if is_our_cal(&dirs[i].join("cal"), exe) {
            return None;
        }
    }
    let shown = tildify(link_dir, home);
    let fix = format!("export PATH=\"{}:$PATH\"", shell_home_form(link_dir, home));
    let other = first.map(|i| {
        let other = dirs[i].join("cal");
        let system = other.starts_with("/usr/bin") || other.starts_with("/bin");
        let label = if system { " (the system calendar)" } else { "" };
        (dirs[i].clone(), format!("{}{label}", other.display()))
    });
    let on_path = dirs.iter().any(|d| d == link_dir);
    let advice = format!(
        "Agents still reach it by full path through the /recall skill. To use `cal` in a \
         terminal, add this to your shell profile (~/.zshrc, ~/.bashrc, ...): {fix}"
    );
    match (on_path, other) {
        (false, Some((_, other))) => Some(format!(
            "{shown} isn't on your shell's PATH, so `cal` runs {other} instead of Callimachus. \
             {advice}"
        )),
        (false, None) => Some(format!(
            "{shown} isn't on your shell's PATH, so `cal` isn't found by name. {advice}"
        )),
        (true, Some((dir, other))) => Some(format!(
            "`cal` runs {other} instead of Callimachus, because {} comes before {shown} on your \
             shell's PATH. {advice}",
            dir.display()
        )),
        // On PATH yet nothing named `cal` resolves: the link itself is broken, which
        // `cal_installed` already reports.
        (true, None) => None,
    }
}

/// Windows: a GUI app inherits the user's real PATH from Explorer, so the process PATH is a
/// fair answer there.
fn windows_path_hint(cal: &Path) -> Option<String> {
    let dir = cal.parent()?;
    let wanted = dir.to_string_lossy().trim_end_matches('\\').to_lowercase();
    let on_path = std::env::var_os("PATH").is_some_and(|p| {
        std::env::split_paths(&p)
            .any(|d| d.to_string_lossy().trim_end_matches('\\').to_lowercase() == wanted)
    });
    (!on_path).then(|| {
        format!(
            "{} isn't on your PATH, so a terminal won't find `cal` by name. Agents use the full \
             path from the /recall skill; to type `cal` yourself, add that folder to your user \
             PATH or run \"{}\".",
            dir.display(),
            cal.display()
        )
    })
}

fn path_hint(cal: &CalState, exe: &Path, wait: bool) -> Option<String> {
    if !cal.installed {
        return None;
    }
    if cfg!(windows) {
        return windows_path_hint(&cal.path);
    }
    let dirs = shell_path_dirs(wait)?;
    let home = home().ok();
    path_hint_for(dirs, &cal.path, exe, home.as_deref())
}

// ---------------------------------------------------------------------------------------------
// Other agents (Codex / Cursor / Gemini)
// ---------------------------------------------------------------------------------------------

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

struct AgentTarget {
    id: &'static str,
    label: &'static str,
    /// Presence marker: the agent's config dir.
    dir: PathBuf,
    config: PathBuf,
    fmt: Fmt,
}

/// The agents we can register the MCP server for, beyond Claude Code. Cursor and Gemini use the
/// JSON `mcpServers` schema; Codex uses a TOML `[mcp_servers]` table under `$CODEX_HOME`.
fn agent_targets() -> Result<Vec<AgentTarget>> {
    let home = home()?;
    let codex = codex_home()?;
    let cursor = home.join(".cursor");
    let gemini = home.join(".gemini");
    Ok(vec![
        AgentTarget {
            id: "codex",
            label: "Codex",
            config: codex.join("config.toml"),
            dir: codex,
            fmt: Fmt::Toml,
        },
        AgentTarget {
            id: "cursor",
            label: "Cursor",
            config: cursor.join("mcp.json"),
            dir: cursor,
            fmt: Fmt::Json,
        },
        AgentTarget {
            id: "gemini",
            label: "Gemini CLI",
            config: gemini.join("settings.json"),
            dir: gemini,
            fmt: Fmt::Json,
        },
    ])
}

/// Detected-agent MCP status (present = config dir exists; registered = our server is wired).
pub fn agent_status(app_exe: &Path) -> Vec<AgentIntegration> {
    let (exe, _) = resolve_exe(app_exe);
    let Ok(agents) = agent_targets() else {
        return Vec::new();
    };
    agents
        .into_iter()
        .map(|a| {
            let registered = match a.fmt {
                Fmt::Json => mcp_registered_json(&a.config, &exe),
                Fmt::Toml => mcp_registered_toml(&a.config, &exe),
            };
            AgentIntegration {
                id: a.id.to_string(),
                label: a.label.to_string(),
                present: a.dir.is_dir(),
                registered,
                config_path: a.config.display().to_string(),
            }
        })
        .collect()
}

/// Register the MCP server for every DETECTED agent (config dir exists). Never creates a config
/// for an agent the user doesn't use. Refuses to run from a temporary location. Idempotent.
/// Tries every agent, so one config it can't edit doesn't keep the others from being registered,
/// then reports any failures.
pub fn install_agents(app_exe: &Path) -> Result<Vec<AgentIntegration>> {
    let exe = stable_app_exe(app_exe)?;
    let mut errors = Vec::new();
    for a in agent_targets()? {
        if !a.dir.is_dir() {
            continue;
        }
        let result = match a.fmt {
            Fmt::Json => register_mcp_json(&a.config, &exe),
            Fmt::Toml => register_mcp_toml(&a.config, &exe),
        };
        if let Err(e) = result {
            errors.push(format!("{}: {e:#}", a.label));
        }
    }
    combine_errors(errors)?;
    Ok(agent_status(app_exe))
}

/// Remove our MCP registration from every agent's config (leaves their other config intact).
/// Tries every agent, then reports any failures.
pub fn uninstall_agents() -> Result<()> {
    let mut errors = Vec::new();
    for a in agent_targets()? {
        let result = match a.fmt {
            Fmt::Json => remove_mcp_json(&a.config),
            Fmt::Toml => remove_mcp_toml(&a.config),
        };
        if let Err(e) = result {
            errors.push(format!("{}: {e:#}", a.label));
        }
    }
    combine_errors(errors)
}

// ---------------------------------------------------------------------------------------------
// Codex and Gemini CLI memory hooks
// ---------------------------------------------------------------------------------------------

/// One agent's memory-hook state for Settings > Agents (`agent_hooks_status`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentHooksStatus {
    /// `codex` or `gemini`.
    pub agent: String,
    /// The file the hooks live in.
    pub config_path: String,
    /// Our SessionStart hook is in that file.
    pub installed: bool,
    /// Why Install would not help or is refused (hooks turned off in the agent's own config,
    /// or the agent is not set up here), else None.
    pub blocked: Option<String>,
    /// Codex only: a startup refresh changed our hook commands (the app moved, or a new
    /// AppImage name), and Codex runs a changed hook only after the user trusts it again in
    /// `/hooks`. Cleared by the next Install or Remove from Settings.
    pub needs_approval: bool,
}

/// An agent whose hooks Callimachus can install, and where.
struct HookTarget {
    id: &'static str,
    label: &'static str,
    /// The agent's config dir (presence marker, as in [`agent_targets`]).
    dir: PathBuf,
    /// The JSON file holding `hooks`: Codex's `hooks.json`, Gemini CLI's `settings.json`.
    file: PathBuf,
    specs: &'static [&'static HookSpec],
}

/// Codex and Gemini CLI, at the config dirs [`agent_targets`] resolves (so a Settings
/// override of Codex's home is honored). Codex reads `<CODEX_HOME>/hooks.json` as well as
/// `[hooks]` tables in `config.toml`; the JSON file keeps our edit out of the user's TOML.
/// (Codex warns at startup when one layer defines hooks in both, so a config.toml that
/// already defines hooks blocks the install, see [`CODEX_HOOKS_IN_TOML`].)
fn hook_targets() -> Result<Vec<HookTarget>> {
    Ok(agent_targets()?
        .into_iter()
        .filter_map(|a| match a.id {
            "codex" => Some(HookTarget {
                id: a.id,
                label: a.label,
                file: a.dir.join("hooks.json"),
                dir: a.dir,
                specs: &CODEX_HOOKS,
            }),
            "gemini" => Some(HookTarget {
                id: a.id,
                label: a.label,
                file: a.config,
                dir: a.dir,
                specs: &GEMINI_HOOKS,
            }),
            _ => None,
        })
        .collect())
}

fn find_hook_target(agent: &str) -> Result<HookTarget> {
    hook_targets()?
        .into_iter()
        .find(|t| t.id == agent)
        .ok_or_else(|| anyhow!("memory hooks are available for codex and gemini, not '{agent}'"))
}

/// Shown when the agent's own config turns its hook system off.
const CODEX_HOOKS_OFF: &str = "hooks are disabled in config.toml";
const GEMINI_HOOKS_OFF: &str = "hooks are disabled in settings.json (hooksConfig.enabled)";
/// Codex warns at every startup when one config layer defines hooks both in `config.toml` and
/// in `hooks.json` ("prefer a single representation", engine/discovery.rs), and our edits stay
/// out of the user's TOML, so a user who already keeps hooks there adds ours by hand.
const CODEX_HOOKS_IN_TOML: &str = "Codex hooks already live in config.toml; add ours there";
/// `hooksConfig.disabled` names our SessionStart hook, so Gemini CLI never runs it.
const GEMINI_HOOK_DISABLED: &str = "the Callimachus hook is turned off in settings.json \
     (hooksConfig.disabled lists callimachus-memory); remove it from that list";

/// Why installing `t`'s hooks would do nothing: the agent is not set up here, or its config
/// turns hooks off. Codex hooks are on by default and only `[features] hooks = false` (or the
/// older alias `codex_hooks = false`) turns them off; Gemini CLI's switch is
/// `hooksConfig.enabled`, default true (both checked against the vendor docs on 2026-11-06).
/// Unreadable config is not a block: the agent would report it itself.
fn hooks_blocked(t: &HookTarget) -> Option<String> {
    if !t.dir.is_dir() {
        return Some(format!(
            "{} is not set up on this machine (no {})",
            t.label,
            t.dir.display()
        ));
    }
    match t.id {
        "codex" => {
            let doc = std::fs::read_to_string(t.dir.join("config.toml"))
                .ok()?
                .parse::<toml_edit::DocumentMut>()
                .ok()?;
            if codex_hooks_disabled(&doc) {
                Some(CODEX_HOOKS_OFF.to_string())
            } else {
                codex_hooks_in_toml(&doc).then(|| CODEX_HOOKS_IN_TOML.to_string())
            }
        }
        "gemini" => {
            let v = read_json_lenient(&t.file)?;
            if gemini_hooks_disabled(&v) {
                Some(GEMINI_HOOKS_OFF.to_string())
            } else {
                gemini_hook_disabled_by_name(&v, t.specs[0])
                    .then(|| GEMINI_HOOK_DISABLED.to_string())
            }
        }
        _ => None,
    }
}

/// `config.toml` defines hook events itself (`[[hooks.SessionStart]]` and the like). The
/// `[hooks.state]` table Codex writes when the user trusts a hook is not a definition.
fn codex_hooks_in_toml(doc: &toml_edit::DocumentMut) -> bool {
    doc.get("hooks")
        .and_then(toml_edit::Item::as_table_like)
        .is_some_and(|t| t.iter().any(|(k, _)| k != "state"))
}

/// Gemini CLI skips a hook whose `name` is in `hooksConfig.disabled`.
fn gemini_hook_disabled_by_name(v: &Value, spec: &HookSpec) -> bool {
    let Some(name) = spec.name else {
        return false;
    };
    v.get("hooksConfig")
        .and_then(|h| h.get("disabled"))
        .and_then(Value::as_array)
        .is_some_and(|names| names.iter().any(|n| n.as_str() == Some(name)))
}

fn codex_hooks_disabled(doc: &toml_edit::DocumentMut) -> bool {
    let features = doc.get("features");
    ["hooks", "codex_hooks"]
        .iter()
        .any(|k| features.and_then(|f| f.get(k)).and_then(|v| v.as_bool()) == Some(false))
}

fn gemini_hooks_disabled(v: &Value) -> bool {
    v.get("hooksConfig")
        .and_then(|h| h.get("enabled"))
        .and_then(Value::as_bool)
        == Some(false)
}

/// Whether `t`'s file holds our SessionStart hook for it (whether or not the agent runs it).
fn agent_hooks_present(t: &HookTarget, exe: &Path) -> bool {
    read_json_lenient(&t.file)
        .and_then(|v| our_hook_command(v.as_object()?, t.specs[0].event, exe))
        .is_some_and(|cmd| our_hook_subcommand(&cmd) == Some(t.specs[0].subcommand))
}

/// [`agent_hooks_present`], and the agent runs it: a Gemini hook disabled by name is off.
fn agent_hooks_installed(t: &HookTarget, exe: &Path) -> bool {
    agent_hooks_present(t, exe)
        && !(t.id == "gemini"
            && read_json_lenient(&t.file)
                .is_some_and(|v| gemini_hook_disabled_by_name(&v, t.specs[0])))
}

/// A JSON config for a status read: comments stripped first (Gemini CLI allows them in its
/// settings.json), None when missing or unparseable.
fn read_json_lenient(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text)
        .ok()
        .or_else(|| serde_json::from_str(&strip_json_comments(&text)).ok())
}

/// `<index dir>/codex-hooks-repointed`: a refresh changed our Codex hook commands since the
/// user last installed them from Settings (see [`AgentHooksStatus::needs_approval`]).
const CODEX_REPOINTED_MARKER: &str = "codex-hooks-repointed";

fn codex_repointed_marker_path() -> PathBuf {
    crate::db::default_index_path()
        .parent()
        .map(|d| d.join(CODEX_REPOINTED_MARKER))
        .unwrap_or_else(|| PathBuf::from(CODEX_REPOINTED_MARKER))
}

fn agent_hooks_row(t: &HookTarget, exe: &Path) -> AgentHooksStatus {
    let installed = agent_hooks_installed(t, exe);
    AgentHooksStatus {
        agent: t.id.to_string(),
        config_path: t.file.display().to_string(),
        installed,
        blocked: hooks_blocked(t),
        needs_approval: installed && t.id == "codex" && codex_repointed_marker_path().exists(),
    }
}

/// Memory-hook state for Codex and Gemini CLI (both rows, detected or not; `blocked` says
/// which are not set up).
pub fn agent_hooks_status(app_exe: &Path) -> Vec<AgentHooksStatus> {
    let (exe, _) = resolve_exe(app_exe);
    hook_targets()
        .unwrap_or_default()
        .iter()
        .map(|t| agent_hooks_row(t, &exe))
        .collect()
}

/// Merge `specs` into the hooks file at `file` (created if missing), aimed at `exe`.
fn install_hooks_into(file: &Path, specs: &[&HookSpec], exe: &Path) -> Result<bool> {
    update_json_file(file, true, |obj| {
        for spec in specs {
            upsert_hook(obj, spec, &spec_command(exe, spec, spec.subcommand), exe)?;
        }
        Ok(())
    })
}

/// Strip our hooks on `specs`' events from `file`, leaving the rest of it.
fn remove_hooks_from(file: &Path, specs: &[&HookSpec], exe: &Path) -> Result<bool> {
    update_json_file(file, false, |obj| {
        for spec in specs {
            strip_our_hooks(obj, spec.event, exe);
        }
        Ok(())
    })
}

/// Install the memory hooks for `agent` (`codex` or `gemini`): SessionStart injects each
/// repo's memory (`cal hook --for <agent>`), SessionEnd queues the session for auto-distill,
/// and for Codex PreCompact snapshots the session. Merges into the agent's file with the
/// same atomic writer as the Claude Code hooks, keeping the user's own hooks; idempotent.
/// Refuses while blocked or while the app runs from a temporary location.
pub fn install_agent_hooks(app_exe: &Path, agent: &str) -> Result<AgentHooksStatus> {
    let t = find_hook_target(agent)?;
    if let Some(reason) = hooks_blocked(&t) {
        bail!("Can't add {} memory hooks: {reason}", t.label);
    }
    let exe = stable_app_exe(app_exe)?;
    install_hooks_into(&t.file, t.specs, &exe)?;
    clear_repointed(&t)?;
    Ok(agent_hooks_row(&t, &exe))
}

/// Remove our hooks from `agent`'s file (only ours; a missing file is left missing).
pub fn uninstall_agent_hooks(app_exe: &Path, agent: &str) -> Result<AgentHooksStatus> {
    let t = find_hook_target(agent)?;
    let (exe, _) = resolve_exe(app_exe);
    remove_hooks_from(&t.file, t.specs, &exe)?;
    clear_repointed(&t)?;
    Ok(agent_hooks_row(&t, &exe))
}

/// An Install or Remove from Settings comes with its own note about trusting new hooks, so
/// the "approve again" flag from an earlier refresh has done its job.
fn clear_repointed(t: &HookTarget) -> Result<()> {
    if t.id == "codex" {
        remove_if_exists(&codex_repointed_marker_path())?;
    }
    Ok(())
}

/// Startup refresh of the Codex and Gemini CLI hooks the user installed: the same in-place
/// re-point as [`refresh_hooks_in`]; never adds a missing hook or creates a file. Codex
/// trusts a hook by the hash of its declaration (command, timeout, matcher; `hook_hash` in
/// its engine/discovery.rs), so a changed Codex entry stops running until the user approves
/// it again in `/hooks`: that leaves the marker behind `needs_approval` for Settings to say so.
fn refresh_agent_hooks(exe: &Path) -> Result<bool> {
    let mut changed = false;
    let mut errors = Vec::new();
    for t in hook_targets()? {
        match update_json_file(&t.file, false, |obj| {
            refresh_specs_in(obj, t.specs, exe);
            Ok(())
        }) {
            Ok(c) => {
                changed |= c;
                if c && t.id == "codex" {
                    let marker = codex_repointed_marker_path();
                    if let Err(e) = std::fs::write(&marker, b"") {
                        errors.push(format!("{}: {e:#}", marker.display()));
                    }
                }
            }
            Err(e) => errors.push(format!("{}: {e:#}", t.label)),
        }
    }
    combine_errors(errors).map(|()| changed)
}

/// What Settings > Agents > Other agents' Remove does: every MCP entry ([`uninstall_agents`])
/// and every Codex and Gemini CLI memory hook. The Windows hook commands carry no
/// missing-binary guard, so hooks left behind by an uninstalled app would error at every
/// session start. Tries both, then reports any failures.
pub fn uninstall_agents_and_hooks() -> Result<()> {
    let mut errors = Vec::new();
    if let Err(e) = uninstall_agents() {
        errors.push(format!("{e:#}"));
    }
    if let Err(e) = uninstall_all_agent_hooks(&current_stable_exe()) {
        errors.push(format!("{e:#}"));
    }
    combine_errors(errors)
}

/// Remove our hooks from every agent's hooks file (the app's own uninstall).
fn uninstall_all_agent_hooks(exe: &Path) -> Result<()> {
    let mut errors = Vec::new();
    for t in hook_targets()? {
        if let Err(e) = remove_hooks_from(&t.file, t.specs, exe) {
            errors.push(format!("{} hooks: {e:#}", t.label));
        }
    }
    combine_errors(errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh temp dir per test (removed on drop), so tests never touch the real HOME.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "calli_integ_{tag}_{}_{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn ours_in(root: &Value, event: &str) -> usize {
        root["hooks"][event]
            .as_array()
            .map(|gs| {
                gs.iter()
                    .flat_map(|g| g["hooks"].as_array().unwrap().iter())
                    .filter(|h| our_hook_subcommand(h["command"].as_str().unwrap()).is_some())
                    .count()
            })
            .unwrap_or(0)
    }

    fn our_group<'a>(root: &'a Value, event: &str) -> &'a Value {
        root["hooks"][event]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| {
                g["hooks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|h| our_hook_subcommand(h["command"].as_str().unwrap()).is_some())
            })
            .expect("our hook group")
    }

    #[test]
    fn skill_is_embedded_and_has_the_cli_anchor() {
        assert!(SKILL_MD.contains("recall"));
        assert!(SKILL_MD.len() > 200);
        // render_skill injects the cal path after this line; if SKILL.md drops it, agents lose
        // the absolute-path fallback, so fail loudly here.
        assert!(SKILL_MD.contains(SKILL_CLI_ANCHOR));
    }

    #[test]
    fn rendered_skill_names_the_cli_path() {
        let rendered = render_skill(Path::new("/home/me/.local/bin/cal"));
        assert_ne!(rendered, SKILL_MD);
        assert!(rendered.contains("`/home/me/.local/bin/cal`"));
        assert!(rendered.contains("\"/home/me/.local/bin/cal\" search"));
        // Only an insertion: the bundled text is all still there, in order.
        let note_start = rendered.find("\nOn this machine").unwrap();
        let note_end = rendered[note_start + 1..].find('\n').unwrap() + note_start + 2;
        let without = format!("{}{}", &rendered[..note_start], &rendered[note_end..]);
        assert_eq!(without, SKILL_MD);
    }

    #[test]
    fn every_shipped_skill_is_recognized() {
        let current = fnv1a64(SKILL_MD.replace("\r\n", "\n").as_bytes());
        assert!(
            SHIPPED_SKILL_HASHES.contains(&current),
            "resources/recall/SKILL.md changed: add {current:#018x} to SHIPPED_SKILL_HASHES"
        );
        assert_eq!(
            fnv1a64(b"a"),
            0xaf63_dc4c_8601_ec8c,
            "FNV-1a reference vector"
        );
    }

    #[test]
    fn untouched_skills_are_recognized_and_edits_are_not() {
        // This version's text, raw or rendered for any CLI path, with either line ending.
        assert!(is_untouched_skill(SKILL_MD));
        assert!(is_untouched_skill(&render_skill(Path::new(
            "/old/.local/bin/cal"
        ))));
        assert!(is_untouched_skill(
            &render_skill(Path::new("/x/cal")).replace('\n', "\r\n")
        ));
        assert_eq!(
            strip_cli_note(&render_skill(Path::new("/x/cal"))).as_deref(),
            Some(SKILL_MD)
        );
        assert_eq!(strip_cli_note(SKILL_MD), None);
        // A user's edit is theirs.
        assert!(!is_untouched_skill(&format!("{SKILL_MD}\nMy own note.\n")));
        assert!(!is_untouched_skill(
            &render_skill(Path::new("/x/cal")).replace("recall", "find")
        ));
    }

    #[test]
    fn removed_marker_merges_and_parses_leniently() {
        let mut later = RemovedIntegration {
            claude_hooks: vec!["SessionStart".into()],
            ..Default::default()
        };
        assert!(!later.is_empty());
        later.merge(RemovedIntegration {
            claude_hooks: vec!["SessionStart".into(), "PreCompact".into()],
            agents: vec!["codex".into()],
            cal: true,
            ..Default::default()
        });
        assert_eq!(later.claude_hooks, ["SessionStart", "PreCompact"]);
        assert_eq!(later.agents, ["codex"]);
        assert!(later.cal && !later.skill && !later.claude_mcp);

        // Camel-case on disk, unknown and missing fields tolerated (other builds' markers).
        let json = serde_json::to_value(&later).unwrap();
        assert_eq!(json["claudeHooks"][1], "PreCompact");
        let parsed: RemovedIntegration =
            serde_json::from_str(r#"{"skill":true,"futureField":1}"#).unwrap();
        assert!(parsed.skill && parsed.claude_hooks.is_empty());
        assert!(RemovedIntegration::default().is_empty());
    }

    #[test]
    fn register_mcp_merges_without_clobbering() {
        let tmp = TempDir::new("mcp");
        let cfg = tmp.path().join(".claude.json");
        std::fs::write(
            &cfg,
            r#"{"numStartups":7,"mcpServers":{"other":{"command":"x"}}}"#,
        )
        .unwrap();

        register_mcp_json(&cfg, Path::new("/app")).unwrap();
        let back = read_json(&cfg);
        assert_eq!(back["numStartups"], 7);
        assert_eq!(back["mcpServers"]["other"]["command"], "x");
        assert_eq!(back["mcpServers"]["callimachus"]["command"], "/app");
        assert_eq!(back["mcpServers"]["callimachus"]["args"][0], "--mcp");
        assert!(mcp_registered_json(&cfg, Path::new("/app")));

        remove_mcp_json(&cfg).unwrap();
        let back = read_json(&cfg);
        assert!(back["mcpServers"].get("callimachus").is_none());
        assert_eq!(back["mcpServers"]["other"]["command"], "x");
    }

    #[test]
    fn config_writes_refuse_unreadable_or_invalid_files() {
        let tmp = TempDir::new("invalid");
        // Not JSON: must error and leave the bytes alone.
        let bad = tmp.path().join("settings.json");
        std::fs::write(&bad, "{ not json").unwrap();
        assert!(register_mcp_json(&bad, Path::new("/app")).is_err());
        assert_eq!(std::fs::read_to_string(&bad).unwrap(), "{ not json");

        // UTF-16 (Windows PowerShell's default redirect encoding) isn't UTF-8: never treated as
        // empty and overwritten.
        let utf16 = tmp.path().join("utf16.json");
        let bytes: Vec<u8> = [0xFF, 0xFE, b'{', 0, b'}', 0].to_vec();
        std::fs::write(&utf16, &bytes).unwrap();
        assert!(register_mcp_json(&utf16, Path::new("/app")).is_err());
        assert_eq!(std::fs::read(&utf16).unwrap(), bytes);

        // A missing file is only created when asked to.
        let missing = tmp.path().join("missing.json");
        remove_mcp_json(&missing).unwrap();
        assert!(!missing.exists());
    }

    #[test]
    fn unchanged_config_is_not_rewritten() {
        let tmp = TempDir::new("unchanged");
        let cfg = tmp.path().join("mcp.json");
        // Compact formatting: a rewrite would pretty-print it, so equal bytes prove no write.
        let original = r#"{"mcpServers":{"other":{"command":"x"}}}"#;
        std::fs::write(&cfg, original).unwrap();
        remove_mcp_json(&cfg).unwrap(); // our entry isn't there: nothing to do
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original);
        assert!(!update_json_file(&cfg, false, |_| Ok(())).unwrap());
    }

    #[test]
    fn cleanup_days_read_and_write_keep_the_other_settings() {
        let tmp = TempDir::new("cleanup");
        let path = tmp.path().join("settings.json");
        // No file: unset, and removing the key creates nothing.
        assert_eq!(cleanup_days_in(&path).unwrap(), None);
        set_cleanup_days_in(&path, None).unwrap();
        assert!(!path.exists());

        std::fs::write(&path, r#"{"theme":"dark","hooks":{"SessionStart":[]}}"#).unwrap();
        assert_eq!(cleanup_days_in(&path).unwrap(), None);
        set_cleanup_days_in(&path, Some(365)).unwrap();
        assert_eq!(cleanup_days_in(&path).unwrap(), Some(365));
        let v = read_json(&path);
        assert_eq!(v["theme"], "dark");
        assert!(v["hooks"]["SessionStart"].is_array());

        // Restoring "unset" removes only our key.
        set_cleanup_days_in(&path, None).unwrap();
        let v = read_json(&path);
        assert!(v.get(CLEANUP_KEY).is_none());
        assert_eq!(v["theme"], "dark");

        // Claude Code rejects 0, so we never write it.
        assert!(set_cleanup_days_in(&path, Some(0)).is_err());
        // An unparseable file is reported and left alone.
        std::fs::write(&path, "{ not json").unwrap();
        assert!(cleanup_days_in(&path).is_err());
        assert!(set_cleanup_days_in(&path, Some(365)).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }

    #[test]
    fn atomic_write_replaces_content_and_leaves_no_temp_files() {
        let tmp = TempDir::new("atomic");
        let cfg = tmp.path().join("config.json");
        std::fs::write(&cfg, "{}").unwrap();
        write_atomic(&cfg, b"{\"a\":1}").unwrap();
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), "{\"a\":1}");
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_keeps_mode_and_symlinks() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new("atomic_unix");

        // ~/.claude.json is 0600; a rewrite must not widen it.
        let private = tmp.path().join(".claude.json");
        std::fs::write(&private, "{}").unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o600)).unwrap();
        register_mcp_json(&private, Path::new("/app")).unwrap();
        let mode = std::fs::metadata(&private).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        // A 0644 file stays 0644.
        let public = tmp.path().join("settings.json");
        std::fs::write(&public, "{}").unwrap();
        std::fs::set_permissions(&public, std::fs::Permissions::from_mode(0o644)).unwrap();
        register_mcp_json(&public, Path::new("/app")).unwrap();
        let mode = std::fs::metadata(&public).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);

        // New files are owner-only.
        let fresh = tmp.path().join("new.json");
        register_mcp_json(&fresh, Path::new("/app")).unwrap();
        let mode = std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        // A dotfiles-style symlink stays a symlink; its target gets the new content.
        let real = tmp.path().join("dotfiles-settings.json");
        std::fs::write(&real, r#"{"model":"opus"}"#).unwrap();
        let link = tmp.path().join("linked-settings.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        register_mcp_json(&link, Path::new("/app")).unwrap();
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        let back = read_json(&real);
        assert_eq!(back["model"], "opus");
        assert_eq!(back["mcpServers"]["callimachus"]["command"], "/app");
    }

    #[test]
    fn hook_commands_are_guarded_and_recognized() {
        let unix = hook_command_for(
            "/x/Callimachus.app/Contents/MacOS/callimachus",
            "hook",
            false,
        );
        assert_eq!(
            unix,
            "[ ! -x \"/x/Callimachus.app/Contents/MacOS/callimachus\" ] || \
             \"/x/Callimachus.app/Contents/MacOS/callimachus\" hook"
        );
        let windows = hook_command_for(
            r"C:\Program Files\Callimachus\callimachus.exe",
            "hook",
            true,
        );
        assert_eq!(
            windows,
            r#""C:\Program Files\Callimachus\callimachus.exe" hook"#
        );

        // Current, legacy and Windows forms are all ours; the user's own hooks are not.
        let exe = Path::new("/opt/apps/recall-app");
        assert!(is_our_hook(&unix, exe));
        assert!(is_our_hook(&windows, exe));
        assert!(is_our_hook(
            &hook_command_for("/x/cal", "snapshot-hook", false),
            exe
        ));
        assert!(is_our_hook(
            &hook_command_for("/x/cal", "recall-now", false),
            exe
        ));
        assert!(is_our_hook("\"/x/cal\" hook", exe));
        assert!(is_our_hook("\"/x/cal\" snapshot-hook", exe));
        assert!(is_our_hook("\"/x/cal\" recall-now", exe));
        // A renamed app binary is recognized by being this exe.
        assert!(is_our_hook(
            &hook_command_for("/opt/apps/recall-app", "hook", false),
            exe
        ));
        assert!(!is_our_hook("\"/x/cal\" other", exe));
        assert!(!is_our_hook("my-own-snapshot-hook", exe));
        assert!(!is_our_hook("echo hook", exe));
        assert!(!is_our_hook("\"/x/cal\"hook", exe));
        // Our shape, someone else's binary: not ours (refresh must never take it over).
        assert!(!is_our_hook("\"/opt/tool/bin/tool\" hook", exe));
        assert!(!is_our_hook(
            &hook_command_for("/opt/tool/bin/tool", "recall-now", false),
            exe
        ));
    }

    #[test]
    fn hook_target_round_trips_through_shell_quoting() {
        let weird = "/Users/a b/$HOME`x`/\"q\"/callimachus";
        let cmd = hook_command_for(weird, "hook", false);
        assert_eq!(hook_target(&cmd, true), Some(PathBuf::from(weird)));
        assert_eq!(
            hook_target("\"/Users/x/.local/bin/cal\" hook", true),
            Some(PathBuf::from("/Users/x/.local/bin/cal"))
        );
        // Windows commands are unescaped: backslashes are path separators.
        let win = hook_command_for(r"\\server\share\callimachus.exe", "hook", true);
        assert_eq!(
            hook_target(&win, false),
            Some(PathBuf::from(r"\\server\share\callimachus.exe"))
        );
        assert_eq!(hook_target("echo hi", true), None);
    }

    #[test]
    fn hook_install_is_idempotent_with_matcher_and_timeouts() {
        let mut root: Value = serde_json::from_str(
            r#"{"model":"opus",
                "hooks":{
                  "PreToolUse":[{"hooks":[{"type":"command","command":"echo hi"}]}],
                  "SessionStart":[{"hooks":[{"type":"command","command":"my-own-hook"}]}]
                }}"#,
        )
        .unwrap();
        let exe = Path::new("/Applications/Callimachus.app/Contents/MacOS/callimachus");
        for _ in 0..2 {
            apply_base_hooks(root.as_object_mut().unwrap(), exe).unwrap();
        }

        assert_eq!(root["model"], "opus");
        assert_eq!(
            root["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "echo hi"
        );
        assert_eq!(
            ours_in(&root, "SessionStart"),
            1,
            "no duplicate on reinstall"
        );
        assert_eq!(ours_in(&root, "PreCompact"), 1);
        assert_eq!(ours_in(&root, "SessionEnd"), 1);
        assert_eq!(
            ours_in(&root, "SubagentStop"),
            0,
            "SubagentStop is no longer installed"
        );
        assert_eq!(
            ours_in(&root, "UserPromptSubmit"),
            0,
            "proactive recall stays opt-in"
        );
        assert!(root["hooks"]["SessionStart"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap())
            .any(|h| h["command"] == "my-own-hook"));

        let session = our_group(&root, "SessionStart");
        assert_eq!(session["matcher"], "startup|clear|compact");
        assert_eq!(session["hooks"][0]["timeout"], 10);
        assert_eq!(session["hooks"][0]["type"], "command");
        let expected_cmd = hook_command(exe, "hook");
        assert_eq!(session["hooks"][0]["command"], expected_cmd.as_str());
        let compact = our_group(&root, "PreCompact");
        assert!(compact.get("matcher").is_none());
        assert_eq!(compact["hooks"][0]["timeout"], 10);
        let end = our_group(&root, "SessionEnd");
        assert!(end.get("matcher").is_none());
        assert_eq!(end["hooks"][0]["timeout"], 5);
        assert_eq!(
            end["hooks"][0]["command"],
            hook_command(exe, "session-end").as_str()
        );
        assert!(is_our_hook(&hook_command(exe, "session-end"), exe));
        assert!(event_has_our_hook(&root, "SessionStart", exe));

        // Proactive recall: its own timeout, and survives a base reinstall.
        upsert_hook(
            root.as_object_mut().unwrap(),
            &PROACTIVE_RECALL_HOOK,
            &hook_command(exe, "recall-now"),
            exe,
        )
        .unwrap();
        apply_base_hooks(root.as_object_mut().unwrap(), exe).unwrap();
        assert_eq!(ours_in(&root, "UserPromptSubmit"), 1);
        assert_eq!(
            our_group(&root, "UserPromptSubmit")["hooks"][0]["timeout"],
            15
        );
    }

    #[test]
    fn reinstall_strips_legacy_subagent_stop_and_old_forms() {
        // What an older version left behind: unguarded `cal`-link hooks, no matcher/timeout, a
        // SubagentStop hook, next to the user's own hooks.
        let mut root: Value = serde_json::from_str(
            r#"{"hooks":{
                  "SessionStart":[{"hooks":[{"type":"command","command":"\"/Users/x/.local/bin/cal\" hook"}]}],
                  "PreCompact":[{"hooks":[{"type":"command","command":"\"/Users/x/.local/bin/cal\" snapshot-hook"}]}],
                  "SubagentStop":[
                    {"hooks":[{"type":"command","command":"\"/Users/x/.local/bin/cal\" snapshot-hook"}]},
                    {"hooks":[{"type":"command","command":"notify-me"}]}
                  ]
                }}"#,
        )
        .unwrap();
        let exe = Path::new("/opt/callimachus");
        apply_base_hooks(root.as_object_mut().unwrap(), exe).unwrap();
        assert_eq!(ours_in(&root, "SessionStart"), 1);
        assert_eq!(ours_in(&root, "PreCompact"), 1);
        assert_eq!(ours_in(&root, "SubagentStop"), 0);
        // The user's SubagentStop hook is kept.
        assert_eq!(
            root["hooks"]["SubagentStop"][0]["hooks"][0]["command"],
            "notify-me"
        );
        assert_eq!(
            our_group(&root, "SessionStart")["hooks"][0]["command"],
            hook_command(exe, "hook").as_str()
        );

        // When SubagentStop held only our hook, the emptied key goes too.
        let mut only_ours: Value = serde_json::from_str(
            r#"{"hooks":{"SubagentStop":[{"hooks":[{"type":"command","command":"\"/x/cal\" snapshot-hook"}]}]}}"#,
        )
        .unwrap();
        apply_base_hooks(only_ours.as_object_mut().unwrap(), exe).unwrap();
        assert!(only_ours["hooks"].get("SubagentStop").is_none());
    }

    #[test]
    fn plugin_is_detected_only_when_enabled() {
        assert!(plugin_enabled_in(
            &json!({"enabledPlugins": {"callimachus@callimachus": true}})
        ));
        // The same plugin from another marketplace (a fork, a directory listing).
        assert!(plugin_enabled_in(
            &json!({"enabledPlugins": {"callimachus@claude-plugins-official": true}})
        ));
        for off in [
            json!({}),
            json!({"enabledPlugins": {"callimachus@callimachus": false}}),
            json!({"enabledPlugins": {"callimachus-extras@callimachus": true}}),
            json!({"enabledPlugins": {"other@callimachus": true}}),
            json!({"enabledPlugins": {"callimachus": true}}),
            json!({"enabledPlugins": {"callimachus@callimachus": "yes"}}),
            json!({"enabledPlugins": ["callimachus@callimachus"]}),
        ] {
            assert!(!plugin_enabled_in(&off), "{off}");
        }
        assert_eq!(
            plugin_manages(&json!({"enabledPlugins": {"callimachus@callimachus": true}})),
            cfg!(unix),
            "Windows keeps the app's own install: the plugin's sh hooks find no cal there"
        );
    }

    /// Handing over to the plugin strips the base hooks the plugin ships, keeps the opt-in
    /// proactive-recall hook (the plugin has none) and the user's own hooks.
    #[test]
    fn hand_over_strips_base_hooks_only() {
        let exe = Path::new("/x/callimachus");
        let mut root = json!({
            "hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "echo mine"}]}]}
        });
        let obj = root.as_object_mut().unwrap();
        apply_base_hooks(obj, exe).unwrap();
        upsert_hook(
            obj,
            &PROACTIVE_RECALL_HOOK,
            &hook_command(exe, "recall-now"),
            exe,
        )
        .unwrap();
        strip_base_hooks(obj, exe);
        for spec in BASE_HOOKS {
            assert_eq!(ours_in(&root, spec.event), 0, "{} handed over", spec.event);
        }
        assert_eq!(ours_in(&root, PROACTIVE_RECALL_HOOK.event), 1);
        assert_eq!(
            root["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            "echo mine"
        );
    }

    /// The hand-over records what it removed, and turning the plugin off puts back exactly
    /// that: a piece the user removed or replaced in the meantime is left as they made it,
    /// and pieces the app never installed are not added.
    #[test]
    fn plugin_hand_over_round_trips_through_its_marker() {
        let tmp = TempDir::new("plugin_round_trip");
        let files = ClaudeFiles {
            settings: tmp.path().join("settings.json"),
            skill: tmp.path().join("skills/recall/SKILL.md"),
            config: tmp.path().join(".claude.json"),
        };
        let marker = tmp.path().join(PLUGIN_HANDOVER_MARKER);
        let exe = Path::new("/Applications/Callimachus.app/Contents/MacOS/callimachus");
        let cli = Path::new("/Users/me/.local/bin/cal");
        update_json_file(&files.settings, true, |obj| apply_base_hooks(obj, exe)).unwrap();
        write_skill_at(&files.skill, cli).unwrap();
        register_mcp_json(&files.config, exe).unwrap();

        assert!(hand_over_in(&files, &marker, exe).unwrap());
        let settings = read_json(&files.settings);
        for spec in BASE_HOOKS {
            assert_eq!(ours_in(&settings, spec.event), 0, "{}", spec.event);
        }
        assert!(!files.skill.exists());
        assert!(
            !files.skill.parent().unwrap().exists(),
            "the empty recall dir goes too"
        );
        assert!(read_json(&files.config)["mcpServers"]
            .get(MCP_NAME)
            .is_none());
        let handed = read_handover(&marker).unwrap().unwrap();
        assert_eq!(handed.hooks, ["SessionStart", "PreCompact", "SessionEnd"]);
        assert!(handed.skill && handed.mcp);
        // Again (Install re-run): nothing left to take, and the record is kept.
        assert!(!hand_over_in(&files, &marker, exe).unwrap());
        assert_eq!(read_handover(&marker).unwrap().unwrap(), handed);

        // The plugin goes off; meanwhile the user wrote their own MCP entry.
        update_json_file(&files.config, false, |obj| {
            obj.insert("mcpServers".into(), json!({MCP_NAME: {"command": "npx"}}));
            Ok(())
        })
        .unwrap();
        assert!(take_back_in(&files, &marker, exe, cli).unwrap());
        let settings = read_json(&files.settings);
        for spec in BASE_HOOKS {
            assert_eq!(ours_in(&settings, spec.event), 1, "{}", spec.event);
        }
        assert_eq!(
            std::fs::read_to_string(&files.skill).unwrap(),
            render_skill(cli)
        );
        assert_eq!(
            read_json(&files.config)["mcpServers"][MCP_NAME]["command"],
            "npx"
        );
        assert!(!marker.exists());
        assert!(
            !take_back_in(&files, &marker, exe, cli).unwrap(),
            "no marker: nothing to restore"
        );

        // Only what was handed over comes back: a user who had removed SessionEnd and the
        // MCP entry before enabling the plugin keeps them removed.
        update_json_file(&files.settings, false, |obj| {
            strip_our_hooks(obj, SESSION_END_HOOK.event, exe);
            Ok(())
        })
        .unwrap();
        std::fs::write(&files.config, "{}").unwrap();
        hand_over_in(&files, &marker, exe).unwrap();
        take_back_in(&files, &marker, exe, cli).unwrap();
        let settings = read_json(&files.settings);
        assert_eq!(ours_in(&settings, SESSION_START_HOOK.event), 1);
        assert_eq!(ours_in(&settings, SESSION_END_HOOK.event), 0);
        assert!(read_json(&files.config).get("mcpServers").is_none());
    }

    /// Only the entry the app registered (`<app> --mcp`) gives way to the plugin's server; a
    /// hand-written one (which Claude Code dedupes against the plugin's by command) stays.
    #[test]
    fn hand_over_removes_only_the_app_mcp_entry() {
        let tmp = TempDir::new("plugin_mcp");
        let cfg = tmp.path().join(".claude.json");
        let exe = Path::new("/Applications/Callimachus.app/Contents/MacOS/callimachus");
        std::fs::write(&cfg, r#"{"mcpServers": {"other": {"command": "x"}}}"#).unwrap();
        register_mcp_json(&cfg, exe).unwrap();
        assert!(remove_app_mcp_json(&cfg).unwrap());
        let v = read_json(&cfg);
        assert!(v["mcpServers"].get(MCP_NAME).is_none());
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        assert!(
            !remove_app_mcp_json(&cfg).unwrap(),
            "nothing left to remove"
        );

        std::fs::write(
            &cfg,
            r#"{"mcpServers": {"callimachus": {"command": "npx", "args": ["-y", "callimachus-mcp"]}}}"#,
        )
        .unwrap();
        assert!(!remove_app_mcp_json(&cfg).unwrap());
        assert_eq!(read_json(&cfg)["mcpServers"][MCP_NAME]["command"], "npx");
    }

    /// Hooks only the plugin ships, after the base ones in its hooks.json. The SessionStart
    /// guard keeps the index fresh for a plugin user without the app (`cal index
    /// --if-stale`, see `headless::stale_index_gate`); the app's own install leaves it out,
    /// since the app's watcher does that job. On `startup` and `resume`, the starts of real
    /// work; `clear` and `compact` happen mid-session, after one of those already checked.
    const PLUGIN_ONLY_HOOKS: [HookSpec; 1] = [HookSpec {
        event: "SessionStart",
        subcommand: "index --if-stale",
        matcher: Some("startup|resume"),
        // The guard probes three locks and at most reads one row; the pass runs detached.
        timeout_secs: 5,
        millis: false,
        name: None,
        powershell: false,
    }];

    /// The repo's Claude Code plugin (plugins/claude-code) ships what `install()` would: the
    /// bundled skill byte for byte, the base hooks with the same events, matchers, timeouts and
    /// subcommands (as scripts that run the `cal` link the app installs, guarded like ours) plus
    /// `PLUGIN_ONLY_HOOKS`, the MCP server under our name, and the app's version, which
    /// sync-versions.mjs keeps in step.
    #[test]
    fn claude_plugin_mirrors_the_app_install() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../plugins/claude-code");
        let read = |rel: &str| {
            std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
        };
        // The plugin's copy is the bundled text plus a fixed note on where `cal` lives (the
        // app renders that note per machine instead); plugins/test pins the same suffix.
        const PLUGIN_SKILL_SUFFIX: &str = "\n## With the Claude Code plugin\n\nThe `cal` CLI is \
            `~/.local/bin/cal` (on Windows, `cal.exe` next to the Callimachus app). If a bare \
            `cal` is not found or prints a calendar (the system `cal(1)` winning on PATH), call \
            it by that full path. The MCP prompts' slash commands carry the plugin's prefix, \
            for example `/mcp__plugin_callimachus_callimachus__recall`.\n";
        assert_eq!(
            read("skills/recall/SKILL.md"),
            format!("{SKILL_MD}{PLUGIN_SKILL_SUFFIX}"),
            "plugins/claude-code/skills/recall/SKILL.md is resources/recall/SKILL.md plus the \
             plugin note"
        );

        // Anthropic's directory allows no variable but `${CLAUDE_PLUGIN_ROOT}` in a hook
        // command, so each plugin hook is a plain sh script in scripts/ that runs the linked
        // `cal` and exits quietly without it (the app's own install writes that guard inline).
        let cal = "~/.local/bin/cal";
        // A plugin-only hook also runs only when the linked `cal` lists its command in `cal
        // help`: the marketplace can update the plugin ahead of the app, and an older app's
        // `cal` answers an unknown command with exit 1, a hook error in every session.
        let probe = |spec: &HookSpec| {
            let cmd = spec.subcommand.split(' ').next().unwrap();
            let line = format!("\n  cal {cmd} ");
            assert!(
                crate::cli_core::USAGE.contains(&line),
                "`cal help` must list `cal {cmd}` for the plugin's probe"
            );
            format!("{cal} help 2>/dev/null | grep -q '^  cal {cmd} ' || exit 0\n")
        };
        let mut expected = Map::new();
        let plugin_only = PLUGIN_ONLY_HOOKS.iter().map(|s| (s, true));
        for (spec, probed) in BASE_HOOKS.iter().map(|s| (*s, false)).chain(plugin_only) {
            let name = spec.subcommand.split(' ').next().unwrap();
            let script = format!("scripts/{name}.sh");
            let body = read(&script);
            assert!(
                body.starts_with("#!/bin/sh\n"),
                "{script} is a plain sh script"
            );
            assert!(
                body.contains(&format!("[ -x {cal} ] || exit 0\n")),
                "{script} exits quietly when cal is not installed"
            );
            if probed {
                assert!(
                    body.contains(&probe(spec)),
                    "{script} asks `cal help` first"
                );
            }
            assert!(
                body.ends_with(&format!("exec {cal} {}\n", spec.subcommand)),
                "{script} ends by running `cal {}`",
                spec.subcommand
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(root.join(&script))
                    .unwrap()
                    .permissions()
                    .mode();
                assert!(
                    mode & 0o111 != 0,
                    "{script} is executable (git keeps the mode)"
                );
            }
            let mut group = Map::new();
            if let Some(m) = spec.matcher {
                group.insert("matcher".into(), json!(m));
            }
            group.insert(
                "hooks".into(),
                json!([{
                    "type": "command",
                    "command": format!("\"${{CLAUDE_PLUGIN_ROOT}}\"/{script}"),
                    "timeout": spec.timeout_secs,
                }]),
            );
            let groups = expected.entry(spec.event).or_insert_with(|| json!([]));
            groups.as_array_mut().unwrap().push(Value::Object(group));
        }
        let hooks: Value = serde_json::from_str(&read("hooks/hooks.json")).unwrap();
        assert_eq!(hooks["hooks"], Value::Object(expected));

        // Anthropic's directory flags an `npx` package as a launcher it cannot read, so the npm
        // launcher is vendored into the plugin, byte for byte, and run with node. It downloads
        // the release named by its own package.json, which sync-versions.mjs stamps.
        let mcp: Value = serde_json::from_str(&read(".mcp.json")).unwrap();
        let version = env!("CALLIMACHUS_VERSION");
        assert_eq!(
            mcp["mcpServers"][MCP_NAME],
            json!({"command": "node", "args": ["${CLAUDE_PLUGIN_ROOT}/mcp/bin/callimachus-mcp.mjs"]})
        );
        let launcher: Value = serde_json::from_str(&read("mcp/package.json")).unwrap();
        assert_eq!(launcher["version"], version);
        let npm = root.join("../../packages/callimachus-mcp");
        for rel in [
            "bin/callimachus-mcp.mjs",
            "lib/download.mjs",
            "lib/install-cal.mjs",
        ] {
            assert!(
                read(&format!("mcp/{rel}")) == std::fs::read_to_string(npm.join(rel)).unwrap(),
                "plugins/claude-code/mcp/{rel} is a copy of packages/callimachus-mcp/{rel}"
            );
        }

        // The directory listing's icon: the app's own 512 px PNG.
        let icon = std::fs::read(root.join(".claude-plugin/icon.png")).unwrap();
        assert!(icon.starts_with(b"\x89PNG\r\n\x1a\n"), "icon.png is a PNG");
        let app_icon = Path::new(env!("CARGO_MANIFEST_DIR")).join("../icons/icon.png");
        assert!(
            icon == std::fs::read(app_icon).unwrap(),
            "plugins/claude-code/.claude-plugin/icon.png is apps/desktop/src-tauri/icons/icon.png"
        );

        let manifest: Value = serde_json::from_str(&read(".claude-plugin/plugin.json")).unwrap();
        assert_eq!(manifest["version"], version);
        let market: Value = serde_json::from_str(
            &std::fs::read_to_string(root.join("../../.claude-plugin/marketplace.json")).unwrap(),
        )
        .unwrap();
        let entry = &market["plugins"][0];
        assert_eq!(
            format!(
                "{}@{}",
                manifest["name"].as_str().unwrap(),
                market["name"].as_str().unwrap()
            ),
            PLUGIN_ID
        );
        assert_eq!(entry["name"], manifest["name"]);
        assert_eq!(entry["source"]["path"], "plugins/claude-code");
        assert_eq!(entry["source"]["ref"], format!("v{version}"));
    }

    /// The error-playbook hooks are opt-in like proactive recall: Install leaves them out,
    /// a reinstall re-shapes them once opted in (both events, even when only one was there),
    /// a plugin hand-over keeps them, uninstall removes them, and the user's own PostToolUse
    /// hooks are never touched.
    #[test]
    fn error_recall_hooks_are_opt_in_and_survive_reinstall_and_hand_over() {
        let exe = Path::new("/Applications/Callimachus.app/Contents/MacOS/callimachus");
        let mut root = json!({"hooks": {"PostToolUse": [
            {"matcher": "Bash", "hooks": [{"type": "command", "command": "lint-it"}]}
        ]}});
        apply_base_hooks(root.as_object_mut().unwrap(), exe).unwrap();
        assert_eq!(
            ours_in(&root, "PostToolUse"),
            0,
            "off until the user opts in"
        );
        assert_eq!(ours_in(&root, "PostToolUseFailure"), 0);

        upsert_hook(
            root.as_object_mut().unwrap(),
            &ERROR_RECALL_HOOK,
            &hook_command(exe, "recall-error"),
            exe,
        )
        .unwrap();
        for _ in 0..2 {
            apply_base_hooks(root.as_object_mut().unwrap(), exe).unwrap();
        }
        for event in ["PostToolUse", "PostToolUseFailure"] {
            assert_eq!(ours_in(&root, event), 1, "{event}");
            let group = our_group(&root, event);
            assert_eq!(group["matcher"], "Bash", "{event}");
            assert_eq!(group["hooks"][0]["timeout"], 5, "{event}");
            assert_eq!(
                group["hooks"][0]["command"],
                hook_command(exe, "recall-error").as_str()
            );
        }
        assert!(is_our_hook(&hook_command(exe, "recall-error"), exe));
        assert!(event_has_our_hook(&root, ERROR_RECALL_HOOK.event, exe));

        strip_base_hooks(root.as_object_mut().unwrap(), exe);
        assert_eq!(ours_in(&root, "PostToolUse"), 1, "the plugin ships none");
        assert_eq!(ours_in(&root, "PostToolUseFailure"), 1);

        for event in ALL_HOOK_EVENTS {
            strip_our_hooks(root.as_object_mut().unwrap(), event, exe);
        }
        assert_eq!(ours_in(&root, "PostToolUse"), 0);
        assert!(root["hooks"].get("PostToolUseFailure").is_none());
        assert_eq!(
            root["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
            "lint-it"
        );

        // The uninstaller's record carries it, from older markers too.
        let mut removed: RemovedIntegration =
            serde_json::from_str(r#"{"proactiveRecall": true}"#).unwrap();
        assert!(!removed.error_recall);
        removed.merge(RemovedIntegration {
            error_recall: true,
            ..Default::default()
        });
        assert!(removed.error_recall && removed.proactive_recall);
    }

    #[test]
    fn uninstall_strips_every_event_and_empty_keys() {
        let mut root = json!({"model": "opus"});
        let obj = root.as_object_mut().unwrap();
        let exe = Path::new("/x/callimachus");
        apply_base_hooks(obj, exe).unwrap();
        upsert_hook(
            obj,
            &PROACTIVE_RECALL_HOOK,
            &hook_command(exe, "recall-now"),
            exe,
        )
        .unwrap();
        for event in ALL_HOOK_EVENTS {
            strip_our_hooks(obj, event, exe);
        }
        for event in ALL_HOOK_EVENTS {
            assert_eq!(ours_in(&root, event), 0, "{event} cleared on uninstall");
        }
        assert!(root.get("hooks").is_none(), "no empty hooks object left");
        assert_eq!(root["model"], "opus");
    }

    /// An install from before SessionEnd existed gets it once, from the binary its SessionStart
    /// hook runs; without our SessionStart hook, or with SessionEnd already there, nothing
    /// changes. While SessionStart runs another live binary (an installed release while a dev
    /// build runs), that binary may not know `session-end`, so nothing is added either.
    #[test]
    fn session_end_joins_an_existing_install_once() {
        let tmp = TempDir::new("session_end");
        let exe = tmp.path().join("callimachus");
        std::fs::write(&exe, "").unwrap();
        let other = tmp.path().join("installed-callimachus");
        std::fs::write(&other, "").unwrap();

        let mut bare = json!({"hooks": {"PreToolUse": []}});
        assert_eq!(
            add_session_end_hook_in(bare.as_object_mut().unwrap(), &exe),
            SessionEndAdd::NoInstall
        );
        assert_eq!(ours_in(&bare, "SessionEnd"), 0);

        let mut elsewhere = json!({});
        upsert_hook(
            elsewhere.as_object_mut().unwrap(),
            &SESSION_START_HOOK,
            &hook_command(&other, "hook"),
            &exe,
        )
        .unwrap();
        assert_eq!(
            add_session_end_hook_in(elsewhere.as_object_mut().unwrap(), &exe),
            SessionEndAdd::OtherBinary
        );
        assert_eq!(ours_in(&elsewhere, "SessionEnd"), 0);

        let mut root = json!({});
        upsert_hook(
            root.as_object_mut().unwrap(),
            &SESSION_START_HOOK,
            &hook_command(&exe, "hook"),
            &exe,
        )
        .unwrap();
        assert_eq!(
            add_session_end_hook_in(root.as_object_mut().unwrap(), &exe),
            SessionEndAdd::Added
        );
        assert_eq!(
            our_group(&root, "SessionEnd")["hooks"][0]["command"],
            hook_command(&exe, "session-end").as_str()
        );
        assert_eq!(
            add_session_end_hook_in(root.as_object_mut().unwrap(), &exe),
            SessionEndAdd::Present
        );
        assert_eq!(ours_in(&root, "SessionEnd"), 1);
        // Refresh then keeps it in shape like the other hooks.
        let before = root.clone();
        refresh_hooks_in(root.as_object_mut().unwrap(), &exe).unwrap();
        assert_eq!(root, before);
    }

    #[test]
    fn refresh_upgrades_present_hooks_only() {
        let tmp = TempDir::new("refresh_hooks");
        let exe = tmp.path().join("callimachus");
        std::fs::write(&exe, "").unwrap();
        // A dead legacy SessionStart hook; no PreCompact (the user removed it); SubagentStop.
        let mut root: Value = serde_json::from_str(
            r#"{"hooks":{
                  "SessionStart":[{"hooks":[{"type":"command","command":"\"/gone/.local/bin/cal\" hook"}]}],
                  "SubagentStop":[{"hooks":[{"type":"command","command":"\"/gone/.local/bin/cal\" snapshot-hook"}]}]
                }}"#,
        )
        .unwrap();
        refresh_hooks_in(root.as_object_mut().unwrap(), &exe).unwrap();
        let session = our_group(&root, "SessionStart");
        assert_eq!(session["matcher"], "startup|clear|compact");
        assert_eq!(
            session["hooks"][0]["command"],
            hook_command(&exe, "hook").as_str()
        );
        assert_eq!(ours_in(&root, "PreCompact"), 0, "not re-added");
        assert_eq!(
            ours_in(&root, "SessionEnd"),
            0,
            "refresh alone adds nothing"
        );
        assert!(root["hooks"].get("SubagentStop").is_none());

        // A second refresh changes nothing.
        let before = root.clone();
        refresh_hooks_in(root.as_object_mut().unwrap(), &exe).unwrap();
        assert_eq!(root, before);

        // A hook aimed at another live Callimachus binary keeps its target.
        let dev = tmp.path().join("target-debug-callimachus");
        std::fs::write(&dev, "").unwrap();
        let mut dev_root = json!({});
        upsert_hook(
            dev_root.as_object_mut().unwrap(),
            &SESSION_START_HOOK,
            &format!("\"{}\" hook", dev.display()),
            &exe,
        )
        .unwrap();
        refresh_hooks_in(dev_root.as_object_mut().unwrap(), &exe).unwrap();
        assert_eq!(
            our_group(&dev_root, "SessionStart")["hooks"][0]["command"],
            hook_command(&dev, "hook").as_str()
        );
    }

    #[test]
    fn refresh_keeps_user_tuning_and_user_hooks() {
        let tmp = TempDir::new("refresh_tuning");
        let exe = tmp.path().join("callimachus");
        std::fs::write(&exe, "").unwrap();
        let tool = tmp.path().join("tool");
        std::fs::write(&tool, "").unwrap();
        let user_cmd = format!("\"{}\" hook", tool.display());
        // The user's own `"<path>" hook` comes first; ours (old unguarded form) was tuned: a
        // wider matcher, a longer timeout, plus an extra group with our command for `resume`.
        let mut root = json!({"hooks": {"SessionStart": [
            {"hooks": [{"type": "command", "command": user_cmd}]},
            {"matcher": "*", "hooks": [{"type": "command", "command": "\"/gone/.local/bin/cal\" hook", "timeout": 30}]},
            {"matcher": "resume", "hooks": [{"type": "command", "command": "\"/gone/.local/bin/cal\" hook"}]},
        ]}});
        refresh_hooks_in(root.as_object_mut().unwrap(), &exe).unwrap();
        let groups = root["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(groups.len(), 3, "nothing removed or added: {root}");
        assert_eq!(
            groups[0]["hooks"][0]["command"],
            user_cmd.as_str(),
            "user hook untouched"
        );
        assert!(groups[0].get("matcher").is_none());
        let ours = hook_command(&exe, "hook");
        assert_eq!(groups[1]["matcher"], "*");
        assert_eq!(groups[1]["hooks"][0]["timeout"], 30);
        assert_eq!(groups[1]["hooks"][0]["command"], ours.as_str());
        assert_eq!(groups[2]["matcher"], "resume");
        assert_eq!(
            groups[2]["hooks"][0]["timeout"], 10,
            "missing timeout added"
        );
        assert_eq!(groups[2]["hooks"][0]["command"], ours.as_str());

        // A group shared with a user hook gets no matcher (it would scope their hook too).
        let mut shared = json!({"hooks": {"SessionStart": [{"hooks": [
            {"type": "command", "command": "echo hi"},
            {"type": "command", "command": "\"/gone/.local/bin/cal\" hook"},
        ]}]}});
        refresh_hooks_in(shared.as_object_mut().unwrap(), &exe).unwrap();
        assert!(shared["hooks"]["SessionStart"][0].get("matcher").is_none());
        assert_eq!(
            shared["hooks"]["SessionStart"][0]["hooks"][1]["command"],
            ours.as_str()
        );
    }

    #[cfg(unix)] // Unix path semantics
    #[test]
    fn refresh_repoints_only_dead_app_registrations() {
        let tmp = TempDir::new("refresh_mcp");
        let exe = tmp.path().join("callimachus");
        std::fs::write(&exe, "").unwrap();
        let cfg = tmp.path().join(".claude.json");

        // Dead absolute path run with --mcp (an old AppImage mount): re-pointed.
        std::fs::write(
            &cfg,
            r#"{"mcpServers":{"callimachus":{"type":"stdio","command":"/tmp/.mount_abc/usr/bin/callimachus","args":["--mcp"],"env":{"K":"V"}}}}"#,
        )
        .unwrap();
        assert!(refresh_mcp_json(&cfg, &exe).unwrap());
        let back = read_json(&cfg);
        assert_eq!(
            back["mcpServers"]["callimachus"]["command"],
            exe.to_string_lossy().as_ref()
        );
        assert_eq!(back["mcpServers"]["callimachus"]["env"]["K"], "V");
        assert!(!refresh_mcp_json(&cfg, &exe).unwrap(), "idempotent");

        // The user's npx registration under our name is theirs.
        let npx =
            r#"{"mcpServers":{"callimachus":{"command":"npx","args":["-y","callimachus-mcp"]}}}"#;
        std::fs::write(&cfg, npx).unwrap();
        assert!(!refresh_mcp_json(&cfg, &exe).unwrap());
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), npx);

        // Never creates config.
        let none = tmp.path().join("absent.json");
        assert!(!refresh_mcp_json(&none, &exe).unwrap());
        assert!(!none.exists());

        // Codex TOML: same rule, comments kept.
        let toml = tmp.path().join("config.toml");
        std::fs::write(
            &toml,
            "# my codex\nmodel = \"o3\"\n\n[mcp_servers.callimachus]\ncommand = \"/gone/callimachus\"\nargs = [\"--mcp\"]\n",
        )
        .unwrap();
        assert!(refresh_mcp_toml(&toml, &exe).unwrap());
        let text = std::fs::read_to_string(&toml).unwrap();
        assert!(text.starts_with("# my codex\n"));
        assert!(mcp_registered_toml(&toml, &exe));
    }

    /// The deb, rpm and macOS .app used to ship `callimachus-mcp`; a client entry pointed at it
    /// by hand would go dead on upgrade and the server would vanish from that client.
    #[cfg(unix)] // Unix path semantics
    #[test]
    fn refresh_repoints_a_dead_standalone_server_with_mcp_args() {
        let tmp = TempDir::new("refresh_standalone");
        let exe = tmp.path().join("callimachus");
        std::fs::write(&exe, "").unwrap();
        let cfg = tmp.path().join(".claude.json");

        std::fs::write(
            &cfg,
            r#"{"mcpServers":{"callimachus":{"command":"/gone/usr/bin/callimachus-mcp","env":{"K":"V"}}}}"#,
        )
        .unwrap();
        assert!(refresh_mcp_json(&cfg, &exe).unwrap());
        let back = read_json(&cfg);
        let entry = &back["mcpServers"]["callimachus"];
        assert_eq!(entry["command"], exe.to_string_lossy().as_ref());
        assert_eq!(entry["args"], json!(["--mcp"]));
        assert_eq!(entry["env"]["K"], "V");
        assert!(!refresh_mcp_json(&cfg, &exe).unwrap(), "idempotent");

        // A dead standalone path with args of its own, or another dead binary run without
        // args, is not recognizably ours: left alone.
        for unknown in [
            r#"{"mcpServers":{"callimachus":{"command":"/gone/callimachus-mcp","args":["--db","x"]}}}"#,
            r#"{"mcpServers":{"callimachus":{"command":"/gone/other-server"}}}"#,
        ] {
            std::fs::write(&cfg, unknown).unwrap();
            assert!(!refresh_mcp_json(&cfg, &exe).unwrap(), "{unknown}");
            assert_eq!(std::fs::read_to_string(&cfg).unwrap(), unknown);
        }

        // A standalone binary that still exists (a cargo or Releases install) is the user's.
        let live = tmp.path().join("callimachus-mcp");
        std::fs::write(&live, "").unwrap();
        let kept =
            json!({"mcpServers": {"callimachus": {"command": live.to_string_lossy()}}}).to_string();
        std::fs::write(&cfg, &kept).unwrap();
        assert!(!refresh_mcp_json(&cfg, &exe).unwrap());

        // Codex TOML gains the args too.
        let toml = tmp.path().join("config.toml");
        std::fs::write(
            &toml,
            "[mcp_servers.callimachus]\ncommand = \"/Applications/Gone.app/Contents/MacOS/callimachus-mcp\"\n",
        )
        .unwrap();
        assert!(refresh_mcp_toml(&toml, &exe).unwrap());
        assert!(mcp_registered_toml(&toml, &exe));
        let back = std::fs::read_to_string(&toml)
            .unwrap()
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        assert_eq!(
            back["mcp_servers"]["callimachus"]["args"][0].as_str(),
            Some("--mcp")
        );
    }

    #[test]
    fn codex_toml_merge_preserves_others() {
        let tmp = TempDir::new("codex");
        let cfg = tmp.path().join("config.toml");
        std::fs::write(
            &cfg,
            "# keep me\nmodel = \"o3\"\n\n[mcp_servers.other]\ncommand = \"x\"\n",
        )
        .unwrap();
        register_mcp_toml(&cfg, Path::new("/app")).unwrap();
        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(text.contains("# keep me"));
        let back = text.parse::<toml_edit::DocumentMut>().unwrap();
        assert_eq!(back["model"].as_str(), Some("o3"));
        assert_eq!(back["mcp_servers"]["other"]["command"].as_str(), Some("x"));
        assert_eq!(
            back["mcp_servers"]["callimachus"]["command"].as_str(),
            Some("/app")
        );
        assert_eq!(
            back["mcp_servers"]["callimachus"]["args"][0].as_str(),
            Some("--mcp")
        );
        remove_mcp_toml(&cfg).unwrap();
        let back = std::fs::read_to_string(&cfg)
            .unwrap()
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        assert!(back["mcp_servers"].get("callimachus").is_none());
        assert_eq!(back["mcp_servers"]["other"]["command"].as_str(), Some("x"));

        // An inline-table `mcp_servers` is edited too, not refused (install) or skipped
        // (uninstall).
        std::fs::write(&cfg, "mcp_servers = { other = { command = \"x\" } }\n").unwrap();
        register_mcp_toml(&cfg, Path::new("/app")).unwrap();
        assert!(mcp_registered_toml(&cfg, Path::new("/app")));
        remove_mcp_toml(&cfg).unwrap();
        let back = std::fs::read_to_string(&cfg)
            .unwrap()
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        assert!(back["mcp_servers"].get("callimachus").is_none());
        assert_eq!(back["mcp_servers"]["other"]["command"].as_str(), Some("x"));
    }

    #[cfg(unix)] // Unix path semantics
    #[test]
    fn stable_exe_rules() {
        // Linux AppImage: the real file from $APPIMAGE wins over the temporary mount.
        let tmp = TempDir::new("appimage");
        let image = tmp.path().join("Callimachus.AppImage");
        std::fs::write(&image, "").unwrap();
        let mount = Path::new("/tmp/.mount_CallimXyZ/usr/bin/callimachus");
        assert_eq!(
            linux_stable_exe(mount, Some(image.as_path()), None).unwrap(),
            image
        );
        assert!(linux_stable_exe(mount, None, None).is_err());
        let deb = Path::new("/usr/bin/callimachus");
        assert_eq!(linux_stable_exe(deb, None, None).unwrap(), deb);
        // A deb started from another AppImage app's terminal inherits that app's APPIMAGE and
        // APPDIR; it must stay itself.
        let cursor = tmp.path().join("Cursor.AppImage");
        std::fs::write(&cursor, "").unwrap();
        let cursor_dir = Path::new("/tmp/.mount_CursorAb");
        assert_eq!(
            linux_stable_exe(deb, Some(cursor.as_path()), Some(cursor_dir)).unwrap(),
            deb
        );
        // --appimage-extract-and-run: no .mount_ dir, but inside $APPDIR.
        let extracted = Path::new("/tmp/appimage_extracted_1/usr/bin/callimachus");
        assert_eq!(
            linux_stable_exe(
                extracted,
                Some(image.as_path()),
                Some(Path::new("/tmp/appimage_extracted_1"))
            )
            .unwrap(),
            image
        );

        // macOS: translocated or DMG-run apps are temporary; an external-drive install is not.
        assert!(is_macos_ephemeral(Path::new(
            "/private/var/folders/x/T/AppTranslocation/1234/d/Callimachus.app/Contents/MacOS/callimachus"
        )));
        assert!(is_macos_ephemeral(Path::new(
            "/Volumes/Callimachus/Callimachus.app/Contents/MacOS/callimachus"
        )));
        assert!(!is_macos_ephemeral(Path::new(
            "/Volumes/SSD/Applications/Callimachus.app/Contents/MacOS/callimachus"
        )));
        assert!(!is_macos_ephemeral(Path::new(
            "/Applications/Callimachus.app/Contents/MacOS/callimachus"
        )));
        // At a volume root, only a read-only volume (a mounted DMG) is temporary.
        let at_root = Path::new("/Volumes/SSD/Callimachus.app/Contents/MacOS/callimachus");
        assert!(!is_macos_ephemeral_with(at_root, |_| false));
        assert!(is_macos_ephemeral_with(at_root, |v| v == Path::new("/Volumes/SSD")));
        // Deeper on a drive the probe isn't even asked.
        assert!(!is_macos_ephemeral_with(
            Path::new("/Volumes/SSD/Applications/Callimachus.app/Contents/MacOS/callimachus"),
            |_| true
        ));

        let table = "/dev/disk3s1s1 on / (apfs, sealed, local, read-only, journaled)\n\
                     /dev/disk5s1 on /Volumes/Callimachus 0.11.0 (hfs, local, nodev, nosuid, \
                     read-only, noowners, quarantine, mounted by me)\n\
                     /dev/disk6s2 on /Volumes/SSD (apfs, local, nodev, nosuid, journaled, \
                     noowners)\n";
        assert_eq!(
            mount_flags_read_only(table, Path::new("/Volumes/Callimachus 0.11.0")),
            Some(true)
        );
        assert_eq!(
            mount_flags_read_only(table, Path::new("/Volumes/SSD")),
            Some(false)
        );
        assert_eq!(
            mount_flags_read_only(table, Path::new("/Volumes/Gone")),
            None
        );
    }

    #[cfg(unix)] // Unix path semantics
    #[test]
    fn config_dir_overrides() {
        // The env-var and Settings dirs share the indexer's `~` expansion.
        let home = dirs::home_dir().expect("home dir");
        let expand = |s: &str| crate::indexer::claude::expand_tilde(PathBuf::from(s));
        assert_eq!(expand("/work/claude"), PathBuf::from("/work/claude"));
        assert_eq!(expand("~/.claude-work"), home.join(".claude-work"));
        assert_eq!(expand("~"), home);
        assert_eq!(expand("~nope/x"), PathBuf::from("~nope/x"));

        // .claude.json stays in $HOME unless the relocated dir already has its own.
        let tmp = TempDir::new("claude_json");
        let relocated = tmp.path().join("claude-work");
        std::fs::create_dir_all(&relocated).unwrap();
        let env = Some(relocated.clone().into_os_string());
        assert_eq!(
            claude_config_path_from(env.clone(), tmp.path()),
            tmp.path().join(".claude.json")
        );
        std::fs::write(relocated.join(".claude.json"), "{}").unwrap();
        assert_eq!(
            claude_config_path_from(env, tmp.path()),
            relocated.join(".claude.json")
        );
    }

    #[cfg(unix)] // Unix path semantics
    #[test]
    fn shell_path_output_is_parsed_past_rc_noise() {
        let out = "Welcome!\nlast login...\n__CALLIMACHUS_PATH__=/opt/homebrew/bin:/usr/bin:relative:/bin\n";
        assert_eq!(
            parse_shell_path(out).unwrap(),
            vec![
                PathBuf::from("/opt/homebrew/bin"),
                PathBuf::from("/usr/bin"),
                PathBuf::from("/bin")
            ]
        );
        assert!(parse_shell_path("no marker here").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn cal_slot_only_claims_our_links() {
        let tmp = TempDir::new("cal_slot");
        let exe = tmp.path().join("callimachus");
        std::fs::write(&exe, "").unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let link = bin.join("cal");

        assert_eq!(inspect_cal_slot(&link, &exe), CalSlot::Missing);
        link_cal_at(&link, &exe).unwrap();
        assert_eq!(
            inspect_cal_slot(&link, &exe),
            CalSlot::Ours {
                target: exe.clone(),
                live: true
            }
        );
        link_cal_at(&link, &exe).unwrap(); // idempotent

        // A dangling link to an old Callimachus path is ours and gets re-pointed.
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink("/gone/Callimachus.app/Contents/MacOS/callimachus", &link)
            .unwrap();
        assert!(matches!(
            inspect_cal_slot(&link, &exe),
            CalSlot::Ours { live: false, .. }
        ));
        link_cal_at(&link, &exe).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), exe);

        // Someone else's file or link is never replaced.
        std::fs::remove_file(&link).unwrap();
        std::fs::write(&link, "#!/bin/sh\necho other\n").unwrap();
        assert!(matches!(inspect_cal_slot(&link, &exe), CalSlot::Foreign(_)));
        assert!(link_cal_at(&link, &exe).is_err());
        assert_eq!(
            std::fs::read_to_string(&link).unwrap(),
            "#!/bin/sh\necho other\n"
        );
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink("/usr/bin/cal", &link).unwrap();
        assert!(matches!(inspect_cal_slot(&link, &exe), CalSlot::Foreign(_)));
        assert!(link_cal_at(&link, &exe).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn path_hint_explains_shadowed_or_missing_cal() {
        let tmp = TempDir::new("path_hint");
        let home = tmp.path().join("home");
        let local_bin = home.join(".local").join("bin");
        let system_bin = tmp.path().join("usr-bin");
        std::fs::create_dir_all(&local_bin).unwrap();
        std::fs::create_dir_all(&system_bin).unwrap();
        let exe = tmp.path().join("callimachus");
        std::fs::write(&exe, "").unwrap();
        let cal = local_bin.join("cal");
        link_cal_at(&cal, &exe).unwrap();
        std::fs::write(system_bin.join("cal"), "").unwrap(); // the calendar

        // Ours first: no hint.
        let ok = [local_bin.clone(), system_bin.clone()];
        assert_eq!(path_hint_for(&ok, &cal, &exe, Some(&home)), None);

        // Not on PATH at all: tell them how to add it.
        let missing = [system_bin.clone()];
        let hint = path_hint_for(&missing, &cal, &exe, Some(&home)).unwrap();
        assert!(
            hint.contains("~/.local/bin isn't on your shell's PATH"),
            "{hint}"
        );
        assert!(
            hint.contains("export PATH=\"$HOME/.local/bin:$PATH\""),
            "{hint}"
        );

        // On PATH but shadowed by an earlier dir.
        let shadowed = [system_bin.clone(), local_bin.clone()];
        let hint = path_hint_for(&shadowed, &cal, &exe, Some(&home)).unwrap();
        assert!(hint.contains("comes before ~/.local/bin"), "{hint}");
    }

    // ---- Codex and Gemini CLI memory hooks ----

    fn target_in(dir: &Path, id: &'static str) -> HookTarget {
        let (file, specs): (PathBuf, &'static [&'static HookSpec]) = match id {
            "codex" => (dir.join("hooks.json"), &CODEX_HOOKS),
            _ => (dir.join("settings.json"), &GEMINI_HOOKS),
        };
        HookTarget {
            id,
            label: if id == "codex" { "Codex" } else { "Gemini CLI" },
            dir: dir.to_path_buf(),
            file,
            specs,
        }
    }

    fn hook_entry<'a>(root: &'a Value, event: &str) -> &'a Value {
        &our_group(root, event)["hooks"][0]
    }

    /// Codex: hooks.json is created, the three hooks land with Codex's matcher and second
    /// timeouts (SessionEnd at its 3 s max), a reinstall adds no duplicate, the user's own
    /// hooks survive, and uninstall leaves only them.
    #[test]
    fn codex_hooks_install_idempotently_and_uninstall_cleanly() {
        let tmp = TempDir::new("codex-hooks");
        let t = target_in(tmp.path(), "codex");
        let exe = Path::new("/Applications/Callimachus.app/Contents/MacOS/callimachus");
        assert!(!agent_hooks_installed(&t, exe));
        for _ in 0..2 {
            install_hooks_into(&t.file, t.specs, exe).unwrap();
        }
        let root = read_json(&t.file);
        for event in ["SessionStart", "PreCompact", "SessionEnd"] {
            assert_eq!(ours_in(&root, event), 1, "{event}");
        }
        assert_eq!(
            our_group(&root, "SessionStart")["matcher"],
            "startup|clear|compact"
        );
        let start = hook_entry(&root, "SessionStart");
        assert_eq!(start["timeout"], 10);
        assert!(
            start["command"]
                .as_str()
                .unwrap()
                .ends_with("\" hook --for codex"),
            "{start}"
        );
        assert!(start.get("name").is_none());
        assert_eq!(hook_entry(&root, "SessionEnd")["timeout"], 3);
        assert!(hook_entry(&root, "PreCompact")["command"]
            .as_str()
            .unwrap()
            .ends_with("snapshot-hook --for codex"));
        assert!(agent_hooks_installed(&t, exe));
        // Claude's hook detection does not mistake a Codex hook for Claude's.
        assert_eq!(
            our_hook_subcommand(start["command"].as_str().unwrap()),
            Some("hook --for codex")
        );

        // The user's own hook in the same event survives install and uninstall.
        let mut with_user = read_json(&t.file);
        with_user["hooks"]["SessionStart"]
            .as_array_mut()
            .unwrap()
            .push(json!({"hooks": [{"type": "command", "command": "my-own-hook"}]}));
        std::fs::write(&t.file, serde_json::to_string(&with_user).unwrap()).unwrap();
        install_hooks_into(&t.file, t.specs, exe).unwrap();
        remove_hooks_from(&t.file, t.specs, exe).unwrap();
        let after = read_json(&t.file);
        assert_eq!(
            after,
            json!({"hooks": {"SessionStart": [
                {"hooks": [{"type": "command", "command": "my-own-hook"}]}
            ]}})
        );
        assert!(!agent_hooks_installed(&t, exe));
    }

    /// Gemini: merged into settings.json next to the MCP entry and other settings, with
    /// exact-string `*` matchers, millisecond timeouts and hook names.
    #[test]
    fn gemini_hooks_merge_into_settings_json() {
        let tmp = TempDir::new("gemini-hooks");
        let t = target_in(tmp.path(), "gemini");
        std::fs::write(
            &t.file,
            r#"{"theme":"dark","mcpServers":{"callimachus":{"command":"/x","args":["--mcp"]}}}"#,
        )
        .unwrap();
        let exe = Path::new("/opt/callimachus/callimachus");
        for _ in 0..2 {
            install_hooks_into(&t.file, t.specs, exe).unwrap();
        }
        let root = read_json(&t.file);
        assert_eq!(root["theme"], "dark");
        assert_eq!(root["mcpServers"]["callimachus"]["command"], "/x");
        assert_eq!(ours_in(&root, "SessionStart"), 1);
        assert_eq!(ours_in(&root, "SessionEnd"), 1);
        assert_eq!(ours_in(&root, "PreCompress"), 0);
        assert_eq!(our_group(&root, "SessionStart")["matcher"], "*");
        let start = hook_entry(&root, "SessionStart");
        assert_eq!(start["timeout"], 10_000);
        assert_eq!(start["name"], "callimachus-memory");
        assert_eq!(start["type"], "command");
        assert!(start["command"]
            .as_str()
            .unwrap()
            .ends_with("\" hook --for gemini"));
        assert!(hook_entry(&root, "SessionEnd")["command"]
            .as_str()
            .unwrap()
            .ends_with("session-end --for gemini"));

        remove_hooks_from(&t.file, t.specs, exe).unwrap();
        let after = read_json(&t.file);
        assert!(after.get("hooks").is_none(), "{after}");
        assert_eq!(after["mcpServers"]["callimachus"]["command"], "/x");
    }

    /// Startup refresh re-points a dead target in both files, keeps `--for`, the user's
    /// timeout and Gemini's millisecond unit, and never creates a file.
    #[test]
    fn agent_hook_refresh_repoints_and_keeps_tuning() {
        let tmp = TempDir::new("agent-hook-refresh");
        let dead = Path::new("/gone/Callimachus.app/Contents/MacOS/callimachus");
        let exe = Path::new("/Applications/Callimachus.app/Contents/MacOS/callimachus");
        for id in ["codex", "gemini"] {
            let dir = tmp.path().join(id);
            std::fs::create_dir_all(&dir).unwrap();
            let t = target_in(&dir, id);
            install_hooks_into(&t.file, t.specs, dead).unwrap();
            let mut root = read_json(&t.file);
            let start = root["hooks"]["SessionStart"][0]["hooks"][0]
                .as_object_mut()
                .unwrap();
            start.insert("timeout".into(), json!(7));
            // An entry an older build wrote without a timeout gets the agent's unit.
            root["hooks"]["SessionEnd"][0]["hooks"][0]
                .as_object_mut()
                .unwrap()
                .remove("timeout");
            std::fs::write(&t.file, serde_json::to_string(&root).unwrap()).unwrap();

            let changed = update_json_file(&t.file, false, |obj| {
                refresh_specs_in(obj, t.specs, exe);
                Ok(())
            })
            .unwrap();
            assert!(changed, "{id}");
            let root = read_json(&t.file);
            let start = hook_entry(&root, "SessionStart");
            assert_eq!(
                start["command"],
                hook_command(exe, &format!("hook --for {id}")),
                "{id}"
            );
            assert_eq!(start["timeout"], 7, "the user's timeout stays");
            let end_timeout = if id == "codex" { 3 } else { 5_000 };
            assert_eq!(hook_entry(&root, "SessionEnd")["timeout"], end_timeout);
            assert!(agent_hooks_installed(&t, exe));
        }
        // No file: refresh writes none.
        let t = target_in(&tmp.path().join("none"), "codex");
        assert!(!update_json_file(&t.file, false, |obj| {
            refresh_specs_in(obj, t.specs, exe);
            Ok(())
        })
        .unwrap());
        assert!(!t.file.exists());
    }

    /// Hooks turned off in the agent's own config, or an agent that is not set up, block
    /// the install with a reason; on by default otherwise.
    #[test]
    fn agent_hooks_report_blocked_states() {
        let tmp = TempDir::new("agent-hook-blocked");
        let codex = target_in(tmp.path(), "codex");
        assert_eq!(hooks_blocked(&codex), None, "no config.toml: hooks are on");
        std::fs::write(tmp.path().join("config.toml"), "model = \"o3\"\n").unwrap();
        assert_eq!(hooks_blocked(&codex), None);
        std::fs::write(tmp.path().join("config.toml"), "[features]\nhooks = true\n").unwrap();
        assert_eq!(hooks_blocked(&codex), None);
        for off in [
            "[features]\nhooks = false\n",
            "[features]\ncodex_hooks = false\n",
        ] {
            std::fs::write(tmp.path().join("config.toml"), off).unwrap();
            assert_eq!(
                hooks_blocked(&codex).as_deref(),
                Some(CODEX_HOOKS_OFF),
                "{off}"
            );
        }

        let gemini = target_in(tmp.path(), "gemini");
        std::fs::write(&gemini.file, r#"{"hooksConfig":{"enabled":true}}"#).unwrap();
        assert_eq!(hooks_blocked(&gemini), None);
        std::fs::write(&gemini.file, r#"{"hooksConfig":{"enabled":false}}"#).unwrap();
        assert_eq!(hooks_blocked(&gemini).as_deref(), Some(GEMINI_HOOKS_OFF));

        let missing = target_in(&tmp.path().join("not-here"), "gemini");
        assert!(hooks_blocked(&missing)
            .unwrap()
            .starts_with("Gemini CLI is not set up on this machine"));
    }

    /// Gemini CLI runs hooks through PowerShell on Windows, so its commands there use the
    /// call operator, and are still recognized (and re-pointed) like every other form.
    #[test]
    fn gemini_windows_commands_use_the_powershell_call_operator() {
        let exe = r"C:\Users\me\AppData\Local\Callimachus\callimachus.exe";
        let cmd = spec_command_for(exe, &GEMINI_SESSION_START_HOOK, "hook --for gemini", true);
        assert_eq!(cmd, format!("& \"{exe}\" hook --for gemini"));
        assert_eq!(our_hook_subcommand(&cmd), Some("hook --for gemini"));
        assert_eq!(hook_target(&cmd, false), Some(PathBuf::from(exe)));
        // Codex and Claude Code keep the plain quoted form; Unix is unchanged for Gemini.
        assert_eq!(
            spec_command_for(exe, &CODEX_SESSION_START_HOOK, "hook --for codex", true),
            format!("\"{exe}\" hook --for codex")
        );
        assert_eq!(
            spec_command_for(
                "/opt/c/callimachus",
                &GEMINI_SESSION_START_HOOK,
                "hook --for gemini",
                false
            ),
            hook_command_for("/opt/c/callimachus", "hook --for gemini", false)
        );
        assert!(GEMINI_HOOKS.iter().all(|s| s.powershell));
        assert!(!CODEX_HOOKS
            .iter()
            .chain(BASE_HOOKS.iter())
            .any(|s| s.powershell));
    }

    /// Codex hooks defined in config.toml block ours (hooks.json beside them makes Codex warn
    /// at every startup), but the trust state Codex writes there does not.
    #[test]
    fn codex_hooks_in_config_toml_block_the_install() {
        let tmp = TempDir::new("codex-toml-hooks");
        let codex = target_in(tmp.path(), "codex");
        let toml = tmp.path().join("config.toml");
        std::fs::write(&toml, "[hooks.state.\"x\"]\ntrusted_hash = \"abc\"\n").unwrap();
        assert_eq!(hooks_blocked(&codex), None);
        std::fs::write(
            &toml,
            "[[hooks.SessionStart]]\nmatcher = \"startup\"\n[[hooks.SessionStart.hooks]]\ntype = \"command\"\ncommand = \"echo hi\"\n",
        )
        .unwrap();
        assert_eq!(hooks_blocked(&codex).as_deref(), Some(CODEX_HOOKS_IN_TOML));
    }

    /// A Gemini hook listed by name in `hooksConfig.disabled` reports off and blocked, also
    /// in a settings.json with comments.
    #[test]
    fn gemini_hook_disabled_by_name_reports_off() {
        let tmp = TempDir::new("gemini-disabled");
        let t = target_in(tmp.path(), "gemini");
        let exe = Path::new("/opt/callimachus/callimachus");
        install_hooks_into(&t.file, t.specs, exe).unwrap();
        assert!(agent_hooks_installed(&t, exe));
        let mut root = read_json(&t.file);
        root["hooksConfig"] = json!({"disabled": ["callimachus-memory"]});
        let text = format!("// mine\n{}", serde_json::to_string_pretty(&root).unwrap());
        std::fs::write(&t.file, text).unwrap();
        assert!(agent_hooks_present(&t, exe), "still in the file");
        assert!(!agent_hooks_installed(&t, exe));
        assert_eq!(hooks_blocked(&t).as_deref(), Some(GEMINI_HOOK_DISABLED));
        // The install error names the comments instead of a bare parse error.
        let err = install_hooks_into(&t.file, t.specs, exe).unwrap_err();
        assert!(format!("{err:#}").contains("has comments"), "{err:#}");
    }

    /// Comments go, strings that look like comments stay.
    #[test]
    fn json_comments_are_stripped_outside_strings() {
        let text = "{\n  // a\n  \"url\": \"http://x/*y*/\", /* b\n c */ \"q\": \"\\\"//\"\n}";
        let v: Value = serde_json::from_str(&strip_json_comments(text)).unwrap();
        assert_eq!(v, json!({"url": "http://x/*y*/", "q": "\"//"}));
        assert_eq!(
            strip_json_comments(text).lines().count(),
            text.lines().count()
        );
    }

    /// Every `--for` subcommand is recognized as exactly itself, so refresh keeps it.
    #[test]
    fn for_agent_subcommands_are_recognized_exactly() {
        let exe = Path::new("/Applications/Callimachus.app/Contents/MacOS/callimachus");
        for sub in HOOK_SUBCOMMANDS {
            let cmd = hook_command_for(&exe.to_string_lossy(), sub, false);
            assert_eq!(our_hook_subcommand(&cmd), Some(sub), "{cmd}");
            assert!(is_our_hook(&cmd, exe));
            let win = hook_command_for(&exe.to_string_lossy(), sub, true);
            assert_eq!(our_hook_subcommand(&win), Some(sub), "{win}");
        }
    }

    /// The uninstaller's marker records the agents with hooks, and folds an earlier one in.
    #[test]
    fn marker_keeps_agent_hooks() {
        let mut later = RemovedIntegration::default();
        later.merge(RemovedIntegration {
            agent_hooks: vec!["gemini".into()],
            ..Default::default()
        });
        assert_eq!(later.agent_hooks, ["gemini"]);
        let old: RemovedIntegration = serde_json::from_str(r#"{"cal":true}"#).unwrap();
        assert!(old.agent_hooks.is_empty(), "an older marker still parses");
    }

    /// The shared-dir copy is removed only when it is one of ours, untouched.
    #[test]
    fn shared_skill_removal_spares_an_edited_copy() {
        let tmp = TempDir::new("shared-skill");
        let ours = tmp.path().join("ours.md");
        let theirs = tmp.path().join("theirs.md");
        write_skill_at(&ours, Path::new("/home/me/.local/bin/cal")).unwrap();
        std::fs::write(&theirs, "---\nname: recall\n---\nmy own\n").unwrap();
        remove_untouched_skill(&ours).unwrap();
        remove_untouched_skill(&theirs).unwrap();
        remove_untouched_skill(&tmp.path().join("missing.md")).unwrap();
        assert!(!ours.exists());
        assert!(theirs.exists());
    }
}
