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
//!   it finishes.

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
/// itself, so the one-time backfill for older indexes has nothing to do. Without this the
/// watcher (the app's or `cal watch`'s) later ran that full backfill over the whole new
/// corpus; the app's onboarding avoids it by starting its watcher before the first import.
fn mark_fresh_index(host: &CliHost) {
    if let Ok(conn) = host.db.0.lock() {
        let _ = knowledge::mark_todos_backfilled(&conn);
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

/// `cal index`: one catch-up pass over every source (unchanged files cost a stat each).
/// Progress goes to stderr, the summary to stdout; exits 1 when the scan failed or any
/// session could not be indexed.
pub fn cmd_index(args: &[String]) -> anyhow::Result<()> {
    no_args("index", args)?;
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
    let r = index_once(&host)?;
    println!(
        "indexed {} threads ({} unchanged, {} errors) in {:.1} s",
        r.threads_indexed,
        r.threads_skipped,
        r.errors,
        started.elapsed().as_secs_f64()
    );
    if r.errors > 0 {
        anyhow::bail!(
            "{} error{} while indexing; the [index] lines above name each file or source",
            r.errors,
            if r.errors == 1 { "" } else { "s" }
        );
    }
    Ok(())
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
    println!(
        "distilled {n} thread{}{}",
        if n == 1 { "" } else { "s" },
        if out.failed > 0 {
            format!(" ({} failed)", out.failed)
        } else {
            String::new()
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
