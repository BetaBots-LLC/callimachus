//! The value ledger: a local count of what Callimachus did for the user's agents (a
//! SessionStart injection, a recall hit, an MCP tool call), so the Stats tab and `cal stats`
//! can say "helped N times this week".
//!
//! Privacy: the ledger lives only in `<index dir>/ledger.jsonl` (`<stem>.ledger.jsonl` for an
//! index not named `index.db`, see [`path_for`]) and the `ledger.*` keys of `app_config`. It is never read by telemetry and never sent anywhere; nothing in this
//! module touches the network. A line holds a timestamp, a kind and a short detail (a tool
//! name or a project path).
//!
//! The hooks that gave up ([`Kind::HookSkipped`], [`append_skip`]) go to a file of their own
//! next to it, `ledger.skips.jsonl` ([`skips_path_for`]): every hook exits 0 silently on any
//! error, so without these lines a memory injection that stopped (a schema an update left
//! behind, a locked index, a payload an agent changed) left nothing to report. Their detail
//! is a closed class and the hook's subcommand, never project text; `cal doctor` prints the
//! last seven days of them by class ([`skips`]). Never in `ledger.jsonl`: an app older than
//! them (0.12) rolls that file up every minute and deletes each line it cannot parse, and the
//! Claude Code plugin's `cal` updates itself while the app waits for the user, the very
//! version skew these lines exist to explain.
//!
//! Writers are the hooks and the MCP server, short-lived processes that must never wait on
//! the index lock (a hook has a 1.5-15 s budget, and the app may hold the writer for tens of
//! seconds during an upgrade). So they never go through SQLite: each event is one `write(2)`
//! of one line to a file opened with O_APPEND, which the kernel serializes against every
//! other appender. Errors are swallowed; a missed count is better than a failed hook.
//!
//! Only the GUI reads the file back: [`rollup`] (at launch and on the watcher's minute tick)
//! folds every line older than today into `ledger.daily` and empties the file, keeping 90
//! days of per-day counts plus a running total. It folds the skips file the same way, into
//! `ledger.skips` by day and detail, so that file holds at most today's lines while the app
//! (or `cal watch`) runs; without either it stops growing at [`MAX_FILE_BYTES`] like the
//! ledger.
//!
//! A rollup renames the live file aside, puts today's lines back, commits the past days, and
//! deletes the renamed file last, so a crash at any step neither loses nor recounts a line
//! (see [`fold_file`]). One window stays open: an appender that opened the live file before
//! the rename and writes only after the rollup's final length check (descheduled for the whole
//! fold, or a laptop asleep between its open and its write) writes into the renamed file
//! after it was read, and that one line is deleted with it. Appenders take no lock, because
//! a hook must never wait; a missed count is the accepted cost.

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::knowledge::app_config;

/// The ledger's file name next to `index.db`; other index files get `<stem>.ledger.jsonl`.
pub const FILE: &str = "ledger.jsonl";
const LEDGER_EXT: &str = "jsonl";
/// `<base>.lock` is held by a rollup in progress, so the launch rollup and a minute tick (or
/// two app instances) never fold the same lines twice. `<base>` is the ledger's file name
/// without `.jsonl`, so each index's ledger has its own lock and rolled files.
const LOCK_EXT: &str = "lock";
/// A rollup moves the live file to `<base>.<nanos>.rolling` before reading it.
const ROLLING_EXT: &str = "rolling";

/// app_config: JSON map `"YYYY-MM-DD"` (local day) -> [`Counts`].
pub const DAILY_KEY: &str = "ledger.daily";
/// app_config: how many lines rollups have ever folded, pruned days included.
const TOTAL_KEY: &str = "ledger.total";
/// app_config: JSON map `"YYYY-MM-DD"` (local day) -> {hook skip detail -> count}: the
/// skips file's lines a rollup folded, by class and subcommand. Pruned to [`KEEP_DAYS`]
/// like `ledger.daily`.
const SKIPS_KEY: &str = "ledger.skips";
/// app_config: JSON map of rolled file name -> bytes folded, for every rolled file whose
/// commit landed but whose delete may not have. A rollup cut short after its commit (a
/// crash, a quit) then never counts that file again. Pruned to files that still exist.
const FOLDED_KEY: &str = "ledger.folded";

/// Days of per-day counts kept in `ledger.daily`.
const KEEP_DAYS: i64 = 90;
/// Past this size appends stop. Only a rollup empties the file, so without the app (a
/// CLI-only install) it would otherwise grow forever; an MCP line is ~70 bytes, so this is
/// years of heavy use.
const MAX_FILE_BYTES: u64 = 16 << 20;
/// Caps a line near 300 bytes. Each line is one `write(2)` to an O_APPEND file: the kernel
/// moves to the end and writes in one step, so concurrent appenders never overwrite each
/// other, and on a local disk a write that small is short only when the disk is full.
/// (PIPE_BUF does not apply: it bounds atomic writes to pipes, not to regular files.)
const MAX_DETAIL_BYTES: usize = 256;
/// A lock older than this was left by a rollup that died; a real one takes milliseconds.
const STALE_LOCK: Duration = Duration::from_secs(600);

/// What one ledger line records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// The SessionStart hook injected project memory.
    Hook,
    /// The MCP server answered a tool call.
    Mcp,
    /// The prompt hook (`recall-now`) surfaced prior work.
    Recall,
    /// The SessionStart hook injected the recent-sessions digest (no memory yet).
    Digest,
    /// The error-recall hook (`recall-error`) told the agent how a shell error it just hit
    /// was fixed before. Its own kind: counted as a recall hit it inflated "prompts where past
    /// work was surfaced" with shell commands.
    Playbook,
    /// A hook gave up (see [`SkipClass`]); the detail is `<class> <subcommand>[ <agent>]`.
    /// Only in the skips file ([`skips_path_for`]). One met in `ledger.jsonl` (an unreleased
    /// build wrote some there) is ignored by every reader of the ledger and dropped by its
    /// rollup.
    #[serde(rename = "hook_skipped")]
    HookSkipped,
}

/// Why a hook gave up, as [`Kind::HookSkipped`] records it. A closed set: the detail is a
/// class, never the error's text (which can hold paths and SQL).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipClass {
    /// The index needs migrations this binary would apply, which hooks never run.
    SchemaBehind,
    /// A newer Callimachus upgraded the index past this binary.
    SchemaAhead,
    /// The index stayed locked past the hook's wait (an upgrade, a large cleanup).
    Busy,
    /// The agent sent flags or a stdin payload this binary could not read.
    Payload,
    /// The hook panicked.
    Panic,
    /// Any other error.
    Error,
}

impl SkipClass {
    /// Every class, in the order `cal doctor` prints them.
    pub const ALL: [SkipClass; 6] = [
        SkipClass::SchemaBehind,
        SkipClass::SchemaAhead,
        SkipClass::Busy,
        SkipClass::Payload,
        SkipClass::Panic,
        SkipClass::Error,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            SkipClass::SchemaBehind => "schema_behind",
            SkipClass::SchemaAhead => "schema_ahead",
            SkipClass::Busy => "busy",
            SkipClass::Payload => "payload",
            SkipClass::Panic => "panic",
            SkipClass::Error => "error",
        }
    }
}

