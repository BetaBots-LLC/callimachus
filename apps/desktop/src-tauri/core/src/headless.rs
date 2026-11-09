//! `cal index`, `cal watch`, `cal embed` and `cal distill --auto`: the app's background jobs
//! for a machine without the app (a server or a CI box). They run the
//! same `jobs` code as the app on a [`CliHost`] that prints progress instead of emitting to
//! a webview. All four may create and migrate the index (they are write commands); only
//! `cal embed` without `--no-download` may fetch the embedding model.
//!
//! Which commands yield to the app (`locks`, one set per index file):
//! - `cal watch` refuses to start while the app runs (`gui.lock`), and the app keeps its
//!   own watcher and auto-distill off while `cal watch` runs (`watch.lock`) and refuses a
//!   Shrink index or Reclaim space then.
//! - `cal distill --auto` refuses while either runs, since both distill on their own, and
//!   holds `distill.lock` for its drain, which the app's and `cal watch`'s auto-distill
//!   kicks skip while it is live: two drains would distill the same threads and spend the
//!   daily allowance twice.
//! - `cal index` and `cal embed` do not yield: the job flags that order the app's own jobs
//!   live in each process. Alongside a reindex or the watcher they only repeat work (a scan
//!   skips unchanged files, a vector batch is stored only for messages still pending), but
//!   a Shrink index or Reclaim space in the app holds the write lock for the whole rewrite,
//!   so a file or batch that waits past the busy timeout fails: run the command again after
//!   it finishes. Two `cal index` passes never overlap (`index.lock`).
//! - `cal index --if-stale` is the Claude Code plugin's SessionStart guard: it starts a
//!   `cal index` in the background only when nothing else keeps the index fresh, and like
//!   every hook it prints nothing, exits 0 and never creates the index. Its pass upgrades
//!   the schema only from the npm launcher's `cal` with no app running, a plugin-only
//!   setup's one writer (see [`stale_index_gate`]).

use crate::jobs::{self, EventSink, Host, JobFlags, JobHost, Task};
use crate::{db, embed, indexer, knowledge, locks};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One line to stderr. Not `eprintln!`, which panics on a closed stderr (`cal watch 2>&1 |
/// head`) and would take the watcher thread down with it.
fn note(msg: &str) {
    let _ = writeln!(std::io::stderr(), "{msg}");
}

/// Shortest gap between two `embed:progress` lines: a build sends one per 128 messages,
/// several a second.
const EMBED_LINE_GAP: Duration = Duration::from_secs(2);

/// What a [`CliHost`] remembers between events.
#[derive(Default)]
struct Progress {
    /// The source `index:progress` last named, to print a line per source.
    source: String,
    last_embed_line: Option<Instant>,
    /// The last `embed:error`, which the command turns into its exit code.
    error: Option<String>,
    /// Threads earlier auto-distill rounds went through: each round's `distill:progress`
    /// starts again at 1 of its own batch, so the lines add this to read as one drain.
    distill_base: i64,
    /// The running round's batch size (the last `distill:progress` total).
    distill_round: i64,
    /// The drain's upper bound ("distilling up to N threads"), when known.
    distill_limit: Option<i64>,
}

/// The [`JobHost`] of a headless `cal` command: its own flags, writer and model, and events
/// printed to stderr.
pub struct CliHost {
    flags: JobFlags,
    db: db::Db,
    readers: Option<db::ReadPool>,
    embedder: embed::Embedder,
    runtime: Option<tokio::runtime::Handle>,
    kicks_auto_distill: bool,
    progress: Mutex<Progress>,
}

impl EventSink for CliHost {
    fn emit(&self, event: &str, payload: serde_json::Value) {
        let Ok(mut p) = self.progress.lock() else {
            return;
        };
        let num = |key: &str| payload.get(key).and_then(serde_json::Value::as_i64);
        match event {
            "index:progress" => {
                let current = payload
                    .get("current")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                if !current.is_empty() && current != p.source {
                    note(&format!("[index] scanning {current}"));
                    p.source = current.to_string();
                }
            }
            "embed:progress" => {
                let due = p
                    .last_embed_line
                    .is_none_or(|at| at.elapsed() >= EMBED_LINE_GAP);
                if due {
                    if let (Some(done), Some(total)) = (num("done"), num("total")) {
                        note(&format!("[embed] {done} of {total} messages embedded"));
                    }
                    p.last_embed_line = Some(Instant::now());
                }
            }
            "embed:error" => {
                p.error = payload.as_str().map(str::to_string);
            }
            "distill:progress" => {
                if let (Some(done), Some(total)) = (num("done"), num("total")) {
                    p.distill_round = total;
                    if done > 0 {
                        let so_far = p.distill_base + done;
                        let of = p.distill_limit.unwrap_or(p.distill_base + total);
                        note(&format!("[distill] {so_far} of up to {of} threads"));
                    }
                }
            }
            "distill:done" => {
                p.distill_base += std::mem::take(&mut p.distill_round);
            }
            // index:error comes back as the command's own error; the rest (index:updated,
            // index:done, embed:done, ...) only tell a UI to refetch.
            _ => {}
        }
    }
}

