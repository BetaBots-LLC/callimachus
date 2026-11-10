//! `cal doctor` and the app's Copy diagnostics: one read-only look at everything that decides
//! whether Callimachus works on this machine, in a form a user can paste into a public issue.
//!
//! Every hook exits 0 silently on any error, the MCP server refuses a schema it does not
//! know, and the indexers drop what they cannot read; each is right on its own and together
//! they made "it stopped remembering" impossible to diagnose from a bug report. This collects
//! the answers: the index and its schema against this binary, who holds the locks, the hooks
//! and MCP entries each agent runs and whether their binaries exist, the model, the vector
//! table, Claude Code's retention, the hooks that gave up in the last week
//! (`ledger::SkipClass`) and the parse canary (`indexer::parse_health`).
//!
//! Read-only: the index is opened read-only (or not at all), locks are probed with
//! `locks::holder` and never taken, config files are read and never written. The output names
//! paths, versions and counts only, with the home folder shown as `~` and every string
//! through `context::redact_secrets`; no project text, prompt or file contents.

use crate::indexer::parse_health::{self, Drift, LastPass, PassSums};
use crate::{
    backup, cleanup, context, db, embed, indexer, integration, jobs, knowledge, ledger, locks,
};
use rusqlite::Connection;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

/// Days of hook skips reported.
pub const SKIP_DAYS: u32 = 7;

/// What `cal doctor` adds on request.
#[derive(Debug, Default, Clone, Copy)]
pub struct Options {
    /// Run `PRAGMA quick_check` (reads the whole file: about 66 s on a 5.3 GB index).
    pub quick_check: bool,
}