/// Events per kind (the `week` shape `value_ledger` returns and each `ledger.daily` day).
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct Counts {
    #[serde(default)]
    pub hook: u64,
    #[serde(default)]
    pub mcp: u64,
    #[serde(default)]
    pub recall: u64,
    #[serde(default)]
    pub digest: u64,
    /// Days rolled up before this kind existed read as 0.
    #[serde(default)]
    pub playbook: u64,
}

impl Counts {
    fn add(&mut self, kind: Kind) {
        match kind {
            Kind::Hook => self.hook += 1,
            Kind::Mcp => self.mcp += 1,
            Kind::Recall => self.recall += 1,
            Kind::Digest => self.digest += 1,
            Kind::Playbook => self.playbook += 1,
            // Not help: the skips file folds into `ledger.skips` ([`Fold::Skips`]).
            Kind::HookSkipped => {}
        }
    }

    fn merge(&mut self, other: &Counts) {
        self.hook += other.hook;
        self.mcp += other.mcp;
        self.recall += other.recall;
        self.digest += other.digest;
        self.playbook += other.playbook;
    }

    pub fn total(&self) -> u64 {
        self.hook + self.mcp + self.recall + self.digest + self.playbook
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Line {
    ts: i64,
    kind: Kind,
    #[serde(default)]
    detail: String,
    /// On a line a rollup put back on the live ledger: the rolled file it came from, so a
    /// rollup redone after a crash knows which of that file's lines are already back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    from: Option<String>,
}

impl Line {
    /// The line as JSON without its `from` tag: what makes two copies the same event.
    fn canonical(&self) -> String {
        let mut bare = self.clone();
        bare.from = None;
        serde_json::to_string(&bare).unwrap_or_default()
    }
}

/// The ledger next to `index`: `ledger.jsonl` for `index.db`, `<stem>.ledger.jsonl` for any
/// other name. The dev build's `index.dev.db` sits in the same folder as the installed app's
/// index, and a shared ledger would let the dev app fold the real hooks' counts into the dev
/// index for good. None for a path with no parent.
pub fn path_for(index: &Path) -> Option<PathBuf> {
    let dir = index.parent().filter(|d| !d.as_os_str().is_empty())?;
    let name = match index.file_stem().and_then(|s| s.to_str()) {
        None | Some("index") => FILE.to_string(),
        Some(stem) => format!("{stem}.{FILE}"),
    };
    Some(dir.join(name))
}

/// The ledger's file name without `.jsonl`: the prefix of its lock and rolled files.
fn base_name(ledger: &Path) -> String {
    let name = ledger
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| FILE.to_string());
    match name.strip_suffix(&format!(".{LEDGER_EXT}")) {
        Some(base) => base.to_string(),
        None => name,
    }
}

/// The hook skips file next to `index` ([`append_skip`]): `ledger.skips.jsonl` for
/// `index.db`, `<stem>.ledger.skips.jsonl` for any other name. Its lock and rolled files
/// (`ledger.skips.lock`, `ledger.skips.<n>.rolling`) never match the ledger's, so an app
/// older than this file neither rolls it up nor deletes it. None for a path with no parent.
pub fn skips_path_for(index: &Path) -> Option<PathBuf> {
    path_for(index).map(|ledger| skips_path(&ledger))
}

/// The skips file next to the ledger file `ledger`.
fn skips_path(ledger: &Path) -> PathBuf {
    ledger.with_file_name(format!("{}.skips.{LEDGER_EXT}", base_name(ledger)))
}

/// The ledger next to the database `conn` has open; None for an in-memory database.
pub fn path_for_conn(conn: &Connection) -> Option<PathBuf> {
    conn.path()
        .filter(|p| !p.is_empty())
        .and_then(|p| path_for(Path::new(p)))
}

/// Record one event in the default index's ledger. Never blocks on the index, never
/// creates the index dir, never fails: a missing dir or a full disk just drops the count.
pub fn append(kind: Kind, detail: &str) {
    if let Some(path) = path_for(&crate::db::default_index_path()) {
        append_to(&path, kind, detail, chrono::Utc::now().timestamp());
    }
}

/// Record that the hook `command` (a subcommand such as `hook` or `recall-now`, with the
/// agent `--for` named, if any) gave up for `class`: one O_APPEND line in the default index's
/// skips file ([`skips_path_for`]), created owner-only like the ledger and, like it, never
/// creating the index's directory. Only names from closed sets reach the line: the caller
/// passes the subcommand and agent it matched, never free text.
pub fn append_skip(class: SkipClass, command: &str, agent: Option<&str>) {
    if let Some(path) = skips_path_for(&crate::db::default_index_path()) {
        let detail = skip_detail(class, command, agent);
        append_to(
            &path,
            Kind::HookSkipped,
            &detail,
            chrono::Utc::now().timestamp(),
        );
    }
}

fn skip_detail(class: SkipClass, command: &str, agent: Option<&str>) -> String {
    match agent {
        Some(a) => format!("{} {command} {a}", class.as_str()),
        None => format!("{} {command}", class.as_str()),
    }
}

/// [`append`] to an explicit ledger file at time `ts`.
pub fn append_to(ledger: &Path, kind: Kind, detail: &str, ts: i64) {
    let _ = try_append(ledger, kind, detail, ts);
}

fn try_append(ledger: &Path, kind: Kind, detail: &str, ts: i64) -> std::io::Result<()> {
    let mut detail: String = detail.chars().filter(|c| !c.is_control()).collect();
    if detail.len() > MAX_DETAIL_BYTES {
        let mut end = MAX_DETAIL_BYTES;
        while !detail.is_char_boundary(end) {
            end -= 1;
        }
        detail.truncate(end);
    }
    let mut line = serde_json::to_string(&Line {
        ts,
        kind,
        detail,
        from: None,
    })?;
    line.push('\n');
    let mut file = open_append(ledger)?;
    if file.metadata()?.len() > MAX_FILE_BYTES {
        return Ok(());
    }
    // One write of the whole line: with O_APPEND the kernel places it at the end atomically
    // with respect to every other appender, so concurrent hooks never interleave bytes.
    file.write_all(line.as_bytes())
}

/// Open the ledger for appending, creating the file (never its directory: no index dir
/// means no app install) owner-only, since it holds project paths like the index does.
fn open_append(ledger: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(ledger)
}

/// The local calendar day of `ts` as `YYYY-MM-DD` (the user's "today", not UTC's).
fn day_key(ts: i64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_opt(ts, 0).single() {
        Some(t) => t.format("%Y-%m-%d").to_string(),
        None => chrono::DateTime::from_timestamp(ts, 0)
            .unwrap_or_default()
            .format("%Y-%m-%d")
            .to_string(),
    }
}

fn parse_line(raw: &str) -> Option<Line> {
    serde_json::from_str(raw.trim()).ok()
}

fn load_daily(conn: &Connection) -> Result<BTreeMap<String, Counts>> {
    Ok(app_config::get(conn, DAILY_KEY)?
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default())
}

fn load_skips(conn: &Connection) -> Result<BTreeMap<String, BTreeMap<String, u64>>> {
    Ok(app_config::get(conn, SKIPS_KEY)?
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default())
}

fn load_folded(conn: &Connection) -> Result<BTreeMap<String, u64>> {
    Ok(app_config::get(conn, FOLDED_KEY)?
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default())
}

fn load_total(conn: &Connection) -> Result<u64> {
    Ok(app_config::get(conn, TOTAL_KEY)?
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0))
}