impl JobHost for CliHost {
    fn flags(&self) -> &JobFlags {
        &self.flags
    }
    fn writer(&self) -> Option<&db::Db> {
        Some(&self.db)
    }
    fn readers(&self) -> Option<&db::ReadPool> {
        self.readers.as_ref()
    }
    fn embedder(&self) -> &embed::Embedder {
        &self.embedder
    }
    fn spawn(&self, task: Task) {
        match &self.runtime {
            Some(rt) => {
                rt.spawn(task);
            }
            None => app_log!("[cal] no runtime for a background task; skipped"),
        }
    }
    fn kicks_auto_distill(&self) -> bool {
        self.kicks_auto_distill
    }
    fn watch_batch(&self, kind: &str, paths: usize, threads: usize, elapsed: Duration) {
        // A batch that indexed nothing is not news: with Cursor open its database changes
        // every 30 s all day, which printed a "0 threads" line each time.
        if threads == 0 {
            return;
        }
        let line = format!(
            "{} {kind}: {threads} thread{} indexed from {paths} changed path{} in {} ms",
            chrono::Local::now().format("%H:%M:%S"),
            if threads == 1 { "" } else { "s" },
            if paths == 1 { "" } else { "s" },
            elapsed.as_millis()
        );
        let _ = writeln!(std::io::stdout(), "{line}");
    }
    fn watch_batch_failed(&self, kind: &str, paths: usize) {
        note(&format!(
            "{} {kind}: indexing {paths} changed path{} failed (the line above says why); \
             the next change retries",
            chrono::Local::now().format("%H:%M:%S"),
            if paths == 1 { "" } else { "s" },
        ));
    }
}

/// How a command sets up its host.
struct HostOptions {
    readers: bool,
    embedder: embed::Embedder,
    runtime: Option<tokio::runtime::Handle>,
    kicks_auto_distill: bool,
}

/// Open (creating and migrating if needed) the index for a headless job. The sidecar open
/// waits out an upgrade another process is running instead of failing.
fn open_host(opts: HostOptions) -> anyhow::Result<Arc<CliHost>> {
    let path = db::default_index_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let conn = db::open_sidecar(&path)?;
    // Relocated Claude / Codex data roots saved in Settings, as the app's startup loads them.
    indexer::claude::load_config_dir_setting(&conn);
    indexer::codex::load_codex_home_setting(&conn);
    let readers = if opts.readers {
        Some(db::read_pool(&path, 2)?)
    } else {
        None
    };
    Ok(Arc::new(CliHost {
        flags: JobFlags::default(),
        db: db::Db(Mutex::new(conn)),
        readers,
        embedder: opts.embedder,
        runtime: opts.runtime,
        kicks_auto_distill: opts.kicks_auto_distill,
        progress: Mutex::new(Progress::default()),
    }))
}

/// Whether the index holds no threads yet (true also when it cannot be read).
fn index_is_empty(host: &CliHost) -> bool {
    host.db
        .0
        .lock()
        .ok()
        .and_then(|c| {
            c.query_row("SELECT NOT EXISTS(SELECT 1 FROM threads)", [], |r| {
                r.get::<_, bool>(0)
            })
            .ok()
        })
        .unwrap_or(true)
}

/// Before the first import into an empty index: every TODO will come from indexing
/// itself (and every session's token usage), so the one-time TODO and usage backfills for
/// older indexes have nothing to do. Without this the
/// watcher (the app's or `cal watch`'s) later ran that full backfill over the whole new
/// corpus; the app's onboarding avoids it by starting its watcher before the first import.
fn mark_fresh_index(host: &CliHost) {
    if let Ok(conn) = host.db.0.lock() {
        let _ = knowledge::mark_todos_backfilled(&conn);
        let _ = indexer::usage_backfill::mark_usage_backfilled(&conn);
    }
}

fn no_args(cmd: &str, args: &[String]) -> anyhow::Result<()> {
    match args.first() {
        Some(a) => anyhow::bail!("`cal {cmd}` takes no arguments (got '{a}'). Run `cal help`."),
        None => Ok(()),
    }
}

/// One full pass over every source, as Reindex in the app does.
fn index_once(host: &Arc<CliHost>) -> anyhow::Result<indexer::IndexReport> {
    let dyn_host: Host = host.clone();
    let job = jobs::claim_index_all(host.flags())
        .ok_or_else(|| anyhow::anyhow!("a reindex is already running in this process"))?;
    jobs::run_index_all(&dyn_host, job)
}

/// After `cal index`'s pass, what the watcher does after a batch and after its launch
/// catch-up: link the commits of the threads the pass wrote and refresh the issue clusters
/// when due ([`jobs::graph_upkeep_after_pass`], under the watcher's bounds), then the
/// one-time `file_edits` backfill for sessions indexed before 0034 and the one-time token
/// usage backfill for Codex, Gemini, Qwen and OpenCode sessions indexed before usage (each a
/// no-op once its app_config flag says done). A machine that keeps the index fresh only through the
/// plugin's `--if-stale` guard runs no watcher. Never fails the command: the pass already
/// landed.
fn graph_upkeep(host: &CliHost, pass_started: i64) {
    if let Ok(conn) = host.db.0.lock() {
        jobs::graph_upkeep_after_pass(&conn, pass_started);
    }
    match indexer::file_edits::backfill_file_edits_once(&host.db, &|| false) {
        Ok(Some(r)) => note(&format!(
            "filled file edits for {} older sessions ({} rows)",
            r.filled, r.rows
        )),
        Ok(None) => {}
        Err(e) => note(&format!("file edits backfill: {e:#}")),
    }
    match indexer::usage_backfill::backfill_usage_once(&host.db, &|| false) {
        Ok(Some(r)) if r.filled > 0 => note(&format!(
            "filled token usage for {} older sessions ({} messages)",
            r.filled, r.rows
        )),
        Ok(_) => {}
        Err(e) => note(&format!("usage backfill: {e:#}")),
    }
}

/// `cal index --if-stale`, the plugin's SessionStart guard.
pub const INDEX_IF_STALE: &str = "--if-stale";

