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
pub const SKILL_MD: &str = include_str!("../resources/recall/SKILL.md");

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
    /// The `cal` CLI entry point exists and is ours (`~/.local/bin/cal`, or `cal.exe` on
    /// Windows). Powers the `cal` CLI and the VS Code extension.
    pub cal_installed: bool,
    /// The app runs from a permanent location, so it can be registered with agents.
    pub exe_stable: bool,
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
    let hook_installed = settings
        .as_ref()
        .is_some_and(|v| event_has_our_hook(v, SESSION_START_HOOK.event, &exe));
    let proactive_recall_installed = settings
        .as_ref()
        .is_some_and(|v| event_has_our_hook(v, PROACTIVE_RECALL_HOOK.event, &exe));

    let path_hint = path_hint(&cal, &exe, wait_for_path);
    let cal_problem = if cal.installed {
        None
    } else {
        cal_error.or(cal.problem)
    };

    IntegrationStatus {
        skill_installed,
        skill_outdated,
        mcp_registered,
        hook_installed,
        proactive_recall_installed,
        cal_installed: cal.installed,
        exe_stable: exe_problem.is_none(),
        exe_problem,
        cal_problem,
        path_hint,
        skill_path: skill.map(|p| p.display().to_string()).unwrap_or_default(),
        config_path: config.map(|p| p.display().to_string()).unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------------------------
// Install / uninstall / refresh
// ---------------------------------------------------------------------------------------------

/// Install (or refresh) the skill, the MCP registration, the `cal` CLI and the Claude Code hooks
/// (SessionStart injects each repo's memory; PreCompact snapshots the session before
/// compaction). Idempotent.
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
    let cal = cal_state(&exe);
    write_skill(&cli_entrypoint(&exe, &cal))?;
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

/// Remove the skill file, the Claude Code MCP registration, every Callimachus hook (including
/// ones older versions installed), and our `cal` link(s). Leaves the rest of each config intact
/// and never removes a `cal` that isn't ours. Tries every step, then reports any failures.
pub fn uninstall() -> Result<()> {
    let exe = current_stable_exe();
    let mut errors = Vec::new();
    let mut attempt = |what: &str, result: Result<()>| {
        if let Err(e) = result {
            errors.push(format!("{what}: {e:#}"));
        }
    };
    attempt("skill", skill_path().and_then(|p| remove_if_exists(&p)));
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
    /// Base Claude Code hook events that held our hook (`SessionStart`, `PreCompact`).
    claude_hooks: Vec<String>,
    /// The opt-in UserPromptSubmit hook.
    proactive_recall: bool,
    /// `mcpServers.callimachus` in Claude Code's config.
    claude_mcp: bool,
    /// Ids (see [`agent_targets`]) of the other agents whose config held our MCP entry.
    agents: Vec<String>,
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
        self.proactive_recall |= earlier.proactive_recall;
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
    RemovedIntegration {
        claude_hooks: BASE_HOOKS
            .iter()
            .filter(|spec| has_hook(spec.event))
            .map(|spec| spec.event.to_string())
            .collect(),
        proactive_recall: has_hook(PROACTIVE_RECALL_HOOK.event),
        claude_mcp: claude_config_path().is_ok_and(|c| has_mcp_json(&c)),
        agents,
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
    if let Err(e) = uninstall() {
        errors.push(format!("{e:#}"));
    }
    if let Err(e) = uninstall_agents() {
        errors.push(format!("other agents: {e:#}"));
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
    if removed.skill {
        attempt(
            "skill",
            write_skill(&cli_entrypoint(&exe, &cal_state(&exe))),
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
    record(
        "Claude Code MCP entry",
        claude_config_path().and_then(|c| refresh_mcp_json(&c, &exe)),
    );
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
    record("cal", refresh_cal(&exe));
    record("skill", refresh_skill(&exe));

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

fn write_skill(cli: &Path) -> Result<()> {
    let path = skill_path()?;
    let rendered = render_skill(cli);
    if read_config(&path)?.as_deref() == Some(rendered.as_str()) {
        return Ok(());
    }
    write_atomic(&path, rendered.as_bytes())
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
/// Settings.
fn refresh_skill(exe: &Path) -> Result<bool> {
    let path = skill_path()?;
    let Some(existing) = read_config(&path)? else {
        return Ok(false);
    };
    let cli = cli_entrypoint(exe, &cal_state(exe));
    if existing == render_skill(&cli) || !is_untouched_skill(&existing) {
        return Ok(false);
    }
    write_skill(&cli)?;
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
}

/// Injects the repo's memory. Skips `resume`: the earlier injection is still in the resumed
/// transcript, so it would only duplicate. `clear` and `compact` need it again, because the
/// context was dropped or summarized away.
const SESSION_START_HOOK: HookSpec = HookSpec {
    event: "SessionStart",
    subcommand: "hook",
    matcher: Some("startup|clear|compact"),
    timeout_secs: 10,
};

/// Snapshots the live session before its context is compacted.
const PRE_COMPACT_HOOK: HookSpec = HookSpec {
    event: "PreCompact",
    subcommand: "snapshot-hook",
    matcher: None,
    timeout_secs: 10,
};

/// Opt-in: injects "you may have solved this before" notes. Toggled by `set_proactive_recall`.
const PROACTIVE_RECALL_HOOK: HookSpec = HookSpec {
    event: "UserPromptSubmit",
    subcommand: "recall-now",
    matcher: None,
    timeout_secs: 15,
};

/// What `install()` sets up. Proactive recall stays a separate opt-in because it reads every
/// prompt.
const BASE_HOOKS: [&HookSpec; 2] = [&SESSION_START_HOOK, &PRE_COMPACT_HOOK];

/// Events older versions installed into and this one doesn't. Our hooks there are stripped on
/// install and refresh.
const LEGACY_HOOK_EVENTS: [&str; 1] = ["SubagentStop"];

/// Every event that can hold a Callimachus hook, current or legacy (what uninstall sweeps).
const ALL_HOOK_EVENTS: [&str; 4] = [
    "SessionStart",
    "PreCompact",
    "UserPromptSubmit",
    "SubagentStop",
];

/// The command for one of our hooks. It runs the app binary itself with a `cal` subcommand
/// (main.rs dispatches on it), so hooks don't depend on the `cal` link.
fn hook_command(exe: &Path, subcommand: &str) -> String {
    hook_command_for(&exe.to_string_lossy(), subcommand, cfg!(windows))
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

/// The subcommands our hooks run.
const HOOK_SUBCOMMANDS: [&str; 3] = ["hook", "snapshot-hook", "recall-now"];

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
    group.insert(
        "hooks".into(),
        json!([{ "type": "command", "command": command, "timeout": spec.timeout_secs }]),
    );
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

/// The base hooks in their current shape, plus the proactive-recall hook re-shaped if the user
/// already opted in. Retired hooks are stripped.
fn apply_base_hooks(obj: &mut Map<String, Value>, exe: &Path) -> Result<()> {
    apply_hooks(obj, &BASE_HOOKS, exe)?;
    if our_hook_command(obj, PROACTIVE_RECALL_HOOK.event, exe).is_some() {
        let command = hook_command(exe, PROACTIVE_RECALL_HOOK.subcommand);
        upsert_hook(obj, &PROACTIVE_RECALL_HOOK, &command, exe)?;
    }
    Ok(())
}

/// `specs` in their current shape, aimed at `exe`; retired hooks are stripped.
fn apply_hooks(obj: &mut Map<String, Value>, specs: &[&HookSpec], exe: &Path) -> Result<()> {
    for spec in specs {
        upsert_hook(obj, spec, &hook_command(exe, spec.subcommand), exe)?;
    }
    for event in LEGACY_HOOK_EVENTS {
        strip_our_hooks(obj, event, exe);
    }
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
    if let Some(events) = obj.get_mut("hooks").and_then(Value::as_object_mut) {
        for spec in [
            &SESSION_START_HOOK,
            &PRE_COMPACT_HOOK,
            &PROACTIVE_RECALL_HOOK,
        ] {
            let Some(groups) = events.get_mut(spec.event).and_then(Value::as_array_mut) else {
                continue;
            };
            for group in groups.iter_mut().filter_map(Value::as_object_mut) {
                refresh_group(group, spec, exe);
            }
        }
    }
    for event in LEGACY_HOOK_EVENTS {
        strip_our_hooks(obj, event, exe);
    }
    Ok(())
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
        let command = hook_command(&refresh_target(cmd, exe), sub);
        hook.insert("command".into(), json!(command));
        hook.entry("timeout").or_insert(json!(spec.timeout_secs));
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
    let value: Value = serde_json::from_str(text)
        .with_context(|| format!("{} is not valid JSON; not modifying it", path.display()))?;
    if !value.is_object() {
        bail!("{} is not a JSON object; not modifying it", path.display());
    }
    Ok(value)
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

/// An entry this app registered (an absolute app path run with exactly `--mcp`) whose binary no
/// longer exists. Anything else under our name, such as `npx -y callimachus-mcp` or a path that
/// still exists, is the user's choice and is left alone.
fn is_dead_app_registration<'a>(
    command: Option<&str>,
    mut args: impl Iterator<Item = Option<&'a str>>,
) -> bool {
    let is_mcp_args = args.next() == Some(Some("--mcp")) && args.next().is_none();
    command.is_some_and(|c| {
        let p = Path::new(c);
        p.is_absolute() && !p.exists()
    }) && is_mcp_args
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
        let dead = is_dead_app_registration(
            entry.get("command").and_then(Value::as_str),
            args.into_iter(),
        );
        if dead {
            entry.insert("command".into(), json!(exe.to_string_lossy()));
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
        let dead = is_dead_app_registration(
            entry.get("command").and_then(|c| c.as_str()),
            args.into_iter(),
        );
        if dead {
            entry.insert(
                "command",
                toml_edit::value(exe.to_string_lossy().to_string()),
            );
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
            Ok(target) if target == exe || mentions_callimachus(&target) => {
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
}