/// What a rollup did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RollupReport {
    /// Lines folded into `app_config` (`ledger.daily`, and `ledger.skips` from the skips
    /// file).
    pub folded: u64,
}

/// Which file a rollup folds: the ledger (its help counts into `ledger.daily` and
/// `ledger.total`) or the skips file (into `ledger.skips`). Each drops a line of the other's
/// kind, as it drops a torn one: a `hook_skipped` line an unreleased build wrote into
/// `ledger.jsonl` is never counted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fold {
    Help,
    Skips,
}

impl Fold {
    fn takes(self, kind: Kind) -> bool {
        (kind == Kind::HookSkipped) == (self == Fold::Skips)
    }
}

/// Fold the ledger next to `conn`'s database into `ledger.daily`, and its skips file into
/// `ledger.skips` (GUI only: the hooks and the MCP server only append). A no-op until a file
/// holds a line from before today, so the minute tick costs two short reads.
pub fn rollup(conn: &Connection) -> Result<RollupReport> {
    let Some(path) = path_for_conn(conn) else {
        return Ok(RollupReport::default());
    };
    let now = chrono::Utc::now().timestamp();
    let mut report = rollup_at(conn, &path, now)?;
    report.folded += roll(conn, &skips_path(&path), now, Fold::Skips)?.folded;
    Ok(report)
}

/// [`rollup`] for an explicit ledger file at time `now` (the ledger alone, not its skips).
pub fn rollup_at(conn: &Connection, ledger: &Path, now: i64) -> Result<RollupReport> {
    roll(conn, ledger, now, Fold::Help)
}

/// Roll up `ledger` (the ledger or the skips file, as `fold` says) at time `now`.
fn roll(conn: &Connection, ledger: &Path, now: i64, fold: Fold) -> Result<RollupReport> {
    let Some(dir) = ledger.parent() else {
        return Ok(RollupReport::default());
    };
    let base = base_name(ledger);
    let Some(_lock) = LockFile::acquire(&dir.join(format!("{base}.{LOCK_EXT}")))? else {
        return Ok(RollupReport::default());
    };
    let today = day_key(now);
    let mut report = RollupReport::default();
    // A rollup cut short (a crash between the rename and the delete) left its file behind.
    for leftover in rolling_files(dir, &base)? {
        report.folded += fold_file(conn, ledger, &leftover, &today, now, fold)?;
    }
    if !starts_before(ledger, &today)? {
        return Ok(report);
    }
    // Move the live file aside: appends from here on create a fresh live file. The name is
    // unique (a nanosecond stamp, bumped past any file already there), so the fold records
    // and leftovers of two rollups never mix.
    let mut nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let rolling = loop {
        let candidate = dir.join(format!("{base}.{nanos:020}.{ROLLING_EXT}"));
        if !candidate.exists() {
            break candidate;
        }
        nanos += 1;
    };
    match std::fs::rename(ledger, &rolling) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        Err(e) => return Err(e).with_context(|| format!("moving {}", ledger.display())),
    }
    report.folded += fold_file(conn, ledger, &rolling, &today, now, fold)?;
    Ok(report)
}

/// Whether the ledger's first line is from before `today`. Lines are appended in time
/// order (and a rollup re-appends only today's), so the first line is the oldest.
fn starts_before(ledger: &Path, today: &str) -> Result<bool> {
    let file = match std::fs::File::open(ledger) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("reading {}", ledger.display())),
    };
    for raw in std::io::BufReader::new(file).lines() {
        let Ok(raw) = raw else { return Ok(true) }; // unreadable bytes: let a fold drop them
        if raw.trim().is_empty() {
            continue;
        }
        return Ok(match parse_line(&raw) {
            Some(line) => day_key(line.ts).as_str() < today,
            None => true,
        });
    }
    Ok(false)
}

/// The rolled files of the ledger whose base name is `base` (`<base>.<digits>.rolling`), in
/// the order they were rolled.
fn rolling_files(dir: &Path, base: &str) -> Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_prefix(base)?.strip_prefix('.'))
                .and_then(|n| n.strip_suffix(ROLLING_EXT)?.strip_suffix('.'))
                .is_some_and(|stamp| !stamp.is_empty() && stamp.bytes().all(|b| b.is_ascii_digit()))
        })
        .collect();
    out.sort();
    Ok(out)
}

/// Fold the lines of the moved-aside `rolling` file from before `today` into
/// `ledger.daily` (or, for the skips file, `ledger.skips`), put today's back on the live
/// file, and delete `rolling`. Returns the lines folded.
///
/// The steps run in the order that makes each crash point safe:
/// 1. Today's lines go back on the live ledger first, tagged with `rolling`'s name. A crash
///    after this but before the commit redoes the fold, and the redo skips the lines it finds
///    already back under that tag, so none is counted twice and none is lost, whatever day
///    the redo runs on.
/// 2. The commit folds the past days and records the name and the bytes it read in
///    `ledger.folded`. A redo after the commit only deletes the file.
/// 3. Lines an appender wrote into the renamed file after it was read (it had opened the
///    file before the rename) go back to the live ledger, untouched by the commit.
/// 4. The renamed file is deleted last.
fn fold_file(
    conn: &Connection,
    ledger: &Path,
    rolling: &Path,
    today: &str,
    now: i64,
    fold: Fold,
) -> Result<u64> {
    let name = rolling
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let bytes = std::fs::read(rolling).with_context(|| format!("reading {}", rolling.display()))?;
    let mut back = lines_back_from(ledger, &name)?;
    let mut folded_files = load_folded(conn)?;

    if let Some(&read) = folded_files.get(&name) {
        // Committed already: only a tail written after that read can be new.
        put_back_tail(ledger, &name, &bytes, read, &mut back)?;
        remove_rolled(rolling)?;
        return Ok(0);
    }

    let mut by_day: BTreeMap<String, Counts> = BTreeMap::new();
    let mut skips_by_day: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
    let mut keep = Vec::new();
    let mut folded = 0u64;
    // A torn or foreign line (none should exist: every write is one whole line) is dropped
    // rather than failing every later rollup.
    for line in String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(parse_line)
    {
        if take(&mut back, &line) {
            continue; // put back by a fold that died before its commit
        }
        if !fold.takes(line.kind) {
            continue;
        }
        let day = day_key(line.ts);
        if day.as_str() < today {
            match fold {
                Fold::Help => by_day.entry(day).or_default().add(line.kind),
                Fold::Skips => {
                    *skips_by_day
                        .entry(day)
                        .or_default()
                        .entry(line.detail)
                        .or_insert(0) += 1
                }
            }
            folded += 1;
        } else {
            keep.push(line);
        }
    }
    put_back(ledger, &name, keep)?;

    let tx = conn.unchecked_transaction()?;
    let oldest_kept = day_key(now - (KEEP_DAYS - 1) * 86_400);
    match fold {
        Fold::Help => {
            let mut daily = load_daily(&tx)?;
            for (day, counts) in &by_day {
                daily.entry(day.clone()).or_default().merge(counts);
            }
            daily.retain(|day, _| day.as_str() >= oldest_kept.as_str());
            app_config::put(&tx, DAILY_KEY, &serde_json::to_string(&daily)?)?;
            app_config::put(&tx, TOTAL_KEY, &(load_total(&tx)? + folded).to_string())?;
        }
        Fold::Skips => {
            let mut skips = load_skips(&tx)?;
            for (day, details) in skips_by_day {
                let into = skips.entry(day).or_default();
                for (detail, n) in details {
                    *into.entry(detail).or_insert(0) += n;
                }
            }
            skips.retain(|day, _| day.as_str() >= oldest_kept.as_str());
            app_config::put(&tx, SKIPS_KEY, &serde_json::to_string(&skips)?)?;
        }
    }
    let dir = rolling.parent().unwrap_or(Path::new("."));
    folded_files.retain(|n, _| dir.join(n).exists());
    folded_files.insert(name.clone(), bytes.len() as u64);
    app_config::put(&tx, FOLDED_KEY, &serde_json::to_string(&folded_files)?)?;
    tx.commit()?;

    let now_bytes =
        std::fs::read(rolling).with_context(|| format!("reading {}", rolling.display()))?;
    put_back_tail(ledger, &name, &now_bytes, bytes.len() as u64, &mut back)?;
    remove_rolled(rolling)?;
    Ok(folded)
}