/// `cal index`: one catch-up pass over every source (unchanged files cost a stat each).
/// Progress goes to stderr, the summary to stdout; exits 1 when the scan failed or any
/// session could not be indexed. With [`INDEX_IF_STALE`], the plugin's SessionStart guard
/// instead (see [`index_if_stale`]).
pub fn cmd_index(args: &[String]) -> anyhow::Result<()> {
    if let [flag] = args {
        if flag == INDEX_IF_STALE {
            return index_if_stale();
        }
    }
    if let Some(a) = args.first() {
        anyhow::bail!(
            "unknown argument '{a}' for `cal index` (it takes none, or {INDEX_IF_STALE}). Run `cal help`."
        );
    }
    let index = db::default_index_path();
    // A second pass alongside would only repeat the first one's work; the lock also stops
    // the plugin's guard from starting one while this runs. The guard reads "indexed
    // recently" from `jobs::LAST_PASS_AT`, which the pass stamps when it finishes.
    let _lock = match locks::acquire(&index, locks::INDEX_LOCK)? {
        locks::Acquire::Acquired(lock) => lock,
        locks::Acquire::Held(pid) => anyhow::bail!(
            "cal index is already running{} for this index (the Claude Code plugin starts one in \
             the background at session start); it finishes on its own",
            locks::pid_note(pid)
        ),
    };
    let started = Instant::now();
    let host = open_host(HostOptions {
        readers: false,
        embedder: embed::Embedder::cached_only(),
        runtime: None,
        // A drain spawned here would die with the process after reserving part of the
        // day's allowance; `cal distill --auto` is the headless drain.
        kicks_auto_distill: false,
    })?;
    if index_is_empty(&host) {
        mark_fresh_index(&host);
    }
    let pass_started = chrono::Utc::now().timestamp();
    let r = index_once(&host)?;
    // The checkouts this pass wrote sessions in: import their committed repo memory, as the
    // app's watcher does after each batch.
    if let Some(db) = host.writer() {
        if let Ok(conn) = jobs::lock_anyhow(db) {
            crate::knowledge::repo_memory::import_since(
                &conn,
                pass_started,
                chrono::Utc::now().timestamp(),
            );
        }
    }
    println!(
        "indexed {} threads ({} unchanged, {} errors) in {:.1} s",
        r.threads_indexed,
        r.threads_skipped,
        r.errors,
        started.elapsed().as_secs_f64()
    );
    graph_upkeep(&host, pass_started);
    if r.errors > 0 {
        anyhow::bail!(
            "{} error{} while indexing; the [index] lines above name each file or source",
            r.errors,
            if r.errors == 1 { "" } else { "s" }
        );
    }
    Ok(())
}

/// How recent the last full pass must be for the plugin's SessionStart guard to leave the
/// index alone. An hour: a catch-up pass costs a stat per session file, which a session start
/// can afford hourly but not every time (several agents in parallel each start one).
const STALE_AFTER: Duration = Duration::from_secs(60 * 60);

/// The background pass's log, next to the index (`index.db.index.log`), appended to by each
/// pass and started over once it passes [`INDEX_LOG_MAX_BYTES`].
const INDEX_LOG: &str = "index.log";
/// A pass writes a few hundred bytes (a line per source and the summary), so this keeps the
/// last several hundred passes.
const INDEX_LOG_MAX_BYTES: u64 = 256 * 1024;