/// Everything `cal doctor` reports.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Doctor {
    /// This binary's Callimachus version.
    pub version: String,
    /// `<os> <arch>`, e.g. `macos aarch64`.
    pub platform: String,
    pub index: IndexState,
    /// The cross-process locks next to the index (`locks`), probed, never taken.
    pub locks: Vec<LockState>,
    /// Each agent's Callimachus hooks and MCP entry.
    pub agents: Vec<integration::AgentSetup>,
    /// The `cal` command on PATH (`~/.local/bin/cal`, Unix), when there is one.
    pub cal_link: Option<CalLink>,
    /// The embedding model is on disk (without it semantic search falls back to keywords).
    pub model_cached: bool,
    pub vectors: Option<VectorState>,
    /// The index's snapshots (`backup`) and the last weekly integrity check.
    pub snapshots: SnapshotState,
    pub claude_retention: Option<ClaudeRetention>,
    pub hook_skips: HookSkips,
    /// The parse canary per source (`indexer::parse_health`).
    pub parse_health: Vec<SourceHealth>,
    /// What looks wrong, in plain words.
    pub problems: Vec<String>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct IndexState {
    pub path: String,
    pub exists: bool,
    /// `index.db` plus its `-wal`, in bytes.
    pub bytes: Option<u64>,
    /// Free bytes on the index's volume.
    pub free_bytes: Option<u64>,
    /// The schema version in the file (`PRAGMA user_version`).
    pub schema: Option<i64>,
    /// The schema version this binary writes.
    pub binary_schema: i64,
    /// `current`, `behind` (this binary would migrate it; hooks and the MCP server will not),
    /// `ahead` (a newer Callimachus upgraded it) or `unknown`.
    pub schema_state: String,
    pub threads: Option<i64>,
    /// The newest session activity in the index (unix seconds): how current the index is,
    /// whoever keeps it so.
    pub newest_thread_at: Option<i64>,
    /// When the last full indexing pass finished (unix seconds): the app's Reindex or
    /// onboarding, or `cal index`. None while only the app's watcher has kept the index
    /// current, which stamps no pass.
    pub last_pass_at: Option<i64>,
    /// With `--quick-check`: `ok`, or SQLite's first findings.
    pub quick_check: Option<String>,
    /// Why the index could not be opened, when it could not.
    pub open_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LockState {
    /// `gui`, `watch`, `distill`, `index`, `embed` or `rewrite`.
    pub name: String,
    pub held: bool,
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CalLink {
    pub path: String,
    /// Where the link points, when it is a link.
    pub target: Option<String>,
    /// It resolves to an existing file.
    pub live: bool,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VectorState {
    /// `vec_chunks` has its 0036 shape (`embed::vec_partitioned`).
    pub partitioned: bool,
    /// The one-time rebuild (or a compaction) is still due (`cleanup::vec_rebuild_due`).
    pub rebuild_due: Option<bool>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotState {
    /// Automatic snapshots are taken here: free file clones (macOS), on a disk the app has not
    /// found unable to clone. Elsewhere only `cal backup` makes them.
    pub automatic: bool,
    /// Snapshot folders next to the index.
    pub count: usize,
    /// Of those, automatic snapshots of another index than this one (an earlier index, before
    /// a fresh start): never removed automatically. 0 when the index could not be read.
    pub other_index: usize,
    /// When the newest was taken (unix seconds).
    pub newest_at: Option<i64>,
    /// Why the newest was taken: `pre-migration`, `weekly` or `manual`.
    pub newest_reason: Option<String>,
    /// What the snapshots cost on disk together, where the system reports it.
    pub bytes_on_disk: Option<u64>,
    /// When the app last ran its weekly integrity check (unix seconds). Null also when a
    /// failed check's result was carried across a Shrink index or Reclaim space rewrite.
    pub last_check_at: Option<i64>,
    /// Whether that check passed. A weekly snapshot is taken only after one that did.
    pub last_check_ok: Option<bool>,
    /// Damaged indexes a restore moved aside that are still next to the index.
    pub damaged_copies: usize,
    /// Their size with their `-wal` and `-shm`, in bytes.
    pub damaged_bytes: u64,
    /// Copies of a snapshot that an interrupted restore staged and left next to the index.
    pub staged_copies: usize,
    /// Their size with their `-wal` and `-shm`, in bytes.
    pub staged_bytes: u64,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeRetention {
    /// Claude Code's `cleanupPeriodDays`, None when unset.
    pub cleanup_period_days: Option<i64>,
    /// What Claude Code applies (its 30-day default when unset).
    pub effective_days: i64,
    /// Indexed Claude Code sessions whose transcript is gone: the index is their only copy.
    pub preserved_sessions: i64,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HookSkips {
    pub days: u32,
    pub total: u64,
    /// By class (`schema_behind`, `busy`, ...).
    pub by_class: BTreeMap<String, u64>,
    /// By class, hook and agent, as the ledger records them (`busy hook codex`).
    pub by_hook: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SourceHealth {
    pub source: String,
    /// The latest pass that was a full one or at least 50 records.
    pub last: Option<LastPass>,
    /// The sums of the judged passes the warning compares against.
    pub rolling: PassSums,
    /// A format change found (`dismissed` says whether the user dismissed its warning).
    pub drift: Option<Drift>,
}

/// The lock files `cal doctor` probes, with their names in the output.
const LOCKS: [(&str, &str); 6] = [
    ("gui", locks::GUI_LOCK),
    ("watch", locks::WATCH_LOCK),
    ("distill", locks::DISTILL_LOCK),
    ("index", locks::INDEX_LOCK),
    ("embed", locks::EMBED_LOCK),
    ("rewrite", locks::REWRITE_LOCK),
];

/// Collect the report for the index at `index`, read through `conn` when one is open (the
/// app's pooled reader); otherwise `cal doctor` opens it read-only here, if it exists.
pub fn collect(index: &Path, conn: Option<&Connection>, opts: Options) -> Doctor {
    let own;
    let mut open_error = None;
    let conn = match conn {
        Some(c) => Some(c),
        None if index.exists() => match db::open_readonly(index) {
            Ok(c) => {
                own = c;
                Some(&own)
            }
            Err(e) => {
                open_error = Some(scrub(&format!("{e:#}")));
                None
            }
        },
        None => None,
    };
    let now = chrono::Utc::now().timestamp();
    let mut doctor = Doctor {
        version: env!("CALLIMACHUS_VERSION").to_string(),
        platform: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        index: index_state(index, conn, opts, open_error),
        locks: LOCKS
            .iter()
            .map(|(name, file)| {
                let holder = locks::holder(index, file);
                LockState {
                    name: name.to_string(),
                    held: holder.is_some(),
                    pid: holder.flatten(),
                }
            })
            .collect(),
        agents: integration::installed_setup()
            .into_iter()
            .map(scrub_setup)
            .collect(),
        cal_link: cal_link(),
        // Looks in every root and creates none (`cal doctor` changes nothing).
        model_cached: embed::model_is_cached(),
        vectors: conn.and_then(vector_state),
        snapshots: snapshot_state(index, conn),
        claude_retention: conn.map(claude_retention),
        // The skips file is a plain file next to the index: read even when the index cannot
        // be opened, which is when its lines explain the most.
        hook_skips: hook_skips(conn, ledger::skips_path_for(index).as_deref(), now),
        parse_health: conn
            .map(|c| {
                parse_health::all(c)
                    .into_iter()
                    .map(|(source, h)| {
                        scrub_health(SourceHealth {
                            rolling: h.rolling(),
                            source,
                            last: h.last,
                            drift: h.drift,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        problems: Vec::new(),
    };
    doctor.problems = problems(&doctor).iter().map(|p| scrub(p)).collect();
    doctor
}

fn index_state(
    index: &Path,
    conn: Option<&Connection>,
    opts: Options,
    open_error: Option<String>,
) -> IndexState {
    let size = |p: &Path| std::fs::metadata(p).ok().map(|m| m.len());
    let wal = index.with_file_name(format!(
        "{}-wal",
        index
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default()
    ));
    let bytes = size(index).map(|b| b + size(&wal).unwrap_or(0));
    let binary_schema = Connection::open_in_memory()
        .ok()
        .and_then(|c| db::schema_pending(&c).ok())
        .map_or(0, i64::from);
    let schema: Option<i64> =
        conn.and_then(|c| c.query_row("PRAGMA user_version", [], |r| r.get(0)).ok());
    let schema_state = match schema {
        Some(v) if v == binary_schema => "current",
        Some(v) if v < binary_schema => "behind",
        Some(_) => "ahead",
        None => "unknown",
    };
    IndexState {
        path: scrub(&index.display().to_string()),
        exists: index.exists(),
        bytes,
        free_bytes: index.parent().and_then(cleanup::free_bytes),
        schema,
        binary_schema,
        schema_state: schema_state.into(),
        threads: conn.and_then(|c| {
            c.query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0))
                .ok()
        }),
        // `idx_threads_updated` answers this from its last entry.
        newest_thread_at: conn.and_then(|c| {
            c.query_row("SELECT MAX(updated_at) FROM threads", [], |r| r.get(0))
                .ok()
                .flatten()
        }),
        last_pass_at: conn.and_then(|c| {
            knowledge::app_config::get(c, jobs::LAST_PASS_AT)
                .ok()
                .flatten()
                .and_then(|v| v.trim().parse().ok())
        }),
        quick_check: conn.filter(|_| opts.quick_check).map(quick_check),
        open_error,
    }
}

/// `PRAGMA quick_check`'s verdict: `ok`, or its first few findings.
fn quick_check(conn: &Connection) -> String {
    let rows = conn.prepare("PRAGMA quick_check(5)").and_then(|mut stmt| {
        stmt.query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()
    });
    match rows {
        Ok(rows) => scrub(&rows.join("; ")),
        Err(e) => scrub(&format!("could not run: {e}")),
    }
}

fn vector_state(conn: &Connection) -> Option<VectorState> {
    db::has_table(conn, "vec_chunks").then(|| VectorState {
        partitioned: embed::vec_partitioned(conn),
        rebuild_due: cleanup::vec_rebuild_due(conn).ok(),
    })
}

/// The snapshots next to the index and the app's last weekly integrity check (`backup`), from
/// a directory listing and one `app_config` row: nothing here opens or changes a snapshot.
fn snapshot_state(index: &Path, conn: Option<&Connection>) -> SnapshotState {
    let status = backup::status(index, conn);
    let newest = status.snapshots.first();
    let (damaged_copies, damaged_bytes) = backup::kept_damaged(index);
    let (staged_copies, staged_bytes) = backup::staged_leftovers(index);
    SnapshotState {
        automatic: status.automatic,
        count: status.snapshots.len(),
        other_index: status.snapshots.iter().filter(|s| s.other_index).count(),
        newest_at: newest.map(|s| s.taken_at),
        newest_reason: newest.map(|s| s.reason.as_str().to_string()),
        bytes_on_disk: status.bytes_on_disk,
        // 0 is a failed result carried across a rewrite: no date, not 1970.
        last_check_at: status.last_check.map(|c| c.at).filter(|&at| at > 0),
        last_check_ok: status.last_check.map(|c| c.ok),
        damaged_copies,
        damaged_bytes,
        staged_copies,
        staged_bytes,
    }
}

fn claude_retention(conn: &Connection) -> ClaudeRetention {
    // The Settings override of Claude's config dir, as the indexer resolves it.
    indexer::claude::load_config_dir_setting(conn);
    let days = integration::claude_cleanup_period_days().unwrap_or(None);
    ClaudeRetention {
        cleanup_period_days: days,
        effective_days: days.unwrap_or(integration::CLAUDE_DEFAULT_CLEANUP_DAYS),
        // One stat per indexed session (~21k on the author's index).
        preserved_sessions: indexer::claude::projects_root()
            .map(|root| ledger::preserved_claude_threads(conn, &root).unwrap_or(0))
            .unwrap_or(0),
    }
}

/// The hook skips of the last [`SKIP_DAYS`] days: the days a rollup folded into the index
/// (when it is open) plus the lines of the skips file `skips_file` not rolled up yet.
fn hook_skips(conn: Option<&Connection>, skips_file: Option<&Path>, now: i64) -> HookSkips {
    let by_hook = ledger::skips(conn, skips_file, now, i64::from(SKIP_DAYS)).unwrap_or_default();
    let mut by_class = BTreeMap::new();
    for (detail, n) in &by_hook {
        let class = detail.split(' ').next().unwrap_or_default();
        *by_class.entry(class.to_string()).or_insert(0) += n;
    }
    HookSkips {
        days: SKIP_DAYS,
        total: by_hook.values().sum(),
        by_class,
        by_hook,
    }
}

/// The `cal` entry point on Unix (`~/.local/bin/cal`): where it points and whether it resolves.
fn cal_link() -> Option<CalLink> {
    if cfg!(windows) {
        return None;
    }
    let path = dirs::home_dir()?.join(".local").join("bin").join("cal");
    let meta = std::fs::symlink_metadata(&path).ok()?;
    let target = meta
        .file_type()
        .is_symlink()
        .then(|| std::fs::read_link(&path).ok())
        .flatten()
        .map(|t| scrub(&t.display().to_string()));
    Some(CalLink {
        path: scrub(&path.display().to_string()),
        target,
        live: path.exists(),
    })
}

/// Every string the agents' config files supplied, as it may leave the machine.
fn scrub_setup(mut s: integration::AgentSetup) -> integration::AgentSetup {
    for h in &mut s.hooks {
        h.event = scrub(&h.event);
        h.subcommand = scrub(&h.subcommand);
        h.exe = scrub(&h.exe);
    }
    if let Some(m) = s.mcp.as_mut() {
        m.command = scrub(&m.command);
        m.version = m.version.as_deref().map(scrub);
    }
    s
}

/// The type names a source's files supplied, as they may leave the machine. The indexers
/// keep only a plain character set (`common::kind_name`), which a token can still match.
fn scrub_health(mut s: SourceHealth) -> SourceHealth {
    if let Some(last) = s.last.as_mut() {
        let kinds = std::mem::take(&mut last.stats.unknown_kinds);
        for (name, n) in kinds {
            *last.stats.unknown_kinds.entry(scrub(&name)).or_insert(0) += n;
        }
    }
    if let Some(d) = s.drift.as_mut() {
        d.kinds = d.kinds.iter().map(|k| scrub(k)).collect();
    }
    s
}

/// A path or message as it may leave the machine: the home folder as `~`, secrets redacted,
/// controls stripped.
pub fn scrub(text: &str) -> String {
    let homed = match dirs::home_dir().map(|h| h.display().to_string()) {
        Some(home) if !home.is_empty() && home != "/" => home_as_tilde(text, &home),
        _ => text.to_string(),
    };
    let stripped = context::strip_controls(&homed).into_owned();
    context::redact_secrets(&stripped).into_owned()
}

/// Every `home` in `text` that starts a path (followed by a separator or the end) as `~`.
fn home_as_tilde(text: &str, home: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(home) {
        let after = &rest[at + home.len()..];
        let ends_path = after.is_empty() || after.starts_with(['/', '\\', '"', '\'', ' ', ';']);
        out.push_str(&rest[..at]);
        out.push_str(if ends_path { "~" } else { home });
        rest = after;
    }
    out.push_str(rest);
    out
}

/// What looks wrong, most important first, in words a user can act on.
fn problems(d: &Doctor) -> Vec<String> {
    let mut out = Vec::new();
    let i = &d.index;
    if !i.exists {
        out.push(
            "No index yet: open the Callimachus app once (or run `cal index` on a machine without it)."
                .to_string(),
        );
    } else if let Some(e) = &i.open_error {
        out.push(format!("The index cannot be opened: {e}"));
    }
    match i.schema_state.as_str() {
        "behind" => out.push(format!(
            "The index is at schema {} and this Callimachus writes {}: hooks and the MCP server \
             stay silent until the app (or `cal index` without the app) upgrades it.",
            i.schema.unwrap_or(0),
            i.binary_schema
        )),
        "ahead" => out.push(format!(
            "A newer Callimachus upgraded the index to schema {} (this one writes {}): update \
             this binary.",
            i.schema.unwrap_or(0),
            i.binary_schema
        )),
        _ => {}
    }
    if let Some(q) = i.quick_check.as_deref().filter(|q| *q != "ok") {
        out.push(format!("PRAGMA quick_check found problems: {q}"));
    }
    if let (Some(free), Some(bytes)) = (i.free_bytes, i.bytes) {
        if free < bytes {
            out.push(format!(
                "Less free disk ({}) than the index's size ({}) on its volume{}.",
                human_bytes(free),
                human_bytes(bytes),
                if d.snapshots.automatic {
                    ": no automatic snapshot is added while that lasts (a weekly one still \
                     replaces the last weekly one, and nothing is deleted to make room)"
                } else {
                    ""
                }
            ));
        }
    }
    let snaps = &d.snapshots;
    if snaps.last_check_ok == Some(false) {
        out.push(format!(
            "{} found problems in the index file: no new snapshot is taken until a check passes, \
             and the existing ones are kept. `cal doctor --quick-check` shows what SQLite \
             reports.",
            match snaps.last_check_at {
                Some(at) => format!("The app's weekly integrity check ({})", when(Some(at))),
                None => "An earlier weekly integrity check of the app".to_string(),
            }
        ));
    }
    if snaps.damaged_copies > 0 {
        let one = snaps.damaged_copies == 1;
        // The size of the files; a snapshot may still share some of those blocks.
        out.push(format!(
            "{} damaged index cop{} a restore kept {} still next to the index ({}, the \
             `.damaged-<date>` files): delete them once the restored index looks right.",
            snaps.damaged_copies,
            if one { "y" } else { "ies" },
            if one { "is" } else { "are" },
            human_bytes(snaps.damaged_bytes)
        ));
    }
    if snaps.staged_copies > 0 {
        let one = snaps.staged_copies == 1;
        out.push(format!(
            "{} cop{} of a snapshot that an interrupted restore staged {} still next to the index \
             ({}, the `.restoring-<date>` files): the next restore removes them, and the app does \
             once they are a day old.",
            snaps.staged_copies,
            if one { "y" } else { "ies" },
            if one { "is" } else { "are" },
            human_bytes(snaps.staged_bytes)
        ));
    }
    for a in &d.agents {
        for h in a.hooks.iter().filter(|h| !h.exe_exists) {
            out.push(format!(
                "{}'s {} hook runs {}, which does not exist: reinstall it from Settings > Agents.",
                a.agent, h.event, h.exe
            ));
        }
        if let Some(m) = a.mcp.as_ref().filter(|m| m.exe_exists == Some(false)) {
            out.push(format!(
                "{}'s MCP entry runs {}, which does not exist.",
                a.agent, m.command
            ));
        }
    }
    if d.hook_skips.total > 0 {
        let classes: Vec<String> = d
            .hook_skips
            .by_class
            .iter()
            .map(|(c, n)| format!("{c} {n}"))
            .collect();
        out.push(format!(
            "Hooks gave up {} time(s) in the last {} days ({}).",
            d.hook_skips.total,
            d.hook_skips.days,
            classes.join(", ")
        ));
    }
    for s in &d.parse_health {
        if let Some(drift) = s.drift.as_ref().filter(|d| !d.dismissed) {
            let mut seen = Vec::new();
            if drift.by_share {
                seen.push(format!(
                    "{:.0}% of records unknown or malformed in a pass (usually {:.0}%)",
                    drift.share * 100.0,
                    drift.baseline * 100.0
                ));
            }
            if drift.by_zero_files {
                seen.push(format!(
                    "{} of {} files in a pass held records but no message",
                    drift.zero_files, drift.files
                ));
            }
            if !drift.kinds.is_empty() {
                seen.push(format!("unknown types: {}", drift.kinds.join(", ")));
            }
            out.push(format!(
                "{}'s transcripts may have changed format: {}.",
                s.source,
                seen.join("; ")
            ));
        }
    }
    if !d.model_cached {
        out.push(
            "The embedding model is not on disk: semantic search falls back to keywords."
                .to_string(),
        );
    }
    if d.vectors.as_ref().is_some_and(|v| !v.partitioned) {
        out.push(
            "The one-time vector rebuild for this version has not run yet (the app or `cal \
             watch` runs it when idle)."
                .to_string(),
        );
    }
    out
}

fn human_bytes(n: u64) -> String {
    const GB: f64 = (1u64 << 30) as f64;
    const MB: f64 = (1u64 << 20) as f64;
    let n = n as f64;
    if n >= GB {
        format!("{:.1} GB", n / GB)
    } else {
        format!("{:.0} MB", n / MB)
    }
}

fn when(ts: Option<i64>) -> String {
    ts.and_then(|t| chrono::DateTime::from_timestamp(t, 0))
        .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "never".to_string())
}

/// The report as text, for a terminal and for pasting into an issue.
pub fn render(d: &Doctor) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Callimachus {} on {}", d.version, d.platform);
    let i = &d.index;
    let _ = writeln!(out, "\nindex      {}", i.path);
    if i.exists {
        let _ = writeln!(
            out,
            "           {} on disk, {} free; schema {} ({}, this binary writes {})",
            i.bytes.map_or("?".into(), human_bytes),
            i.free_bytes.map_or("?".into(), human_bytes),
            i.schema.map_or("?".into(), |s| s.to_string()),
            i.schema_state,
            i.binary_schema
        );
        // "never" for the last pass would read as "never indexed" on a machine whose app
        // watcher has kept the index current for months without one.
        let _ = writeln!(
            out,
            "           {} threads, newest activity {}; last full pass {}",
            i.threads.map_or("?".into(), |t| t.to_string()),
            i.newest_thread_at.map_or("none".into(), |t| when(Some(t))),
            i.last_pass_at
                .map_or("not recorded".into(), |t| when(Some(t)))
        );
        if let Some(q) = &i.quick_check {
            let _ = writeln!(out, "           quick_check: {q}");
        }
    } else {
        let _ = writeln!(out, "           (no index yet)");
    }
    let held: Vec<String> = d
        .locks
        .iter()
        .filter(|l| l.held)
        .map(|l| format!("{}{}", l.name, locks::pid_note(l.pid)))
        .collect();
    let _ = writeln!(
        out,
        "locks      {}",
        if held.is_empty() {
            "none held".to_string()
        } else {
            format!("held: {}", held.join(", "))
        }
    );
    let _ = writeln!(
        out,
        "model      {}",
        if d.model_cached {
            "on disk"
        } else {
            "not downloaded (keyword search only)"
        }
    );
    if let Some(v) = &d.vectors {
        let _ = writeln!(
            out,
            "vectors    {}{}",
            if v.partitioned {
                "partitioned (0036 shape)"
            } else {
                "old shape, rebuild pending"
            },
            match v.rebuild_due {
                Some(true) if v.partitioned => "; a compaction is due",
                _ => "",
            }
        );
    }
    let s = &d.snapshots;
    let newest = match (s.newest_reason.as_deref(), s.newest_at) {
        (Some(reason), at @ Some(_)) => format!(" (newest: {reason}, {})", when(at)),
        _ => String::new(),
    };
    let _ = writeln!(
        out,
        "snapshots  {}{}",
        match (s.count, s.automatic) {
            (0, true) => "none yet".to_string(),
            (0, false) => "none (not automatic here; `cal backup` makes one)".to_string(),
            (n, automatic) => format!(
                "{n}{newest}{}{}{}",
                s.bytes_on_disk
                    .map(|b| format!(", {} on disk", human_bytes(b)))
                    .unwrap_or_default(),
                match s.other_index {
                    0 => String::new(),
                    n => format!(", {n} of an earlier index (never removed automatically)"),
                },
                if automatic {
                    ""
                } else {
                    "; not automatic here (`cal backup` makes one)"
                }
            ),
        },
        match (s.last_check_ok, s.last_check_at) {
            (Some(true), at) => format!("; integrity check {}: ok", when(at)),
            (Some(false), Some(at)) => {
                format!("; integrity check {}: FOUND PROBLEMS", when(Some(at)))
            }
            (Some(false), None) => "; an earlier integrity check: FOUND PROBLEMS".to_string(),
            (None, _) => String::new(),
        }
    );
    if let Some(c) = &d.cal_link {
        let _ = writeln!(
            out,
            "cal        {} -> {}{}",
            c.path,
            c.target.as_deref().unwrap_or("(not a link)"),
            if c.live { "" } else { " (broken)" }
        );
    }
    let _ = writeln!(out, "\nagents");
    for a in &d.agents {
        if !a.present && a.hooks.is_empty() && a.mcp.is_none() {
            continue;
        }
        let hooks = if a.hooks.is_empty() {
            "no hooks".to_string()
        } else {
            a.hooks
                .iter()
                .map(|h| {
                    format!(
                        "{} ({}{})",
                        h.event,
                        h.subcommand,
                        if h.exe_exists { "" } else { ", binary missing" }
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mcp = match &a.mcp {
            Some(m) => format!(
                "MCP {} {}{}{}",
                m.kind,
                m.command,
                m.version
                    .as_deref()
                    .map(|v| format!(" ({v})"))
                    .unwrap_or_default(),
                if m.exe_exists == Some(false) {
                    " (missing)"
                } else {
                    ""
                }
            ),
            None => "no MCP entry".to_string(),
        };
        let _ = writeln!(
            out,
            "  {:<8} {}{}; {}",
            a.agent,
            if a.plugin { "plugin enabled; " } else { "" },
            hooks,
            mcp
        );
        if let Some(h) = a.hooks.iter().find(|h| !h.exe_exists) {
            let _ = writeln!(out, "           missing binary: {}", h.exe);
        }
    }
    if let Some(r) = &d.claude_retention {
        let _ = writeln!(
            out,
            "\nretention  Claude Code deletes transcripts after {} days{}; {} indexed sessions \
             exist only in the index",
            r.effective_days,
            if r.cleanup_period_days.is_none() {
                " (default)"
            } else {
                ""
            },
            r.preserved_sessions
        );
    }
    let s = &d.hook_skips;
    if s.total == 0 {
        let _ = writeln!(
            out,
            "hooks      no hook gave up in the last {} days",
            s.days
        );
    } else {
        let _ = writeln!(
            out,
            "hooks      gave up {} time(s) in the last {} days:",
            s.total, s.days
        );
        for (detail, n) in &s.by_hook {
            let _ = writeln!(out, "             {n:>5}  {detail}");
        }
    }
    if !d.parse_health.is_empty() {
        let _ = writeln!(
            out,
            "\nparsing (last judged pass; rolling over up to 8 passes)"
        );
        for p in &d.parse_health {
            let last = p
                .last
                .as_ref()
                .map(|l| parse_health::summary_line(&l.stats))
                .unwrap_or_else(|| "no pass".into());
            let _ = writeln!(out, "  {:<12} {last}", p.source);
            let _ = writeln!(
                out,
                "  {:<12} rolling {:.1}% unknown or malformed, {:.1}% of files without messages{}",
                "",
                p.rolling.bad_share() * 100.0,
                p.rolling.zero_share() * 100.0,
                match &p.drift {
                    Some(d) if d.dismissed => "; a format change was flagged (dismissed)",
                    Some(_) => "; FORMAT CHANGE FLAGGED",
                    None => "",
                }
            );
        }
    }
    if d.problems.is_empty() {
        let _ = writeln!(out, "\nno problems found");
    } else {
        let _ = writeln!(out, "\nproblems");
        for p in &d.problems {
            let _ = writeln!(out, "  - {p}");
        }
    }
    scrub(&out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::common::ParseStats;

    #[test]
    fn home_shows_as_tilde_only_where_a_path_starts() {
        assert_eq!(
            home_as_tilde("/Users/me/x and /Users/me", "/Users/me"),
            "~/x and ~"
        );
        assert_eq!(home_as_tilde("/Users/mel/x", "/Users/me"), "/Users/mel/x");
        assert_eq!(
            home_as_tilde(r#""/Users/me/bin/cal" hook"#, "/Users/me"),
            r#""~/bin/cal" hook"#
        );
    }

    /// The whole report on a temp index: schema, a lock held by this process, hook skips
    /// (one still in the skips file, one a rollup folded into the index) and a source's parse
    /// health, with nothing from the index's text in it.
    #[test]
    fn collects_the_report_from_a_temp_index() {
        let dir = db::test_support::TestDir::new("doctor");
        let conn = db::open(&dir).unwrap();
        conn.execute_batch(
            "INSERT INTO threads (source_id, external_id, title)
             SELECT id, 'x', 'a very private title' FROM sources WHERE kind = 'codex'",
        )
        .unwrap();
        let skips = ledger::skips_path_for(&dir).unwrap();
        let now = chrono::Utc::now().timestamp();
        let detail = "schema_behind session-end";
        ledger::append_to(&skips, ledger::Kind::HookSkipped, detail, now - 2 * 86_400);
        ledger::rollup(&conn).unwrap();
        assert!(!skips.exists(), "the past day is folded into the index");
        ledger::append_to(&skips, ledger::Kind::HookSkipped, detail, now);
        let ledger = ledger::path_for(&dir).unwrap();
        ledger::append_to(&ledger, ledger::Kind::Hook, "/private/project", now);
        let stats = ParseStats {
            lines: 100,
            files: 2,
            ..ParseStats::default()
        };
        indexer::parse_health::after_pass(&conn, "codex", &stats, true);
        let locks::Acquire::Acquired(_lock) = locks::acquire(&dir, locks::WATCH_LOCK).unwrap()
        else {
            panic!("free lock");
        };
        drop(conn);

        let d = collect(&dir, None, Options { quick_check: true });
        assert!(d.index.exists);
        assert_eq!(d.index.schema_state, "current");
        assert_eq!(d.index.threads, Some(1));
        assert_eq!(d.index.newest_thread_at, None, "the one thread has no time");
        assert_eq!(d.index.quick_check.as_deref(), Some("ok"));
        let watch = d.locks.iter().find(|l| l.name == "watch").unwrap();
        assert!(watch.held);
        assert_eq!(watch.pid, Some(std::process::id()));
        assert!(d
            .locks
            .iter()
            .filter(|l| l.name != "watch")
            .all(|l| !l.held));
        assert_eq!(
            d.hook_skips.total, 2,
            "from the file and from the folded key"
        );
        assert_eq!(d.hook_skips.by_class["schema_behind"], 2);
        assert_eq!(d.parse_health.len(), 1);
        assert_eq!(d.parse_health[0].rolling.lines, 100);
        assert!(d
            .problems
            .iter()
            .any(|p| p.contains("Hooks gave up 2 time(s)")));

        let text = render(&d);
        assert!(
            text.contains("1 threads, newest activity none; last full pass not recorded"),
            "{text}"
        );
        assert!(text.contains("schema_behind session-end"), "{text}");
        assert!(text.contains("watch (pid"), "{text}");
        let json = serde_json::to_string(&d).unwrap();
        for leak in ["a very private title", "/private/project"] {
            assert!(
                !text.contains(leak) && !json.contains(leak),
                "{leak} leaked"
            );
        }
    }

    /// No index: the report still comes back, saying so, and nothing is created.
    #[test]
    fn a_missing_index_is_reported_and_left_alone() {
        let dir = db::test_support::TestDir::new("doctor_none");
        let d = collect(&dir, None, Options::default());
        assert!(!d.index.exists);
        assert_eq!(d.index.schema_state, "unknown");
        assert!(d.problems[0].starts_with("No index yet"));
        assert!(
            std::fs::read_dir(dir.parent().unwrap())
                .unwrap()
                .next()
                .is_none(),
            "doctor created nothing next to a missing index"
        );
        assert_eq!(d.hook_skips.days, SKIP_DAYS);
        let text = render(&d);
        assert!(text.contains("(no index yet)"));
        assert!(
            text.contains("no hook gave up in the last 7 days"),
            "{text}"
        );
    }

    /// The snapshots next to the index, a failed weekly check and a damaged copy a restore
    /// left: each is in the report and named as a problem; reading them changes nothing.
    #[test]
    fn snapshots_a_failed_check_and_kept_damaged_copies_are_reported() {
        let dir = db::test_support::TestDir::new("doctor_snapshots");
        let conn = db::open(&dir).unwrap();
        knowledge::app_config::put(
            &conn,
            backup::LAST_CHECK_KEY,
            r#"{"at":1793491200,"ok":false,"ms":5}"#,
        )
        .unwrap();
        drop(conn);
        let folder = dir.parent().unwrap();
        let snapshot = folder
            .join(backup::SNAPSHOTS_DIR)
            .join("20261101T000000Z-weekly");
        std::fs::create_dir_all(&snapshot).unwrap();
        std::fs::write(snapshot.join("index.db"), b"planted").unwrap();
        std::fs::write(folder.join("index.db.damaged-20261102T000000Z"), [0u8; 9]).unwrap();
        std::fs::write(folder.join("index.db.restoring-20261103T000000Z"), [0u8; 4]).unwrap();
        // Without SQLite's own `-wal` / `-shm`, which any read of a closed index brings back.
        let listing = || {
            let mut names: Vec<String> = std::fs::read_dir(folder)
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| !n.ends_with("-wal") && !n.ends_with("-shm"))
                .collect();
            names.sort();
            names
        };
        let before = listing();

        let d = collect(&dir, None, Options::default());
        let s = &d.snapshots;
        assert_eq!((s.count, s.newest_at), (1, Some(1_793_491_200)));
        assert_eq!(s.newest_reason.as_deref(), Some("weekly"));
        assert_eq!(s.last_check_ok, Some(false));
        assert_eq!((s.damaged_copies, s.damaged_bytes), (1, 9));
        assert_eq!((s.staged_copies, s.staged_bytes), (1, 4));
        // The index has no identity yet: the weekly snapshot is not its own to remove.
        assert_eq!(s.other_index, 1);
        assert!(d
            .problems
            .iter()
            .any(|p| p.contains("weekly integrity check") && p.contains("found problems")));
        assert!(d
            .problems
            .iter()
            .any(|p| p.contains("1 damaged index copy a restore kept is still next")));
        assert!(d
            .problems
            .iter()
            .any(|p| p.contains("1 copy of a snapshot that an interrupted restore staged is")));
        let text = render(&d);
        assert!(
            text.contains("snapshots  1 (newest: weekly, 2026-11-01 00:00 UTC)"),
            "{text}"
        );
        assert!(
            text.contains(", 1 of an earlier index (never removed automatically)"),
            "{text}"
        );
        assert!(text.contains("FOUND PROBLEMS"), "{text}");
        assert_eq!(listing(), before, "doctor changed nothing");

        // A failed result carried across a rewrite (`at` 0) has no date: never 1970.
        let conn = db::open(&dir).unwrap();
        knowledge::app_config::put(
            &conn,
            backup::LAST_CHECK_KEY,
            r#"{"at":0,"ok":false,"ms":5}"#,
        )
        .unwrap();
        drop(conn);
        let d = collect(&dir, None, Options::default());
        assert_eq!(
            (d.snapshots.last_check_at, d.snapshots.last_check_ok),
            (None, Some(false))
        );
        let text = render(&d);
        assert!(
            text.contains("; an earlier integrity check: FOUND PROBLEMS"),
            "{text}"
        );
        assert!(!text.contains("1970"), "{text}");
        assert!(d
            .problems
            .iter()
            .any(|p| p.starts_with("An earlier weekly integrity check of the app found problems")));
        assert!(!d.problems.iter().any(|p| p.contains("1970")));

        // No index and no snapshots: the line still says so.
        let none = db::test_support::TestDir::new("doctor_snapshots_none");
        let text = render(&collect(&none, None, Options::default()));
        assert!(text.contains("snapshots  none"), "{text}");
    }

    /// An index this binary cannot open: the hook skips in the file next to it, which are
    /// what explains the failure, are still counted.
    #[test]
    fn an_unreadable_index_still_reports_the_skips_file() {
        let dir = db::test_support::TestDir::new("doctor_broken");
        std::fs::write(
            &*dir,
            b"not a database, just bytes long enough to have a header",
        )
        .unwrap();
        let skips = ledger::skips_path_for(&dir).unwrap();
        let now = chrono::Utc::now().timestamp();
        ledger::append_to(&skips, ledger::Kind::HookSkipped, "error hook claude", now);
        let d = collect(&dir, None, Options::default());
        assert!(d.index.exists);
        assert_eq!(d.hook_skips.total, 1, "{:?}", d.index.open_error);
        assert_eq!(d.hook_skips.by_class["error"], 1);
    }

    /// Type names come from the agents' files: a token-shaped one is redacted in the JSON
    /// report as well as in the text, in the counts, the drift and the problems.
    #[test]
    fn file_derived_type_names_are_redacted_in_the_json_too() {
        let dir = db::test_support::TestDir::new("doctor_redact");
        let conn = db::open(&dir).unwrap();
        // The AWS documentation's example key id, built at runtime.
        let token = concat!("AKIA", "IOSFODNN7", "EXAMPLE");
        let plain = ParseStats {
            lines: 100,
            files: 2,
            ..ParseStats::default()
        };
        indexer::parse_health::after_pass(&conn, "codex", &plain, true);
        let odd = ParseStats {
            unknown_kinds: BTreeMap::from([(token.to_string(), 80)]),
            ..plain
        };
        indexer::parse_health::after_pass(&conn, "codex", &odd, true);
        drop(conn);

        let d = collect(&dir, None, Options::default());
        let health = &d.parse_health[0];
        assert!(health.drift.as_ref().is_some_and(|x| x.by_share));
        assert!(d.problems.iter().any(|p| p.contains("codex's transcripts")));
        let json = serde_json::to_string(&d).unwrap();
        let text = render(&d);
        assert!(!json.contains(token) && !text.contains(token), "{json}");
        assert!(json.contains(context::REDACTED), "{json}");
    }
}