/// The lines on the live ledger a fold of the rolled file `name` already put back, keyed by
/// [`Line::canonical`] with their count.
fn lines_back_from(ledger: &Path, name: &str) -> Result<HashMap<String, usize>> {
    let bytes = match std::fs::read(ledger) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", ledger.display())),
    };
    let mut back = HashMap::new();
    for line in String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(parse_line)
    {
        if line.from.as_deref() == Some(name) {
            *back.entry(line.canonical()).or_insert(0) += 1;
        }
    }
    Ok(back)
}

/// Whether `line` is one of the lines already back, consuming that copy.
fn take(back: &mut HashMap<String, usize>, line: &Line) -> bool {
    match back.get_mut(&line.canonical()) {
        Some(n) if *n > 0 => {
            *n -= 1;
            true
        }
        _ => false,
    }
}

/// Append `lines` to the live ledger in one write, tagged as put back from `name`.
fn put_back(ledger: &Path, name: &str, lines: Vec<Line>) -> Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    let mut out = String::new();
    for mut line in lines {
        line.from = Some(name.to_string());
        out.push_str(&serde_json::to_string(&line)?);
        out.push('\n');
    }
    let mut file = open_append(ledger).with_context(|| format!("writing {}", ledger.display()))?;
    file.write_all(out.as_bytes())?;
    Ok(())
}

/// Put back the lines of the rolled file past byte `read` (written after the fold read it),
/// minus any already back from an earlier attempt.
fn put_back_tail(
    ledger: &Path,
    name: &str,
    bytes: &[u8],
    read: u64,
    back: &mut HashMap<String, usize>,
) -> Result<()> {
    let Some(tail) = bytes.get(read as usize..).filter(|t| !t.is_empty()) else {
        return Ok(());
    };
    let lines = String::from_utf8_lossy(tail)
        .lines()
        .filter_map(parse_line)
        .filter(|line| !take(back, line))
        .collect();
    put_back(ledger, name, lines)
}

fn remove_rolled(rolling: &Path) -> Result<()> {
    match std::fs::remove_file(rolling) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", rolling.display())),
    }
}

/// An exclusive lock held by creating a file with O_EXCL; removed on drop.
struct LockFile(PathBuf);

impl LockFile {
    /// The lock, or None when another rollup holds it.
    fn acquire(path: &Path) -> Result<Option<LockFile>> {
        for _ in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(mut f) => {
                    let _ = write!(f, "{}", std::process::id());
                    return Ok(Some(LockFile(path.to_path_buf())));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age > STALE_LOCK);
                    if !stale {
                        return Ok(None);
                    }
                    let _ = std::fs::remove_file(path);
                }
                Err(e) => return Err(e).with_context(|| format!("locking {}", path.display())),
            }
        }
        Ok(None)
    }
}

impl Drop for LockFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The last seven local days (today included) and the all-time count.
#[derive(Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub week: Counts,
    pub all_time: u64,
}

/// [`Summary`] from `ledger.daily` plus the lines not rolled up yet (today's, or every line
/// when the app has not run since). Read-only: `cal stats` calls it on its read-only open.
pub fn summary(conn: &Connection, ledger: Option<&Path>, now: i64) -> Result<Summary> {
    let first_day = day_key(now - 6 * 86_400);
    let mut out = Summary {
        all_time: load_total(conn)?,
        ..Summary::default()
    };
    for (day, counts) in load_daily(conn)? {
        if day >= first_day {
            out.week.merge(&counts);
        }
    }
    let Some(ledger) = ledger else {
        return Ok(out);
    };
    let bytes = match std::fs::read(ledger) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| format!("reading {}", ledger.display())),
    };
    for line in String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(parse_line)
        .filter(|l| Fold::Help.takes(l.kind))
    {
        out.all_time += 1;
        if day_key(line.ts) >= first_day {
            out.week.add(line.kind);
        }
    }
    Ok(out)
}

/// Per-kind counts over every day `ledger.daily` still keeps ([`KEEP_DAYS`]) plus the lines
/// not rolled up yet: the `cal report` totals. Only the all-time total outlives the 90 days,
/// and it has no per-kind split.
#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct Kept {
    pub counts: Counts,
    /// The oldest local day (`YYYY-MM-DD`) with a count, None when there is none.
    pub first_day: Option<String>,
    pub all_time: u64,
}

/// [`Kept`] from local day `from` on (`YYYY-MM-DD`, None for every kept day). Read-only, like
/// [`summary`].
pub fn kept(conn: &Connection, ledger: Option<&Path>, from: Option<&str>) -> Result<Kept> {
    let mut out = Kept {
        all_time: load_total(conn)?,
        ..Kept::default()
    };
    let note = |out: &mut Kept, day: &str| {
        if out.first_day.as_deref().is_none_or(|d| day < d) {
            out.first_day = Some(day.to_string());
        }
    };
    for (day, counts) in load_daily(conn)? {
        if from.is_none_or(|f| day.as_str() >= f) && counts.total() > 0 {
            out.counts.merge(&counts);
            note(&mut out, &day);
        }
    }
    let Some(ledger) = ledger else {
        return Ok(out);
    };
    let bytes = match std::fs::read(ledger) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| format!("reading {}", ledger.display())),
    };
    for line in String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(parse_line)
        .filter(|l| Fold::Help.takes(l.kind))
    {
        out.all_time += 1;
        let day = day_key(line.ts);
        if from.is_none_or(|f| day.as_str() >= f) {
            out.counts.add(line.kind);
            note(&mut out, &day);
        }
    }
    Ok(out)
}