/// What the SessionStart guard does with the index.
#[derive(Debug, PartialEq, Eq)]
enum StaleGate {
    /// Leave it alone; the reason, for a user who ran the guard by hand.
    Skip(&'static str),
    /// Leave it alone because its schema is not this binary's to change, with the line the
    /// guard writes to the index log (otherwise a stale index would have no visible cause).
    SchemaHeld(String),
    /// Start a pass, which first applies this many schema upgrades (0 for none).
    Run { upgrades: i32 },
}

/// `cal index --if-stale`: the Claude Code plugin's SessionStart guard, so a machine without
/// the app still gets a fresh index. Starts `cal index` in the background (detached, its
/// output appended to `index.db.index.log`) when [`stale_index_gate`] finds nothing else
/// keeping the index fresh, and returns at once: the session never waits on the pass, and
/// this session's own memory injection reads the index as it was. Silent as a hook; on a
/// terminal it says in one line what it did.
fn index_if_stale() -> anyhow::Result<()> {
    let result = start_pass_if_stale();
    match &result {
        Ok(line) => tell(line),
        Err(e) => tell(&format!(
            "cal index --if-stale: could not start a pass: {e:#}"
        )),
    }
    result.map(drop)
}

/// The guard's work, returning the line [`index_if_stale`] shows on a terminal.
fn start_pass_if_stale() -> anyhow::Result<String> {
    let index = db::default_index_path();
    // Resolved once, before the gate: on macOS `current_exe()` is the path as invoked (the
    // `~/.local/bin/cal` link), and the launcher's `followLinkedCal`, running at this same
    // session start, can re-point that link to a newer release. Spawning the link would then
    // run a `cal index` that migrates an index the gate checked against this binary.
    let exe = std::fs::canonicalize(std::env::current_exe()?)?;
    match stale_index_gate(&index, std::time::SystemTime::now(), is_launcher_cal(&exe)) {
        StaleGate::Skip(why) => Ok(format!("cal index --if-stale: left the index alone: {why}")),
        StaleGate::SchemaHeld(line) => {
            append_index_log(&index, &line);
            Ok(format!(
                "cal index --if-stale: left the index alone: {line}"
            ))
        }
        StaleGate::Run { upgrades } => {
            let mut header = "cal index, started by the SessionStart hook".to_string();
            if upgrades > 0 {
                header.push_str(&format!(
                    "; it applies {upgrades} schema upgrade(s) first (the launcher's cal, no app running)"
                ));
            }
            spawn_background_index(&index, &spawn_target(exe), &header)?;
            Ok(format!(
                "cal index --if-stale: started cal index in the background; its output goes to {}",
                locks::lock_path(&index, INDEX_LOG).display()
            ))
        }
    }
}

/// One line to stderr, only when a person is reading it: as a hook the guard must print
/// nothing (Claude Code shows hook stderr), but `cal index --if-stale` typed in a terminal
/// should say why it did nothing.
fn tell(line: &str) {
    use std::io::IsTerminal;
    if std::io::stderr().is_terminal() {
        note(line);
    }
}

/// Whether `exe` (already resolved) is a `cal` the npm launcher downloaded:
/// `<cache>/callimachus-mcp/<version>/cal-<triple>`. Matched by shape rather than against
/// the launcher's cache root, which follows `$XDG_CACHE_HOME`: the shell that ran
/// `npx callimachus-mcp --install-cal` and the agent running this hook can disagree on it.
fn is_launcher_cal(exe: &std::path::Path) -> bool {
    let name_of = |p: Option<&std::path::Path>| {
        p.and_then(std::path::Path::file_name)
            .and_then(|n| n.to_str())
            .map(str::to_string)
    };
    let version_dir = exe.parent();
    let is_version = |v: &str| {
        let core = v.split(['-', '+']).next().unwrap_or_default();
        let parts: Vec<&str> = core.split('.').collect();
        parts.len() == 3
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    };
    name_of(Some(exe)).is_some_and(|n| n.starts_with("cal-"))
        && name_of(version_dir).is_some_and(|v| is_version(&v))
        && name_of(version_dir.and_then(std::path::Path::parent)).as_deref()
            == Some("callimachus-mcp")
}

/// The binary the background pass runs: `exe`, except inside an AppImage, whose mount goes
/// away when the hook (the AppImage runtime's child) exits, under a pass still running from it.
fn spawn_target(exe: std::path::PathBuf) -> std::path::PathBuf {
    if crate::integration::running_from_appimage() {
        if let Ok(image) = crate::integration::stable_app_exe(&exe) {
            return image;
        }
    }
    exe
}

/// What the SessionStart guard does with the index at `index` at `now`. `launcher_cal`: this
/// binary is the npm launcher's `cal` ([`is_launcher_cal`]), the one `cal` allowed to
/// upgrade the index's schema from here. Cheapest checks first: most session starts stop at a
/// lock probe or one `app_config` row.
fn stale_index_gate(
    index: &std::path::Path,
    now: std::time::SystemTime,
    launcher_cal: bool,
) -> StaleGate {
    // Hooks never create the index: the first import is the user's call (`cal index`, or
    // Index my history in the app's onboarding, which an import from here would skip past).
    if !index.exists() {
        return StaleGate::Skip("no index yet");
    }
    if locks::holder(index, locks::INDEX_LOCK).is_some() {
        return StaleGate::Skip("a cal index pass is running");
    }
    if locks::holder(index, locks::GUI_LOCK).is_some() {
        return StaleGate::Skip("the app keeps the index fresh");
    }
    if locks::holder(index, locks::WATCH_LOCK).is_some() {
        return StaleGate::Skip("cal watch keeps the index fresh");
    }
    let Ok(conn) = db::open_readonly(index) else {
        return StaleGate::Skip("the index cannot be read");
    };
    // Only the launcher's `cal` may upgrade the schema, and only with no app running
    // (gui.lock was checked above): it is a plugin-only setup's sole writer, and refusing
    // left the index on the old schema after every schema-bumping release until the user
    // ran `cal index` by hand. An app-linked `cal` never does: a `cal` newer than the
    // installed app would upgrade the index past it (the app then cannot open it), and an
    // index ahead of this binary cannot be written by it at all.
    let upgrades = match db::schema_pending(&conn) {
        Ok(0) => 0,
        Ok(n) if n > 0 && launcher_cal => n,
        Ok(n) if n > 0 => {
            return StaleGate::SchemaHeld(format!(
                "the index needs {n} schema upgrade(s), which this cal leaves to the Callimachus app; open the app to apply them"
            ))
        }
        Ok(_) => {
            return StaleGate::SchemaHeld(
                "a newer Callimachus upgraded the index, which this cal cannot write; update this cal".into(),
            )
        }
        Err(_) => return StaleGate::Skip("the index cannot be read"),
    };
    // When the last full pass finished (`jobs::mark_pass_finished`), even one that found
    // nothing new: a quiet machine's threads all have an old `last_indexed_at`. A point
    // lookup on app_config's key, where `MAX(threads.last_indexed_at)` scanned the whole
    // table (1.4 s cold on 21k threads, inside the hook's 5 s). Unset (no pass since this
    // marker exists, or an app-only history) reads as stale. A pending upgrade does not
    // wait out the hour: until it runs, the MCP server refuses the index.
    let now_secs = now
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let last_pass = knowledge::app_config::get(&conn, jobs::LAST_PASS_AT)
        .ok()
        .flatten()
        .and_then(|v| v.parse::<i64>().ok());
    // A pass "after" now (clock skew) is as fresh as it gets.
    if upgrades == 0 && last_pass.is_some_and(|at| now_secs - at < STALE_AFTER.as_secs() as i64) {
        return StaleGate::Skip("a cal index pass finished within the hour");
    }
    // An empty index has had no first import yet; that stays the user's call, as above.
    match conn.query_row("SELECT EXISTS(SELECT 1 FROM threads)", [], |r| {
        r.get::<_, bool>(0)
    }) {
        Ok(true) => StaleGate::Run { upgrades },
        Ok(false) => StaleGate::Skip("the index is empty"),
        Err(_) => StaleGate::Skip("the index cannot be read"),
    }
}

/// Start `exe index` detached from the hook, with `header` opening its entry in the index log.
fn spawn_background_index(
    index: &std::path::Path,
    exe: &std::path::Path,
    header: &str,
) -> anyhow::Result<()> {
    // Not waited on: the pass outlives this hook, and dropping the handle neither kills nor
    // waits for it.
    spawn_detached(exe, &["index"], open_index_log(index, header))?;
    Ok(())
}

/// Start `exe args` detached: no stdin, stdout and stderr to `log` (or nowhere), in its own
/// process group (Unix) or a new one without a console (Windows), so neither Claude Code
/// waiting on the hook's pipes nor a signal to the hook's group reaches it.
fn spawn_detached(
    exe: &std::path::Path,
    args: &[&str],
    log: Option<std::fs::File>,
) -> std::io::Result<std::process::Child> {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(exe);
    cmd.args(args).stdin(Stdio::null());
    match log {
        Some(log) => {
            cmd.stdout(log.try_clone()?).stderr(log);
        }
        None => {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    cmd.spawn()
}

/// Write one `--- <time> <line>` entry to the index log, for a start the guard declined.
fn append_index_log(index: &std::path::Path, line: &str) {
    drop(open_index_log(index, &format!("SessionStart hook: {line}")));
}

/// The index log opened for appending, with a `--- <time> <header>` line written, or None
/// when it cannot be written (the pass then runs without a log).
fn open_index_log(index: &std::path::Path, header: &str) -> Option<std::fs::File> {
    let path = locks::lock_path(index, INDEX_LOG);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > INDEX_LOG_MAX_BYTES) {
        // Emptied, then opened for appending like any other start below: a handle opened
        // with truncate alone writes at its own offset, so a pass a second session started
        // at the same moment had its lines overwritten by this one's.
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path);
    }
    // Append, not truncate: two sessions starting at once can both pass the gate, and the
    // second pass's "already running" line must not wipe the first one's output.
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .ok()?;
    writeln!(
        file,
        "--- {} {header}",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    )
    .ok()?;
    Some(file)
}

/// `cal watch`: the app's watcher loop in the foreground, for a machine without the app.
/// Prints one line per indexed batch; the minute tick tops up the semantic index (when one
/// exists and the model is on disk; it never downloads), imports Claude Code memory and
/// kicks auto-distill when it is on. Refuses while the app runs. Exits 3 once a newer
/// Callimachus upgraded the index, so a login item restarts the updated binary.
pub fn cmd_watch(args: &[String]) -> anyhow::Result<()> {
    no_args("watch", args)?;
    let index = db::default_index_path();
    // Ours first, then theirs: the app takes gui.lock and then checks watch.lock, so two
    // starting at once can both refuse but never both watch.
    let _lock = match locks::acquire(&index, locks::WATCH_LOCK)? {
        locks::Acquire::Acquired(lock) => lock,
        locks::Acquire::Held(pid) => anyhow::bail!(
            "cal watch is already running{} for this index",
            locks::pid_note(pid)
        ),
    };
    if let Some(pid) = locks::holder(&index, locks::GUI_LOCK) {
        anyhow::bail!(
            "the Callimachus app is running{} and keeps the index fresh itself; quit it to use cal watch",
            locks::pid_note(pid)
        );
    }
    let runtime = tokio::runtime::Runtime::new()?;
    let host = open_host(HostOptions {
        readers: true,
        embedder: embed::Embedder::cached_only(),
        runtime: Some(runtime.handle().clone()),
        kicks_auto_distill: true,
    })?;
    // The watcher's launch catch-up skips an empty index (the app waits for onboarding to
    // start the first import); here the user asked for indexing, so import first.
    let empty = index_is_empty(&host);
    if empty {
        note("cal watch: the index is empty; importing every source first");
        mark_fresh_index(&host);
        index_once(&host)?;
    }
    note(&format!(
        "cal watch: watching your agents' session folders (index at {}); Ctrl-C stops",
        index.display()
    ));
    let result = if empty {
        indexer::watcher::run_after_full_pass(host)
    } else {
        indexer::watcher::run(host)
    };
    match result {
        Err(e) if db::is_schema_mismatch(&e) => {
            note(&format!("cal watch: stopping: {e}"));
            std::process::exit(WATCH_EXIT_SCHEMA_MOVED);
        }
        other => other,
    }
}

/// `cal watch`'s exit code when the index's schema moved past this binary: distinct from
/// 1 (an error), so a login item or a supervisor can restart on it after an update.
const WATCH_EXIT_SCHEMA_MOVED: i32 = 3;

/// `cal embed [--no-download]`: build the semantic index (every message not embedded yet)
/// in the foreground. The one `cal` path that may download the embedding model (about
/// 128 MB, once); `--no-download` keeps the cached-only rule of every other command.
pub fn cmd_embed(args: &[String]) -> anyhow::Result<()> {
    let mut no_download = false;
    for a in args {
        match a.as_str() {
            "--no-download" => no_download = true,
            other => anyhow::bail!("unknown argument '{other}' for `cal embed`. Run `cal help`."),
        }
    }
    if no_download && !embed::model_is_cached() {
        anyhow::bail!(
            "the embedding model is not on this machine yet; run `cal embed` without --no-download to fetch it (about 128 MB)"
        );
    }
    // Before anything downloads the model or creates an index: with no threads there is
    // nothing to embed, and the 128 MB download would be for nothing.
    if !db::default_index_path().exists() {
        anyhow::bail!("nothing to embed yet; run `cal index` first");
    }
    let started = Instant::now();
    let host = open_host(HostOptions {
        readers: false,
        embedder: if no_download {
            embed::Embedder::cached_only()
        } else {
            embed::Embedder::default()
        },
        runtime: None,
        kicks_auto_distill: false,
    })?;
    if index_is_empty(&host) {
        anyhow::bail!("nothing to embed yet; run `cal index` first");
    }
    let dyn_host: Host = host.clone();
    let running = jobs::claim_embed_build(host.flags(), jobs::EmbedTrigger::User)
        .ok_or_else(|| anyhow::anyhow!("a semantic build is already running in this process"))?;
    jobs::run_embed_build(&dyn_host, running, jobs::EmbedTrigger::User);
    if let Some(e) = host.progress.lock().ok().and_then(|mut p| p.error.take()) {
        anyhow::bail!("{e}");
    }
    let (done, total) = embed::embed_progress(&*jobs::lock_anyhow(&host.db)?)?;
    println!(
        "semantic index: {done} of {total} messages embedded ({:.1} s)",
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

/// `cal distill --auto`: one drain of the auto-distill worklist (threads active in the last
/// 30 days, idle for 30 minutes and not distilled yet) under the daily cap, with the engine
/// and the auto-distill switch saved in Settings. Refuses while the app or `cal watch` runs
/// (both distill on their own) or another drain holds `distill.lock`. Exits 1 when every
/// thread it tried failed (offline, an expired key, a logged-out CLI).
pub fn cmd_distill_auto(args: &[String]) -> anyhow::Result<()> {
    no_args("distill --auto", args)?;
    let index = db::default_index_path();
    if !index.exists() {
        anyhow::bail!("no index yet; run `cal index` first");
    }
    // Ours first, then theirs, as `cal watch` does: the app and `cal watch` hold their lock
    // from startup and check this one before each auto-distill kick.
    let _lock = match locks::acquire(&index, locks::DISTILL_LOCK)? {
        locks::Acquire::Acquired(lock) => lock,
        locks::Acquire::Held(pid) => anyhow::bail!(
            "cal distill --auto is already running{} for this index",
            locks::pid_note(pid)
        ),
    };
    if let Some(pid) = locks::holder(&index, locks::GUI_LOCK) {
        anyhow::bail!(
            "the Callimachus app is running{} and distills on its own; use Build memory there, or quit it first",
            locks::pid_note(pid)
        );
    }
    if let Some(pid) = locks::holder(&index, locks::WATCH_LOCK) {
        anyhow::bail!(
            "cal watch is running{} and distills on its own when auto-distill is on; stop it first",
            locks::pid_note(pid)
        );
    }
    let runtime = tokio::runtime::Runtime::new()?;
    let host = open_host(HostOptions {
        readers: true,
        embedder: embed::Embedder::cached_only(),
        runtime: Some(runtime.handle().clone()),
        kicks_auto_distill: false,
    })?;
    {
        let conn = jobs::lock_anyhow(&host.db)?;
        // Says what to fix when memory is off or the engine has no key, before any round
        // reserves part of the allowance.
        let (provider, model, _, _) = crate::resolve_distill_engine(&conn)?;
        match drain_gate(&conn, chrono::Utc::now().timestamp())? {
            DrainGate::Skip(why) => {
                println!("{why}");
                return Ok(());
            }
            DrainGate::Run(remaining) => {
                if let Ok(mut p) = host.progress.lock() {
                    p.distill_limit = Some(remaining);
                }
                note(&format!(
                    "distilling up to {remaining} threads with {provider}/{model}"
                ));
            }
        }
    }
    let dyn_host: Host = host.clone();
    let out = runtime.block_on(jobs::auto_distill_drain(&dyn_host));
    let n = out.distilled;
    // The grounding gate's drops are otherwise only in per-round log lines; the summary is
    // what a scheduled `cal distill --auto` run shows its user.
    let mut notes = Vec::new();
    if out.failed > 0 {
        notes.push(format!("{} failed", out.failed));
    }
    if out.ungrounded > 0 {
        notes.push(format!(
            "{} ungrounded fact{} dropped",
            out.ungrounded,
            if out.ungrounded == 1 { "" } else { "s" }
        ));
    }
    println!(
        "distilled {n} thread{}{}",
        if n == 1 { "" } else { "s" },
        if notes.is_empty() {
            String::new()
        } else {
            format!(" ({})", notes.join(", "))
        }
    );
    if n == 0 && out.failed > 0 {
        anyhow::bail!(
            "every thread it tried failed{}; check the engine in Settings > AI engine",
            out.last_error
                .map(|e| format!(" (last error: {e})"))
                .unwrap_or_default()
        );
    }
    Ok(())
}

/// Whether a `cal distill --auto` drain should start.
#[derive(Debug, PartialEq, Eq)]
enum DrainGate {
    /// Up to this many threads (today's remaining allowance).
    Run(i64),
    /// Nothing to do; the line says why.
    Skip(String),
}

/// The auto-distill switch and today's allowance, as the app's own drain checks them.
fn drain_gate(conn: &rusqlite::Connection, now: i64) -> anyhow::Result<DrainGate> {
    if !knowledge::get_config(conn)?.auto_distill {
        return Ok(DrainGate::Skip(
            "auto-distill is off; turn on \"Auto-distill in the background\" in Settings > AI engine to use cal distill --auto"
                .into(),
        ));
    }
    let cap = knowledge::auto_distill_daily_cap(conn)?;
    if cap == 0 {
        // README: "0 pauses it". It does not reset at midnight, so say what unpauses it.
        return Ok(DrainGate::Skip(
            "auto-distill is paused: its daily limit is 0 threads; raise it in Settings > AI engine".into(),
        ));
    }
    let remaining = knowledge::auto_distill_remaining_today(conn, now)?;
    if remaining <= 0 {
        return Ok(DrainGate::Skip(format!(
            "today's auto-distill allowance ({cap} threads) is spent; it resets at midnight UTC"
        )));
    }
    Ok(DrainGate::Run(remaining))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_index(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("calli_headless_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("index.db")
    }

    fn host_on(path: &std::path::Path, rt: &tokio::runtime::Runtime) -> Arc<CliHost> {
        Arc::new(CliHost {
            flags: JobFlags::default(),
            db: db::Db(Mutex::new(db::open(path).unwrap())),
            readers: Some(db::read_pool(path, 2).unwrap()),
            embedder: embed::Embedder::cached_only(),
            runtime: Some(rt.handle().clone()),
            kicks_auto_distill: false,
            progress: Mutex::new(Progress::default()),
        })
    }

    /// The plugin's SessionStart guard starts a pass only for an existing, non-empty index
    /// whose last full pass finished more than an hour ago, with no app, `cal watch` or other
    /// pass keeping it fresh.
    #[test]
    fn the_session_start_guard_only_starts_a_pass_for_a_stale_idle_index() {
        let path = temp_index("stale_gate");
        let now = std::time::SystemTime::now();
        let now_secs = chrono::Utc::now().timestamp();
        let gate = |p: &std::path::Path| stale_index_gate(p, now, false);
        let run = StaleGate::Run { upgrades: 0 };
        assert_eq!(gate(&path), StaleGate::Skip("no index yet"));
        assert!(!path.exists(), "the guard never creates the index");

        let mut conn = db::open(&path).unwrap();
        assert_eq!(gate(&path), StaleGate::Skip("the index is empty"));
        let sid = indexer::source_id(&conn, "claude_code").unwrap();
        indexer::upsert_thread(
            &mut conn,
            sid,
            &indexer::ParsedThread {
                external_id: "t".into(),
                messages: vec![indexer::ParsedMessage {
                    role: "user".into(),
                    text: "hello".into(),
                    tool_name: None,
                    ts: Some(now_secs),
                }],
                ..Default::default()
            },
        )
        .unwrap();
        // No full pass on record (only the app's watcher wrote threads, say): stale.
        assert_eq!(gate(&path), run);
        jobs::mark_pass_finished(&conn, now_secs - 60);
        assert_eq!(
            gate(&path),
            StaleGate::Skip("a cal index pass finished within the hour")
        );
        jobs::mark_pass_finished(&conn, now_secs - 2 * 3600);
        assert_eq!(gate(&path), run, "two hours old, nobody indexing");

        // A pass in progress, the app, or `cal watch`: each keeps it fresh. A pass that
        // started (took index.lock) but never finished leaves the index stale.
        for (name, why) in [
            (locks::INDEX_LOCK, "a cal index pass is running"),
            (locks::GUI_LOCK, "the app keeps the index fresh"),
            (locks::WATCH_LOCK, "cal watch keeps the index fresh"),
        ] {
            let locks::Acquire::Acquired(lock) = locks::acquire(&path, name).unwrap() else {
                panic!("{name} is free");
            };
            assert_eq!(gate(&path), StaleGate::Skip(why));
            drop(lock);
        }
        assert_eq!(
            gate(&path),
            run,
            "the pass that took index.lock never finished"
        );

        // A schema behind this binary: only the launcher's cal upgrades it; the app-linked
        // one leaves it to the app, and nobody writes an index a newer build upgraded.
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        conn.pragma_update(None, "user_version", version - 1)
            .unwrap();
        assert!(matches!(gate(&path), StaleGate::SchemaHeld(l) if l.contains("open the app")));
        // The upgrade runs at once, not an hour after the last pass: the MCP server refuses
        // the index until it does.
        jobs::mark_pass_finished(&conn, now_secs - 60);
        assert_eq!(
            stale_index_gate(&path, now, true),
            StaleGate::Run { upgrades: 1 }
        );
        jobs::mark_pass_finished(&conn, now_secs - 2 * 3600);
        conn.pragma_update(None, "user_version", version + 1)
            .unwrap();
        for launcher in [false, true] {
            assert!(
                matches!(stale_index_gate(&path, now, launcher), StaleGate::SchemaHeld(l) if l.contains("update this cal")),
                "launcher {launcher}"
            );
        }
        conn.pragma_update(None, "user_version", version).unwrap();
        assert_eq!(gate(&path), run);
    }

    /// The guard's freshness read is one row by app_config's primary key, not a scan: it runs
    /// at every session start inside a 5 s hook timeout, next to `cal hook` reading the index.
    #[test]
    fn the_guard_reads_the_last_pass_with_a_point_lookup() {
        let path = temp_index("last_pass_plan");
        let conn = db::open(&path).unwrap();
        let mut stmt = conn
            .prepare("EXPLAIN QUERY PLAN SELECT value FROM app_config WHERE key = ?1")
            .unwrap();
        let plan = stmt
            .query_map([jobs::LAST_PASS_AT], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join(" | ");
        assert!(
            plan.starts_with("SEARCH app_config USING INDEX") && plan.contains("(key=?)"),
            "{plan}"
        );
    }

    #[test]
    fn only_a_cal_in_the_launchers_cache_counts_as_the_launchers() {
        let yes = |p: &str| is_launcher_cal(std::path::Path::new(p));
        assert!(yes(
            "/home/me/.cache/callimachus-mcp/0.11.4/cal-x86_64-unknown-linux-gnu"
        ));
        // Another $XDG_CACHE_HOME than this process sees, and a prerelease.
        assert!(yes(
            "/tmp/xdg/callimachus-mcp/1.0.0-rc.1/cal-aarch64-apple-darwin"
        ));
        assert!(!yes(
            "/Applications/Callimachus.app/Contents/MacOS/callimachus"
        ));
        assert!(!yes(
            "/home/me/src/callimachus/apps/desktop/src-tauri/target/release/cal"
        ));
        assert!(!yes(
            "/home/me/.cache/callimachus-mcp/0.11.4/callimachus-mcp-x86_64-unknown-linux-gnu"
        ));
        assert!(!yes("/home/me/.cache/callimachus-mcp/latest/cal-x86_64"));
        assert!(!yes("/home/me/.cache/other/0.11.4/cal-x86_64"));
    }

    /// The log keeps earlier passes (two sessions starting at once must not wipe each other's
    /// output) and starts over once it is too big, still appending after that.
    #[test]
    fn the_index_log_appends_and_starts_over_when_big() {
        let path = temp_index("index_log");
        let log = locks::lock_path(&path, INDEX_LOG);
        drop(open_index_log(&path, "first").unwrap());
        append_index_log(&path, "left the index alone");
        let text = std::fs::read_to_string(&log).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.lines().next().unwrap().ends_with(" first"), "{text}");
        assert!(
            text.ends_with(" SessionStart hook: left the index alone\n"),
            "{text}"
        );

        std::fs::write(&log, vec![b'x'; INDEX_LOG_MAX_BYTES as usize + 1]).unwrap();
        let mut ours = open_index_log(&path, "after the reset").unwrap();
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.starts_with("--- "), "{}", &text[..20]);
        assert_eq!(text.lines().count(), 1);
        // Another session's pass appends meanwhile; ours must land after it, not over it.
        let mut theirs = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        writeln!(theirs, "their pass").unwrap();
        writeln!(ours, "our pass").unwrap();
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.ends_with("their pass\nour pass\n"), "{text}");
    }

    /// The background pass never holds up the session: the spawn returns while the child
    /// still runs, the child leads its own process group (a signal to the hook's group does
    /// not reach it), reads no stdin, and appends its output to the log.
    #[cfg(unix)]
    #[test]
    fn a_background_pass_is_detached_from_the_hook() {
        let path = temp_index("spawn_detached");
        let log = locks::lock_path(&path, INDEX_LOG);
        std::fs::write(&log, "an earlier pass\n").unwrap();
        let script = "read line; echo \"stdin=$?\"; \
                      echo \"pid=$$ pgid=$(ps -o pgid= -p $$ | tr -d ' ')\"; \
                      echo to-stderr >&2; sleep 1; echo done";
        let started = Instant::now();
        let mut child = spawn_detached(
            std::path::Path::new("/bin/sh"),
            &["-c", script],
            open_index_log(&path, "test pass"),
        )
        .unwrap();
        assert!(started.elapsed() < Duration::from_millis(900));
        assert!(
            child.try_wait().unwrap().is_none(),
            "returned before the child exits"
        );
        assert!(child.wait().unwrap().success());
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.starts_with("an earlier pass\n--- "), "{text}");
        assert!(text.contains(" test pass\n"), "{text}");
        assert!(
            text.contains("stdin=1\n"),
            "stdin is /dev/null (EOF): {text}"
        );
        assert!(
            text.contains("to-stderr\n") && text.ends_with("done\n"),
            "{text}"
        );
        let ids = text
            .lines()
            .find_map(|l| l.strip_prefix("pid="))
            .expect("the pid line");
        let (pid, pgid) = ids.split_once(" pgid=").unwrap();
        assert_eq!(pid, pgid, "the child leads its own process group");
        assert_ne!(pgid, std::process::id().to_string());
    }

    #[test]
    fn the_drain_honors_the_toggle_and_a_zero_cap_reads_as_paused() {
        let path = temp_index("gate");
        let conn = db::open(&path).unwrap();
        let now = 1_800_000_000;
        let skip = |g: DrainGate| match g {
            DrainGate::Skip(why) => why,
            DrainGate::Run(n) => panic!("ran with {n}"),
        };
        assert!(skip(drain_gate(&conn, now).unwrap()).contains("auto-distill is off"));
        knowledge::set_auto_distill(&conn, true).unwrap();
        assert_eq!(drain_gate(&conn, now).unwrap(), DrainGate::Run(50));
        knowledge::set_auto_distill_daily_cap(&conn, 0).unwrap();
        let why = skip(drain_gate(&conn, now).unwrap());
        assert!(why.contains("paused"), "{why}");
        assert!(!why.contains("resets"), "a zero cap does not reset: {why}");
    }

    /// An engine that fails every call ends the drain after one round, with the failures
    /// counted, instead of re-claiming batch after batch until the day's allowance is gone.
    #[test]
    fn a_drain_whose_calls_all_fail_stops_after_one_round() {
        let path = temp_index("drain_fails");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let host = host_on(&path, &rt);
        let now = chrono::Utc::now().timestamp();
        {
            let mut conn = host.db.0.lock().unwrap();
            let sid = indexer::source_id(&conn, "claude_code").unwrap();
            for i in 0..3 {
                indexer::upsert_thread(
                    &mut conn,
                    sid,
                    &indexer::ParsedThread {
                        external_id: format!("t{i}"),
                        title: Some(format!("t{i}")),
                        project_path: Some("/proj/drain".into()),
                        created_at: Some(now - 7_200),
                        updated_at: Some(now - 7_200),
                        messages: (0..4)
                            .map(|j| indexer::ParsedMessage {
                                role: if j % 2 == 0 { "user" } else { "assistant" }.into(),
                                text: format!("message {j} of thread {i}"),
                                tool_name: None,
                                ts: Some(now - 7_200),
                            })
                            .collect(),
                        ..Default::default()
                    },
                )
                .unwrap();
            }
            let put = |k: &str, v: &str| knowledge::app_config::put(&conn, k, v).unwrap();
            put("knowledge.enabled", "1");
            put("knowledge.auto_distill", "1");
            put("knowledge.provider", "ollama");
            put("knowledge.model", "test-model");
            // Nothing listens on the discard port: every call is refused at once.
            put("provider.ollama.base_url", "http://127.0.0.1:9");
        }
        let dyn_host: Host = host.clone();
        let out = rt.block_on(jobs::auto_distill_drain(&dyn_host));
        assert_eq!(out.distilled, 0);
        assert_eq!(out.failed, 3, "{out:?}");
        assert!(out.last_error.is_some());
        let conn = host.db.0.lock().unwrap();
        // One round reserved its 3 threads; no second round re-claimed them.
        assert_eq!(
            knowledge::auto_distill_remaining_today(&conn, now).unwrap(),
            47
        );
    }
}