/// The hook skips of the last `days` local days (today included), by detail
/// (`<class> <subcommand>[ <agent>]`): the days a rollup folded into `ledger.skips` (read
/// through `conn`, when the index is open and readable) plus the lines of the skips file
/// `skips_file` ([`skips_path_for`]) not rolled up yet. Read-only, like [`summary`]: `cal
/// doctor` calls it on its read-only open, or with no index at all, when the file's lines
/// are what explains why.
pub fn skips(
    conn: Option<&Connection>,
    skips_file: Option<&Path>,
    now: i64,
    days: i64,
) -> Result<BTreeMap<String, u64>> {
    let first_day = day_key(now - (days.max(1) - 1) * 86_400);
    let mut out: BTreeMap<String, u64> = BTreeMap::new();
    // An index this binary cannot read (a newer schema, a damaged file) still leaves the
    // file's lines to count.
    let folded = conn.and_then(|c| load_skips(c).ok()).unwrap_or_default();
    for (day, details) in folded {
        if day >= first_day {
            for (detail, n) in details {
                *out.entry(detail).or_insert(0) += n;
            }
        }
    }
    let Some(file) = skips_file else {
        return Ok(out);
    };
    let bytes = match std::fs::read(file) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| format!("reading {}", file.display())),
    };
    for line in String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(parse_line)
        .filter(|l| Fold::Skips.takes(l.kind) && day_key(l.ts) >= first_day)
    {
        *out.entry(line.detail).or_insert(0) += 1;
    }
    Ok(out)
}

/// Indexed Claude Code sessions whose transcript is gone from `projects_root`: deleted by
/// Claude Code's `cleanupPeriodDays` sweep (or by hand) and still searchable here, since no
/// indexer path removes a thread whose file vanished (only the user's own Cleanup does,
/// which deletes the row too). Subagent transcripts are left out: they go with their
/// session. Zero when the projects root itself is missing or holds none of the indexed
/// transcripts (a moved or switched config dir would otherwise count every session as
/// deleted).
///
/// One stat per session: ~21k on the author's index, run off the main thread.
pub fn preserved_claude_threads(conn: &Connection, projects_root: &Path) -> Result<i64> {
    if !projects_root.is_dir() {
        return Ok(0);
    }
    let mut stmt = conn.prepare(
        "SELECT t.external_id FROM threads t JOIN sources s ON s.id = t.source_id
         WHERE s.kind = 'claude_code' AND t.is_subagent = 0",
    )?;
    let ids = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let (mut gone, mut present) = (0i64, 0i64);
    for id in ids {
        // external_id is the path relative to the projects root (claude::external_id_for),
        // or absolute for a transcript outside it, which `join` keeps as is.
        if projects_root.join(id?).exists() {
            present += 1;
        } else {
            gone += 1;
        }
    }
    // Not one indexed transcript under this root: the threads were indexed under another
    // one (a changed `CLAUDE_CONFIG_DIR` or Settings override, a second account), not
    // deleted. Claiming them all as preserved would be false.
    Ok(if present == 0 { 0 } else { gone })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "calli_ledger_{tag}_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn temp_db(dir: &Path) -> Connection {
        crate::db::open(&dir.join("index.db")).unwrap()
    }

    /// Noon local time `days` days before now: far from a midnight, so day math is stable.
    fn noon(days_ago: i64) -> i64 {
        use chrono::TimeZone;
        let today = chrono::Local::now().date_naive() - chrono::Duration::days(days_ago);
        chrono::Local
            .from_local_datetime(&today.and_hms_opt(12, 0, 0).unwrap())
            .earliest()
            .unwrap()
            .timestamp()
    }

    fn lines(path: &Path) -> Vec<Line> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|l| parse_line(l).expect("every line is whole JSON"))
            .collect()
    }

    #[test]
    fn append_writes_one_json_line_and_never_creates_the_dir() {
        let dir = temp_dir("append");
        let ledger = dir.join(FILE);
        append_to(&ledger, Kind::Mcp, "search_threads", 1_700_000_000);
        append_to(&ledger, Kind::Hook, "/repo\nwith newline", 1_700_000_001);
        let text = std::fs::read_to_string(&ledger).unwrap();
        assert_eq!(
            text.lines().next().unwrap(),
            r#"{"ts":1700000000,"kind":"mcp","detail":"search_threads"}"#
        );
        let all = lines(&ledger);
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].detail, "/repowith newline", "control chars dropped");

        // No index dir: nothing is created and nothing panics.
        let missing = dir.join("nope").join(FILE);
        append_to(&missing, Kind::Hook, "x", 1);
        assert!(!missing.parent().unwrap().exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_appends_lose_and_tear_nothing() {
        let dir = temp_dir("contention");
        let ledger = dir.join(FILE);
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let ledger = ledger.clone();
                std::thread::spawn(move || {
                    for i in 0..250 {
                        append_to(&ledger, Kind::Mcp, &format!("tool-{t}-{i}"), 1_000 + i);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let all = lines(&ledger);
        assert_eq!(all.len(), 2_000);
        let unique: std::collections::HashSet<_> = all.iter().map(|l| &l.detail).collect();
        assert_eq!(unique.len(), 2_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn kept_sums_every_kept_day_and_the_live_lines_per_kind() {
        let dir = temp_dir("kept");
        let conn = temp_db(&dir);
        let ledger = dir.join(FILE);
        let now = noon(0);
        append_to(&ledger, Kind::Hook, "/a", noon(40));
        append_to(&ledger, Kind::Mcp, "search_threads", noon(3));
        rollup_at(&conn, &ledger, now).unwrap();
        append_to(&ledger, Kind::Hook, "/b", now);
        let all = kept(&conn, Some(&ledger), None).unwrap();
        assert_eq!((all.counts.hook, all.counts.mcp), (2, 1));
        assert_eq!(all.all_time, 3);
        assert_eq!(all.first_day, Some(day_key(noon(40))));
        // From a later day on, the older day drops out of the per-kind counts only.
        let recent = kept(&conn, Some(&ledger), Some(&day_key(noon(5)))).unwrap();
        assert_eq!((recent.counts.hook, recent.counts.mcp), (1, 1));
        assert_eq!(recent.first_day, Some(day_key(noon(3))));
        assert_eq!(recent.all_time, 3);
        // No file and nothing rolled up: nothing counted, no first day.
        let empty = temp_dir("kept_empty");
        let fresh = temp_db(&empty);
        assert_eq!(
            kept(&fresh, Some(&empty.join(FILE)), None).unwrap(),
            Kept::default()
        );
    }

    #[test]
    fn rollup_folds_past_days_keeps_today_and_counts_once() {
        let dir = temp_dir("rollup");
        let conn = temp_db(&dir);
        let ledger = dir.join(FILE);
        let now = noon(0);
        append_to(&ledger, Kind::Hook, "/a", noon(3));
        append_to(&ledger, Kind::Mcp, "search_threads", noon(3));
        append_to(&ledger, Kind::Digest, "/b", noon(1));
        append_to(&ledger, Kind::Playbook, "/b", noon(1));
        append_to(&ledger, Kind::Recall, "/a", now);
        append_to(&ledger, Kind::Mcp, "get_thread", now);

        let before = summary(&conn, Some(&ledger), now).unwrap();
        assert_eq!(before.all_time, 6);
        assert_eq!(before.week.total(), 6);
        assert_eq!((before.week.recall, before.week.playbook), (1, 1));

        let report = rollup_at(&conn, &ledger, now).unwrap();
        assert_eq!(report.folded, 4);
        // Today's lines stay in the file; the past days are in app_config.
        let left = lines(&ledger);
        assert_eq!(left.len(), 2);
        assert!(left.iter().all(|l| l.ts == now));
        let daily = load_daily(&conn).unwrap();
        assert_eq!(daily[&day_key(noon(3))].hook, 1);
        assert_eq!(daily[&day_key(noon(3))].mcp, 1);
        assert_eq!(daily[&day_key(noon(1))].digest, 1);
        assert_eq!(daily[&day_key(noon(1))].playbook, 1);
        assert!(!daily.contains_key(&day_key(now)));
        assert!(rolling_files(&dir, "ledger").unwrap().is_empty());
        assert!(!dir.join("ledger.lock").exists());

        // The same totals either side of the rollup, and a second rollup is a no-op.
        assert_eq!(summary(&conn, Some(&ledger), now).unwrap(), before);
        assert_eq!(rollup_at(&conn, &ledger, now).unwrap().folded, 0);
        assert_eq!(summary(&conn, Some(&ledger), now).unwrap(), before);

        // An append after the rollup lands in the fresh file.
        append_to(&ledger, Kind::Hook, "/c", now);
        assert_eq!(summary(&conn, Some(&ledger), now).unwrap().all_time, 7);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A hook skip is one line with a closed-set detail in the skips file, never in the
    /// ledger; the skips rollup folds it into `ledger.skips` by class and leaves the help
    /// counts alone.
    #[test]
    fn hook_skips_keep_their_class_through_their_own_rollup() {
        let dir = crate::db::test_support::TestDir::new("ledger_skips");
        let conn = crate::db::open(&dir).unwrap();
        let ledger = path_for(&dir).unwrap();
        let skips_file = skips_path_for(&dir).unwrap();
        assert_eq!(skips_file, ledger.with_file_name("ledger.skips.jsonl"));
        let now = noon(0);
        append_to(&ledger, Kind::Hook, "/a", noon(2));
        let skip = |class, command, agent, ts| {
            append_to(
                &skips_file,
                Kind::HookSkipped,
                &skip_detail(class, command, agent),
                ts,
            )
        };
        skip(SkipClass::SchemaBehind, "session-end", None, noon(2));
        skip(SkipClass::Busy, "hook", Some("codex"), now);
        assert_eq!(
            std::fs::read_to_string(&skips_file)
                .unwrap()
                .lines()
                .next()
                .unwrap(),
            format!(
                r#"{{"ts":{},"kind":"hook_skipped","detail":"schema_behind session-end"}}"#,
                noon(2)
            )
        );
        let help = summary(&conn, Some(&ledger), now).unwrap();
        assert_eq!((help.week.total(), help.all_time), (1, 1));
        let by_detail = BTreeMap::from([
            ("busy hook codex".to_string(), 1),
            ("schema_behind session-end".to_string(), 1),
        ]);
        assert_eq!(
            skips(Some(&conn), Some(&skips_file), now, 7).unwrap(),
            by_detail
        );

        let daily_before = app_config::get(&conn, DAILY_KEY).unwrap();
        assert_eq!(
            roll(&conn, &skips_file, now, Fold::Skips).unwrap().folded,
            1
        );
        assert_eq!(
            lines(&skips_file).len(),
            1,
            "today's skip stays in the file"
        );
        assert_eq!(
            app_config::get(&conn, DAILY_KEY).unwrap(),
            daily_before,
            "the help counts are not the skips rollup's"
        );
        assert_eq!(load_total(&conn).unwrap(), 0);
        assert_eq!(
            skips(Some(&conn), Some(&skips_file), now, 7).unwrap(),
            by_detail,
            "the same counts either side of the rollup"
        );
        assert_eq!(
            skips(Some(&conn), Some(&skips_file), now, 1).unwrap().len(),
            1,
            "today only"
        );
        // No index to read: the lines not rolled up yet still count.
        assert_eq!(
            skips(None, Some(&skips_file), now, 7).unwrap(),
            BTreeMap::from([("busy hook codex".to_string(), 1)])
        );
        // `rollup` folds the ledger and the skips file, each with its own lock and leftovers.
        assert_eq!(rollup_at(&conn, &ledger, now).unwrap().folded, 1);
        assert!(rolling_files(dir.parent().unwrap(), "ledger.skips")
            .unwrap()
            .is_empty());
        assert!(!dir.with_file_name("ledger.skips.lock").exists());
    }

    /// What 0.12's rollup does to `ledger.jsonl`: it keeps the lines it can parse (the five
    /// help kinds) and deletes the rest, every minute. Skips live in their own file, so an
    /// older app next to a newer hook binary (the plugin updates itself, the app waits for
    /// the user) erases none of them.
    #[test]
    fn an_older_apps_rollup_of_the_ledger_loses_no_skip() {
        #[derive(Deserialize)]
        #[serde(rename_all = "lowercase")]
        #[allow(dead_code)]
        enum OldKind {
            Hook,
            Mcp,
            Recall,
            Digest,
            Playbook,
        }
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct OldLine {
            ts: i64,
            kind: OldKind,
        }
        let dir = crate::db::test_support::TestDir::new("ledger_old_rollup");
        let ledger = path_for(&dir).unwrap();
        let skips_file = skips_path_for(&dir).unwrap();
        // The index exists (the hooks record nothing without one).
        drop(crate::db::open(&dir).unwrap());
        append_to(&ledger, Kind::Hook, "/a", noon(1));
        crate::db::test_support::with_default_index(&dir, || {
            append_skip(SkipClass::SchemaBehind, "session-end", None);
            append_skip(SkipClass::Busy, "hook", Some("claude"));
        });
        append_to(&ledger, Kind::Mcp, "search_threads", noon(0));
        let skipped_before = std::fs::read(&skips_file).unwrap();

        let text = std::fs::read_to_string(&ledger).unwrap();
        let kept_by_old: Vec<&str> = text
            .lines()
            .filter(|l| serde_json::from_str::<OldLine>(l).is_ok())
            .collect();
        assert_eq!(kept_by_old.len(), text.lines().count(), "nothing to delete");
        std::fs::write(&ledger, kept_by_old.join("\n") + "\n").unwrap();

        assert_eq!(std::fs::read(&skips_file).unwrap(), skipped_before);
        let now = chrono::Utc::now().timestamp();
        assert_eq!(
            skips(None, Some(&skips_file), now, 7)
                .unwrap()
                .values()
                .sum::<u64>(),
            2
        );
    }

    /// The ledger's readers never see a skip: `cal report`'s totals and the Stats card read
    /// the same with skips recorded (in the file and folded) or not. A `hook_skipped` line
    /// an unreleased build wrote into `ledger.jsonl` is ignored there and dropped by its
    /// rollup, and the skips count leaves it out.
    #[test]
    fn the_ledger_never_counts_a_skip() {
        let dir = crate::db::test_support::TestDir::new("ledger_kept_skips");
        let conn = crate::db::open(&dir).unwrap();
        let ledger = path_for(&dir).unwrap();
        let skips_file = skips_path_for(&dir).unwrap();
        let now = noon(0);
        append_to(&ledger, Kind::Hook, "/a", noon(2));
        append_to(&ledger, Kind::Hook, "/b", noon(2));
        let report = |conn: &Connection| {
            (
                kept(conn, Some(&ledger), None).unwrap(),
                summary(conn, Some(&ledger), now).unwrap(),
            )
        };
        let without = report(&conn);
        let detail = skip_detail(SkipClass::SchemaBehind, "hook", Some("claude"));
        for days in [3, 0] {
            append_to(&skips_file, Kind::HookSkipped, &detail, noon(days));
        }
        roll(&conn, &skips_file, now, Fold::Skips).unwrap();
        assert_eq!(report(&conn), without);

        // The unreleased build's line, a day before the help.
        let legacy = format!(
            r#"{{"ts":{},"kind":"hook_skipped","detail":"busy hook"}}"#,
            noon(3)
        );
        let text = std::fs::read_to_string(&ledger).unwrap();
        std::fs::write(&ledger, format!("{legacy}\n{text}")).unwrap();
        assert_eq!(report(&conn), without);
        assert_eq!(rollup_at(&conn, &ledger, now).unwrap().folded, 2);
        assert_eq!(report(&conn), without);
        assert!(!std::fs::read_to_string(&ledger)
            .unwrap_or_default()
            .contains("hook_skipped"));
        assert_eq!(
            skips(Some(&conn), Some(&skips_file), now, 7).unwrap(),
            BTreeMap::from([(detail, 2)])
        );
    }

    /// An app older than a kind drops its lines instead of failing or miscounting.
    #[test]
    fn a_line_of_an_unknown_kind_is_dropped() {
        assert!(parse_line(r#"{"ts":1,"kind":"from_the_future","detail":"x"}"#).is_none());
        assert!(parse_line(r#"{"ts":1,"kind":"hook_skipped","detail":"busy hook"}"#).is_some());
    }

    #[test]
    fn rollup_prunes_past_90_days_but_keeps_the_all_time_total() {
        let dir = temp_dir("prune");
        let conn = temp_db(&dir);
        let ledger = dir.join(FILE);
        let now = noon(0);
        append_to(&ledger, Kind::Hook, "/old", noon(120));
        append_to(&ledger, Kind::Hook, "/recent", noon(10));
        append_to(&ledger, Kind::Mcp, "x", noon(2));
        assert_eq!(rollup_at(&conn, &ledger, now).unwrap().folded, 3);
        let daily = load_daily(&conn).unwrap();
        assert!(!daily.contains_key(&day_key(noon(120))));
        assert!(daily.contains_key(&day_key(noon(10))));
        let s = summary(&conn, Some(&ledger), now).unwrap();
        assert_eq!(s.all_time, 3);
        // Only the line two days ago falls in the week.
        assert_eq!(
            s.week,
            Counts {
                mcp: 1,
                ..Counts::default()
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rollup_cut_short_is_finished_without_double_counting() {
        let dir = temp_dir("leftover");
        let conn = temp_db(&dir);
        let ledger = dir.join(FILE);
        let now = noon(0);
        // A rolled file whose fold was committed before the process died: its today's line
        // went back to the live ledger first, so finishing it only deletes it...
        let committed = dir.join("ledger.1.rolling");
        append_to(&committed, Kind::Hook, "/a", noon(2));
        append_to(&committed, Kind::Mcp, "kept", now);
        let read = std::fs::metadata(&committed).unwrap().len();
        let back = Line {
            ts: now,
            kind: Kind::Mcp,
            detail: "kept".into(),
            from: Some("ledger.1.rolling".into()),
        };
        std::fs::write(&ledger, serde_json::to_string(&back).unwrap() + "\n").unwrap();
        app_config::put(
            &conn,
            FOLDED_KEY,
            &format!(r#"{{"ledger.1.rolling":{read}}}"#),
        )
        .unwrap();
        app_config::put(&conn, TOTAL_KEY, "1").unwrap();
        // ...and one that died before its commit.
        let pending = dir.join("ledger.2.rolling");
        append_to(&pending, Kind::Recall, "/b", noon(1));

        let report = rollup_at(&conn, &ledger, now).unwrap();
        assert_eq!(report.folded, 1, "only the uncommitted file counts");
        assert_eq!(load_total(&conn).unwrap(), 2);
        assert_eq!(lines(&ledger).len(), 1, "today's line is back once");
        assert!(rolling_files(&dir, "ledger").unwrap().is_empty());
        assert_eq!(
            load_folded(&conn).unwrap().keys().collect::<Vec<_>>(),
            vec!["ledger.2.rolling"],
            "names of deleted files are pruned at the next commit"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The crash the put-back-first order exists for: today's lines were back on the live
    /// ledger but the commit never landed, and the app next runs two days later. Those lines
    /// are past now, yet they are counted once (from the live ledger), not twice, and the
    /// rest of the rolled file is folded.
    #[test]
    fn a_fold_that_died_before_its_commit_is_redone_on_a_later_day_exactly_once() {
        let dir = temp_dir("redo");
        let conn = temp_db(&dir);
        let ledger = dir.join(FILE);
        let rolled = dir.join("ledger.1.rolling");
        append_to(&rolled, Kind::Hook, "/old", noon(3));
        append_to(&rolled, Kind::Mcp, "same", noon(2));
        append_to(&rolled, Kind::Mcp, "same", noon(2));
        // Day 2's rollup put both of that day's lines back, then died.
        let back = Line {
            ts: noon(2),
            kind: Kind::Mcp,
            detail: "same".into(),
            from: Some("ledger.1.rolling".into()),
        };
        let one = serde_json::to_string(&back).unwrap() + "\n";
        std::fs::write(&ledger, one.repeat(2)).unwrap();
        append_to(&ledger, Kind::Recall, "/fresh", noon(1));

        let report = rollup_at(&conn, &ledger, noon(0)).unwrap();
        assert_eq!(report.folded, 4, "/old once, each `same` once, /fresh once");
        assert_eq!(load_total(&conn).unwrap(), 4);
        let daily = load_daily(&conn).unwrap();
        assert_eq!(daily[&day_key(noon(2))].mcp, 2);
        assert_eq!(daily[&day_key(noon(3))].hook, 1);
        assert!(lines(&ledger).is_empty());
        assert!(rolling_files(&dir, "ledger").unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Lines written into the rolled file after the fold read it (an appender that opened
    /// the live file before the rename) are put back on the live ledger, not deleted.
    #[test]
    fn a_late_write_into_the_rolled_file_is_put_back() {
        let dir = temp_dir("tail");
        let conn = temp_db(&dir);
        let ledger = dir.join(FILE);
        let rolled = dir.join("ledger.1.rolling");
        append_to(&rolled, Kind::Hook, "/a", noon(2));
        let read = std::fs::metadata(&rolled).unwrap().len();
        app_config::put(
            &conn,
            FOLDED_KEY,
            &format!(r#"{{"ledger.1.rolling":{read}}}"#),
        )
        .unwrap();
        append_to(&rolled, Kind::Mcp, "late", noon(0));

        assert_eq!(rollup_at(&conn, &ledger, noon(0)).unwrap().folded, 0);
        let left = lines(&ledger);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].detail, "late");
        assert!(rolling_files(&dir, "ledger").unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn each_index_file_has_its_own_ledger_lock_and_rolled_files() {
        let dir = temp_dir("names");
        assert_eq!(path_for(&dir.join("index.db")), Some(dir.join(FILE)));
        let dev = path_for(&dir.join("index.dev.db")).unwrap();
        assert_eq!(dev, dir.join("index.dev.ledger.jsonl"));
        assert_eq!(base_name(&dev), "index.dev.ledger");
        assert_eq!(base_name(&dir.join(FILE)), "ledger");

        // The dev app's rollup never touches the installed app's ledger.
        let conn = crate::db::open(&dir.join("index.dev.db")).unwrap();
        append_to(&dir.join(FILE), Kind::Hook, "/real", noon(2));
        append_to(&dev, Kind::Hook, "/dev", noon(2));
        std::fs::write(dir.join("ledger.7.rolling"), "").unwrap();
        assert_eq!(rollup(&conn).unwrap().folded, 1);
        assert_eq!(
            lines(&dir.join(FILE)).len(),
            1,
            "the real ledger is untouched"
        );
        assert!(
            dir.join("ledger.7.rolling").exists(),
            "and so are its rolled files"
        );
        assert!(rolling_files(&dir, "index.dev.ledger").unwrap().is_empty());

        // The skips files follow the same names, and no ledger's rollup (an older app's
        // included: the same `rolling_files` filter) takes a skips file's rolled files.
        assert_eq!(
            skips_path_for(&dir.join("index.db")),
            Some(dir.join("ledger.skips.jsonl"))
        );
        assert_eq!(
            skips_path_for(&dir.join("index.dev.db")),
            Some(dir.join("index.dev.ledger.skips.jsonl"))
        );
        std::fs::write(dir.join("ledger.skips.8.rolling"), "").unwrap();
        assert_eq!(
            rolling_files(&dir, "ledger").unwrap(),
            vec![dir.join("ledger.7.rolling")]
        );
        assert_eq!(
            rolling_files(&dir, "ledger.skips").unwrap(),
            vec![dir.join("ledger.skips.8.rolling")]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_long_detail_is_capped_in_bytes_on_a_char_boundary() {
        let dir = temp_dir("cap");
        let ledger = dir.join(FILE);
        append_to(&ledger, Kind::Hook, &"é".repeat(400), 1);
        let detail = &lines(&ledger)[0].detail;
        assert!(detail.len() <= MAX_DETAIL_BYTES && detail.len() >= MAX_DETAIL_BYTES - 1);
        assert!(detail.chars().all(|c| c == 'é'));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_held_lock_skips_the_rollup_and_a_stale_one_is_taken() {
        let dir = temp_dir("lock");
        let conn = temp_db(&dir);
        let ledger = dir.join(FILE);
        append_to(&ledger, Kind::Hook, "/a", noon(2));
        std::fs::write(dir.join("ledger.lock"), "123").unwrap();
        assert_eq!(rollup_at(&conn, &ledger, noon(0)).unwrap().folded, 0);
        assert_eq!(lines(&ledger).len(), 1, "a held lock leaves the file alone");

        let old = std::time::SystemTime::now() - Duration::from_secs(3_600);
        std::fs::File::options()
            .write(true)
            .open(dir.join("ledger.lock"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert_eq!(rollup_at(&conn, &ledger, noon(0)).unwrap().folded, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rollup_runs_against_the_ledger_next_to_the_open_database() {
        let dir = temp_dir("conn");
        let conn = temp_db(&dir);
        // SQLite reports the resolved path (/private/var on macOS for /var).
        let found = path_for_conn(&conn).unwrap();
        assert_eq!(found.file_name().unwrap(), FILE);
        assert_eq!(
            found.parent().unwrap().canonicalize().unwrap(),
            dir.canonicalize().unwrap()
        );
        append_to(&dir.join(FILE), Kind::Hook, "/a", noon(1));
        assert_eq!(rollup(&conn).unwrap().folded, 1);
        let mem = Connection::open_in_memory().unwrap();
        assert_eq!(path_for_conn(&mem), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_vanished_transcript_keeps_its_thread_and_counts_as_preserved() {
        let dir = temp_dir("preserved");
        let mut conn = temp_db(&dir);
        let root = dir.join("projects");
        std::fs::create_dir_all(root.join("-repo")).unwrap();
        let kept = root.join("-repo").join("kept.jsonl");
        let gone = root.join("-repo").join("gone.jsonl");
        let line = |sid: &str, text: &str| {
            format!(
                r#"{{"type":"user","sessionId":"{sid}","cwd":"/repo","timestamp":"2026-01-01T00:00:00Z","message":{{"role":"user","content":"{text}"}}}}"#
            )
        };
        std::fs::write(&kept, line("kept", "keep this session") + "\n").unwrap();
        std::fs::write(&gone, line("gone", "this one gets swept") + "\n").unwrap();
        crate::indexer::claude::scan_root(&mut conn, &root, &mut || {}).unwrap();
        let threads = |c: &Connection| -> i64 {
            c.query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(threads(&conn), 2);
        assert_eq!(preserved_claude_threads(&conn, &root).unwrap(), 0);

        // Claude Code's cleanup sweep deletes the transcript; a rescan keeps the thread.
        std::fs::remove_file(&gone).unwrap();
        crate::indexer::claude::scan_root(&mut conn, &root, &mut || {}).unwrap();
        assert_eq!(threads(&conn), 2, "a vanished transcript keeps its thread");
        assert_eq!(preserved_claude_threads(&conn, &root).unwrap(), 1);

        // A missing projects root counts nothing rather than everything.
        assert_eq!(
            preserved_claude_threads(&conn, &dir.join("elsewhere")).unwrap(),
            0
        );
        // So does a root that holds none of the indexed transcripts (a switched config dir).
        let other = dir.join("other-account");
        std::fs::create_dir_all(other.join("-repo")).unwrap();
        assert_eq!(preserved_claude_threads(&conn, &other).unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
