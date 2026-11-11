//! The background jobs: reindex, the watcher's catch-up, the semantic-index build, distill,
//! the auto-distill drain, harvest, and the whole-file rewrites (Shrink index / Reclaim space)
//! with the flags that keep them from writing side by side.
//!
//! They used to live in the app crate and read Tauri state; they now take a [`JobHost`]: the
//! app implements it over its `AppHandle` (events go out through Tauri's `Emitter`), and the
//! headless `cal index` / `cal watch` / `cal embed` / `cal distill --auto` implement it over a
//! plain struct that prints progress. Event names and payloads are the ones the frontend
//! listens to (`listen(` under apps/desktop/src), so they must not change: a payload goes out
//! as the `serde_json::Value` of the same struct the app used to emit, which Tauri serializes
//! to the same bytes (serde_json's `preserve_order` keeps the field order).

use crate::{agent, cleanup, context, db, embed, indexer, jobclock, knowledge, ledger, locks};
use serde::Serialize;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Where a job's events go: the app's webview (Tauri's `Emitter`), or a headless `cal`
/// command's stderr.
pub trait EventSink: Send + Sync {
    fn emit(&self, event: &str, payload: serde_json::Value);
}

/// Emit `payload` as JSON (what Tauri's `emit` would have serialized).
pub fn emit<S: EventSink + ?Sized, T: Serialize + ?Sized>(sink: &S, event: &str, payload: &T) {
    sink.emit(
        event,
        serde_json::to_value(payload).unwrap_or(serde_json::Value::Null),
    );
}

/// A boxed task for [`JobHost::spawn`].
pub type Task = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// What the jobs need from the process running them: the flags, the shared writer and read
/// pool, the shared embedding model, an async runtime for the LLM calls, and hooks for what
/// only one host does (the app's opt-in telemetry, `cal watch`'s batch lines).
pub trait JobHost: EventSink {
    fn flags(&self) -> &JobFlags;
    /// The shared writer connection (None before the app's startup opened the index).
    fn writer(&self) -> Option<&db::Db>;
    /// The pooled read-only connections (None before startup opened the index).
    fn readers(&self) -> Option<&db::ReadPool>;
    /// The shared query model (queries and fact embeds; a build loads its own).
    fn embedder(&self) -> &embed::Embedder;
    /// Run `task` on the host's async runtime, without waiting for it.
    fn spawn(&self, task: Task);
    /// Whether finished jobs kick the auto-distill drain. A one-shot `cal` command says no:
    /// a drain spawned on its runtime would die with the process after reserving part of
    /// the day's allowance.
    fn kicks_auto_distill(&self) -> bool {
        true
    }
    /// A user-started reindex finished (the app's opt-in `index_completed` event).
    fn index_completed(&self, _sources: usize, _threads: usize, _elapsed: Duration) {}
    /// A distill run stored this many threads (the app's opt-in `distill_completed` event).
    fn distill_completed(&self, _threads: usize) {}
    /// The watcher indexed one batch (`cal watch` prints a line per batch).
    fn watch_batch(&self, _kind: &str, _paths: usize, _threads: usize, _elapsed: Duration) {}
    /// The watcher's scan of one batch failed (the error is already logged).
    fn watch_batch_failed(&self, _kind: &str, _paths: usize) {}
    /// Host-specific work on the watcher's once-a-minute tick: the app hands its hooks, skill
    /// and MCP entry over to the Claude Code plugin, or takes them back. Not for `cal watch`:
    /// the hand-over re-points integrations at the running binary, which is the app's job.
    fn minute_tick(&self) {}
}

/// A shared handle to the host, cheap to clone into a worker thread or task.
pub type Host = Arc<dyn JobHost>;

fn writer(host: &dyn JobHost) -> &db::Db {
    host.writer()
        .expect("the index is open before any job runs")
}

/// Same as the app's `lock` but anyhow-typed, for background helpers. Never fails: a panic
/// under an earlier guard is recovered from ([`db::lock_conn`]); the `Result` keeps the
/// callers' `?`.
pub fn lock_anyhow(db: &db::Db) -> anyhow::Result<std::sync::MutexGuard<'_, rusqlite::Connection>> {
    Ok(db::lock_conn(&db.0))
}

/// The first line of a distill error, capped, for the thread's stored error.
pub fn short_error(e: &anyhow::Error) -> String {
    e.to_string()
        .lines()
        .next()
        .unwrap_or("distillation failed")
        .chars()
        .take(160)
        .collect()
}

/// Run blocking work on the runtime's blocking pool (the caller is a task on the host's
/// runtime, so this is Tauri's pool in the app).
async fn blocking<T, F>(f: F) -> anyhow::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| anyhow::anyhow!("background task failed: {e}"))?
}

/// [`blocking`] with a pooled read-only connection checked out on the worker thread.
async fn read_blocking<T, F>(pool: &db::ReadPool, f: F) -> anyhow::Result<T>
where
    T: Send + 'static,
    F: FnOnce(&rusqlite::Connection) -> anyhow::Result<T> + Send + 'static,
{
    // r2d2 pools are Arc-backed, so the clone is cheap and 'static for the closure.
    let pool = pool.0.clone();
    blocking(move || {
        let conn = pool.get().map_err(|e| anyhow::anyhow!("read pool: {e}"))?;
        f(&conn)
    })
    .await
}

// ---- flags ------------------------------------------------------------------------------

/// The background embedding job: a run guard plus its live progress counters, so
/// `embedding_status` can answer from memory while a build runs instead of re-running the
/// two COUNT(*) scans on every poll.
#[derive(Default)]
pub struct EmbedJob {
    pub running: Arc<JobFlag>,
    pub done: AtomicI64,
    pub total: AtomicI64,
    /// The user asked for the running build: it was started by a Build click, or one landed
    /// during a capped background top-up, which then hands over to a full build so the
    /// click's "up to date" toast is true. Such a build is never cancelled. Cleared when
    /// the build releases `running`.
    pub user_requested: AtomicBool,
    /// A reindex or an index rewrite asked the running background top-up to stop after its
    /// current batch (see [`request_embed_yield`]). Reset when a build starts.
    pub cancel: AtomicBool,
    /// Background top-ups stay off until then after one stopped because the model would
    /// not load (a corrupt cache): retrying every quiet minute would only repeat it.
    pub auto_paused_until: Mutex<Option<std::time::Instant>>,
    /// The watcher's vector rebuild ([`cleanup::rebuild_vec_chunks`]) stays off until then
    /// after one failed (too little free disk): each quiet minute would only repeat it.
    pub rebuild_paused_until: Mutex<Option<std::time::Instant>>,
    /// The [`JobGuard`] token of this process's build while it holds `embed.lock`, 0 when
    /// none does. A whole-file rewrite asks this process's build to yield; only another
    /// process's lock (a terminal `cal embed`) makes it refuse.
    lock_token: AtomicU64,
}

/// How long background top-ups pause after the model failed to load in one.
const AUTO_EMBED_FAILURE_PAUSE: Duration = Duration::from_secs(60 * 60);

impl EmbedJob {
    pub fn is_running(&self) -> bool {
        self.running.is_set()
    }

    /// Whether the build holding `embed.lock` is this process's running one. The token is
    /// stored after the lock is taken and stops matching once the guard releases `running`,
    /// before the lock itself drops, so a stale answer only ever reads as "another process"
    /// (a rewrite refuses for a moment) and never hides a terminal `cal embed`.
    fn holds_process_lock(&self) -> bool {
        let token = self.lock_token.load(Ordering::SeqCst);
        token != 0 && self.running.0.load(Ordering::SeqCst) == token
    }
}

/// Guards against more than one user-started re-index (Reindex, a single source) running
/// at a time. The watcher's launch catch-up holds [`CatchUpJob`] instead, so the search bar
/// and the Reindex button only report reindexes the user started.
#[derive(Default)]
pub struct IndexJob(pub Arc<JobFlag>);

/// Held while the watcher's launch catch-up rescans every source. It holds the write lock
/// like a reindex, so every gate that waits for [`IndexJob`] waits for this too, but it is
/// reported apart (`JobsStatus::catch_up`): it runs on every launch, takes 0.1-2 s, and
/// showing it as "Indexing..." flashed a 0% bar and a disabled Reindex button each time.
#[derive(Default)]
pub struct CatchUpJob(pub Arc<JobFlag>);

/// Guards against more than one project-distill running at a time; clearing it mid-run
/// (via `cancel_distill`) also signals the running job to stop.
#[derive(Default)]
pub struct DistillJob(pub Arc<JobFlag>);

/// Held while Shrink index / Reclaim space rewrites the whole file, so a second rewrite,
/// a reindex, the watcher catch-up, a semantic build or a distill doesn't start mid-VACUUM.
#[derive(Default)]
pub struct CompactJob(pub Arc<JobFlag>);

/// Every job's flag, in one place: what a host keeps for the life of the process.
#[derive(Default)]
pub struct JobFlags {
    pub index: IndexJob,
    pub catch_up: CatchUpJob,
    pub embed: EmbedJob,
    pub distill: DistillJob,
    pub compact: CompactJob,
}

/// One background job's busy flag: the token of the [`JobGuard`] holding it, 0 when free.
/// Tokens are never reused, so a guard can tell its own claim from a newer job's.
#[derive(Default)]
pub struct JobFlag(AtomicU64);

impl JobFlag {
    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::SeqCst) != 0
    }

    /// Free the flag whoever holds it (a cancel). The holder sees [`JobGuard::is_current`]
    /// turn false and stops, and its drop then leaves a newer job's claim alone.
    pub fn clear(&self) {
        self.0.store(0, Ordering::SeqCst);
    }
}

/// Source of [`JobGuard`] tokens; starts at 1 because 0 means "free".
static NEXT_JOB_TOKEN: AtomicU64 = AtomicU64::new(1);

/// A claimed background-job flag (IndexJob / CatchUpJob / EmbedJob / DistillJob /
/// CompactJob), released when dropped. The release profile keeps panic=unwind, so a panic
/// in a scan parser or in ort inference unwinds through this guard instead of skipping a
/// hand-written release. The flags gate each other, so one stuck flag used to make
/// Reindex, the semantic build and distill silently do nothing until the app restarted.
pub struct JobGuard {
    flag: Arc<JobFlag>,
    token: u64,
    /// The cross-process lock that goes with this claim (`rewrite.lock` for a rewrite,
    /// `embed.lock` for a build), dropped with it, after the flag is released.
    process_lock: Option<locks::ProcessLock>,
}

impl JobGuard {
    /// Claim `flag`, or None if a job already holds it.
    pub fn try_acquire(flag: &Arc<JobFlag>) -> Option<Self> {
        let token = NEXT_JOB_TOKEN.fetch_add(1, Ordering::Relaxed);
        flag.0
            .compare_exchange(0, token, Ordering::SeqCst, Ordering::SeqCst)
            .ok()?;
        Some(Self {
            flag: Arc::clone(flag),
            token,
            process_lock: None,
        })
    }

    /// Hold `lock` for as long as this claim: other processes see the job through it.
    fn holding(mut self, lock: locks::ProcessLock) -> Self {
        self.process_lock = Some(lock);
        self
    }

    /// Whether this guard still owns its flag: false once a cancel freed it, even if
    /// another job has claimed it since.
    pub fn is_current(&self) -> bool {
        self.flag.0.load(Ordering::SeqCst) == self.token
    }
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        // Release only our own claim. After cancel_distill a new distill may hold the flag,
        // and clearing it would make that job read the release as its own cancel.
        let _ = self
            .flag
            .0
            .compare_exchange(self.token, 0, Ordering::SeqCst, Ordering::SeqCst);
    }
}

/// Longest a reindex or an index rewrite waits for a background top-up to stop. It stops
/// after its current batch: 128 messages at about 1.7 vectors each (317k vectors for 190k
/// embedded messages on the measured index), about 3.5 s at the background model's 63
/// chunks/s on a 24-core machine, longer on a smaller one.
const EMBED_YIELD_WAIT: Duration = Duration::from_secs(10);

/// Ask a running background top-up to stop after its current batch, so a reindex or an
/// index rewrite gets the writer instead of being refused for a build the user never
/// started. False when the user asked for the running build (never cancelled). Call it
/// holding the caller's own flag (IndexJob / CompactJob): a build claims EmbedJob and then
/// checks those, so no new build starts after this, and a build that claimed first is the
/// one this sees and stops.
pub fn request_embed_yield(flags: &JobFlags) -> bool {
    let job = &flags.embed;
    if !job.is_running() {
        return true;
    }
    if job.user_requested.load(Ordering::SeqCst) {
        return false;
    }
    job.cancel.store(true, Ordering::SeqCst);
    true
}

/// Block until the semantic build releases EmbedJob, at most [`EMBED_YIELD_WAIT`]. Returns
/// whether it did. Call it off the main thread, after [`request_embed_yield`].
pub fn wait_for_embed_yield(flags: &JobFlags) -> bool {
    let deadline = std::time::Instant::now() + EMBED_YIELD_WAIT;
    let job = &flags.embed;
    while job.is_running() {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

/// The app's weekly index snapshot, from its once-a-minute tick (`JobHost::minute_tick`):
/// when one is due ([`crate::backup::weekly_due`]: a week since the last, a day since the
/// last integrity check) and no job holds the writer, run [`crate::backup::run_weekly`] on a
/// thread of its own. Expired snapshots of this index are retired on every tick (one
/// directory listing, the index's identity read on the pooled reader), and so are staged
/// copies a day-old interrupted restore left. Only the app: `cal watch` and `cal index` leave
/// snapshots to the app and `cal backup`.
pub fn snapshot_tick(host: &dyn JobHost) {
    static RUNNING: AtomicBool = AtomicBool::new(false);
    let index = db::default_index_path();
    let now = chrono::Utc::now().timestamp();
    if RUNNING.load(Ordering::SeqCst) || writer_job_running(host.flags()) {
        return;
    }
    crate::backup::remove_stale_staged(&index, now);
    let Some(conn) = host.readers().and_then(|pool| pool.0.get().ok()) else {
        return;
    };
    crate::backup::prune(&index, crate::backup::index_id(&conn).as_deref(), now);
    let due = crate::backup::weekly_due(&conn, now);
    drop(conn);
    if !due || RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("weekly-snapshot".into())
        .spawn(move || {
            let _ = crate::backup::run_weekly(&index, now);
            RUNNING.store(false, Ordering::SeqCst);
        });
    if spawned.is_err() {
        RUNNING.store(false, Ordering::SeqCst);
    }
}

/// Whether a job that holds the write lock for long stretches is running: a reindex (or the
/// watcher's catch-up), a semantic build, or a whole-file rewrite. Distill waits for these.
pub fn writer_job_running(flags: &JobFlags) -> bool {
    flags.index.0.is_set()
        || flags.catch_up.0.is_set()
        || flags.embed.is_running()
        || flags.compact.0.is_set()
}

// ---- events -----------------------------------------------------------------------------

/// Pushed on each embedded batch so the UI can show progress WITHOUT polling
/// `embedding_status` (which runs two locked COUNT(*) scans). Counts are tracked
/// incrementally by the job, not re-queried.
#[derive(Clone, Serialize)]
pub struct EmbedProgressEvent {
    pub done: i64,
    pub total: i64,
}

/// Pushed before each source during a background reindex, to drive a progress bar.
#[derive(Clone, Serialize)]
pub struct IndexProgressEvent {
    pub done: i64,
    pub total: i64,
    pub current: String,
}

/// Pushed per thread during a project distill, to drive a progress bar.
#[derive(Clone, Serialize)]
pub struct DistillProgressEvent {
    pub done: i64,
    pub total: i64,
}

// ---- reindex ----------------------------------------------------------------------------

/// `app_config` key: when the last full pass ([`run_index_all`], from the app's Reindex and
/// onboarding or from `cal index`) finished, in unix seconds.
pub const LAST_PASS_AT: &str = "index.last_pass_at";

/// Record that a full pass finished at `now` (best effort: a failed write only means the
/// guard may start the next pass sooner).
pub fn mark_pass_finished(conn: &rusqlite::Connection, now: i64) {
    let _ = knowledge::app_config::put(conn, LAST_PASS_AT, &now.to_string());
}

/// Kick off a re-index of every source in the BACKGROUND on a dedicated connection, so
/// the button returns instantly and the UI stays responsive. Emits `index:done` (with
/// the report) when finished, or `index:error` when the scan failed; no-op if one is
/// already running.
pub fn index_all(host: &Host) {
    // Note: a running distill does NOT block reindex: the distill loop yields to us.
    let Some(job) = claim_index_all(host.flags()) else {
        return;
    };
    let host = Arc::clone(host);
    std::thread::spawn(move || {
        let _ = run_index_all(&host, job);
    });
}

/// Claim [`IndexJob`] for a full reindex, or None when it should not start (one is already
/// running, a VACUUM or a build the user started is writing).
pub fn claim_index_all(flags: &JobFlags) -> Option<JobGuard> {
    let job = JobGuard::try_acquire(&flags.index.0)?; // None: already running
                                                      // Claim first, then check, so a semantic build starting meanwhile sees IndexJob and
                                                      // backs off. A VACUUM or a build the user started is writing: don't fight it for the
                                                      // lock (`job` drops). A background top-up stops after its batch instead: before, a
                                                      // Reindex during one silently did nothing.
    if flags.compact.0.is_set() || !request_embed_yield(flags) {
        return None;
    }
    Some(job)
}

/// The body of [`index_all`], on the calling thread (`cal index` runs it in the
/// foreground). Returns the scan's result; the events say the same.
pub fn run_index_all(host: &Host, job: JobGuard) -> anyhow::Result<indexer::IndexReport> {
    let flags = host.flags();
    if !wait_for_embed_yield(flags) {
        app_log!("[index] the semantic top-up is still running after 10 s; reindexing anyway");
    }
    wait_for_catch_up(flags);
    let started = std::time::Instant::now();
    let clock = jobclock::JobClock::start();
    // Dedicated connection (not the shared Mutex<Connection>): the scan never holds
    // the lock every other UI query needs; WAL lets readers proceed while we write.
    let prog = Arc::clone(host);
    let report = db::open(&db::default_index_path()).and_then(|mut c| {
        // Estimate the total from the existing thread count: on a re-index that's
        // accurate (≈ one file per thread), giving a real %; on a first run it's 0, so
        // the UI falls back to an indeterminate bar until rows start landing.
        let total_est: i64 = c
            .query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0))
            .unwrap_or(0);
        let r = indexer::scan_all_with_progress(&mut c, |seen, current| {
            emit(
                &*prog,
                "index:progress",
                &IndexProgressEvent {
                    done: seen as i64,
                    total: total_est,
                    current: current.to_string(),
                },
            );
        })?;
        // Read by the plugin's SessionStart guard (`cal index --if-stale`), which starts no
        // pass within the hour after this. Stamped here, at the end of a scan that ran, not
        // when one starts: a pass that dies at once (SQLITE_BUSY under a VACUUM) must not
        // read as an hour of freshness. Per-file errors still count as a pass, or one bad
        // file would start a full pass at every session start.
        mark_pass_finished(&c, chrono::Utc::now().timestamp());
        // Fold the WAL back into the main db so the -wal file doesn't grow unbounded
        // across reindex runs. PASSIVE never blocks readers.
        let _ = c.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");
        // Corpus shape for the opt-in `index_completed` event (bucketed there).
        let count = |sql: &str| {
            c.query_row(sql, [], |row| row.get::<_, i64>(0))
                .unwrap_or(0)
        };
        let shape = (
            count("SELECT COUNT(DISTINCT source_id) FROM threads"),
            count("SELECT COUNT(*) FROM threads"),
        );
        Ok((r, shape))
    });
    // Release here, not at the end of the closure: auto_distill_kick below returns early
    // while IndexJob is set, so a later release would skip the post-reindex distill.
    drop(job);
    let result = match report {
        Ok((r, (sources, threads))) => {
            app_log!(
                "[index] reindex: {} threads indexed, {} unchanged, {} errors in {}",
                r.threads_indexed,
                r.threads_skipped,
                r.errors,
                clock.thread_summary()
            );
            host.index_completed(
                sources.max(0) as usize,
                threads.max(0) as usize,
                started.elapsed(),
            );
            Ok(r)
        }
        Err(e) => {
            // An empty report alone reads as "you have no history" in onboarding, so
            // also say that the scan failed (and leave a trace in the log). Contract for
            // main.tsx: `index:error` (payload: the error chain as a string) always comes
            // BEFORE this run's `index:done`, whose report is then empty, so the
            // `index:done` handler can check for a stored error instead of treating the
            // run as "no threads found". A later successful run sends no `index:error`,
            // so clear the stored one when the next run starts.
            app_log!("[index] {e:#}");
            emit(&**host, "index:error", &format!("{e:#}"));
            Err(e)
        }
    };
    let empty = indexer::IndexReport::default();
    emit(&**host, "index:done", result.as_ref().unwrap_or(&empty));
    // Newly indexed threads may need distilling: drain them if auto-distill is on.
    auto_distill_kick(host);
    result
}

/// How long a user-started reindex waits for the launch catch-up before scanning anyway.
/// The catch-up measured 0.1-0.2 s warm and about 2 s with a changed Cursor DB; the scans
/// are idempotent, so going ahead past a stuck one only repeats work.
const CATCH_UP_WAIT: Duration = Duration::from_secs(30);

/// Block until the watcher's launch catch-up releases [`CatchUpJob`]. Call it holding
/// [`IndexJob`]: the catch-up claims its flag and then checks IndexJob, so once this claim
/// is visible no new catch-up starts, and only one already past that check is waited for.
pub fn wait_for_catch_up(flags: &JobFlags) {
    let deadline = std::time::Instant::now() + CATCH_UP_WAIT;
    let catch_up = &flags.catch_up;
    while catch_up.0.is_set() {
        if std::time::Instant::now() >= deadline {
            app_log!("[index] the launch catch-up is still running after 30 s; reindexing anyway");
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Claim [`IndexJob`] for a single-source reindex, or say why it can't start.
pub fn claim_index_source(flags: &JobFlags) -> anyhow::Result<JobGuard> {
    if flags.compact.0.is_set() {
        anyhow::bail!("The index is being rewritten. Try again when that finishes.");
    }
    JobGuard::try_acquire(&flags.index.0).ok_or_else(|| {
        anyhow::anyhow!("A re-index is already running. Try again when it finishes.")
    })
}

/// Index a single source by kind, holding a [`claim_index_source`] claim. Blocking: call it
/// off the main thread. Like index_all, a distill drain that yielded to this scan resumes once
/// the claim is released, also when the scan panics (see [`ReleaseThenKick`]).
pub fn index_source(
    host: &Host,
    job: JobGuard,
    kind: &str,
) -> anyhow::Result<indexer::IndexReport> {
    let _release = ReleaseThenKick {
        job: Some(job),
        host,
    };
    wait_for_catch_up(host.flags());
    // The shared source registry, so a new or renamed source kind can't miss this command.
    let scan =
        indexer::scanner_for(kind).ok_or_else(|| anyhow::anyhow!("unknown source kind: {kind}"))?;
    let mut conn = db::open(&db::default_index_path())?;
    indexer::scan_counted(&mut conn, kind, true, |conn| scan(conn, &mut || {}))
}

/// Releases a writer job's claim and then kicks auto-distill when it drops, on return or
/// while a panic unwinds. Before the jobs moved into core the app command kicked after its
/// `blocking(..).await`, which also ran after a panicked scan; a kick at the end of the
/// function body would be skipped by the unwind, and a drain that yielded to the scan would
/// wait for an unrelated later job to kick it.
struct ReleaseThenKick<'a> {
    job: Option<JobGuard>,
    host: &'a Host,
}

impl Drop for ReleaseThenKick<'_> {
    fn drop(&mut self) {
        // Released first: auto_distill_kick returns early while the writer flag is set.
        drop(self.job.take());
        auto_distill_kick(self.host);
    }
}

// ---- whole-file rewrites ------------------------------------------------------------------

/// Claim [`CompactJob`] for a whole-file rewrite, or say why it can't start. VACUUM holds
/// the write lock for the whole rewrite, so a reindex or semantic build running alongside
/// stalls on it, and a second rewrite fails with "database is locked". A background
/// top-up is asked to stop and waited for (at most [`EMBED_YIELD_WAIT`]), since the watcher
/// starts one after most quiet minutes of agent activity. The claim also holds
/// `rewrite.lock` until [`release_rewrite`], so the `cal` writers refuse to start meanwhile.
pub async fn claim_rewrite(flags: &JobFlags) -> anyhow::Result<JobGuard> {
    claim_rewrite_at(flags, &db::default_index_path()).await
}

/// [`claim_rewrite`] for the index at `index`.
async fn claim_rewrite_at(flags: &JobFlags, index: &std::path::Path) -> anyhow::Result<JobGuard> {
    const ALREADY: &str = "The index is already being rewritten. Try again when that finishes.";
    // Claim first, then check: a job that starts after this sees CompactJob and waits.
    let Some(job) = JobGuard::try_acquire(&flags.compact.0) else {
        anyhow::bail!("{ALREADY}");
    };
    // A weekly integrity check holds a read transaction for about a minute, which would keep
    // the VACUUM's closing checkpoint from giving the space back: stop it (it runs again
    // later, and the rewrite drops the snapshots it guards anyway).
    crate::backup::interrupt_check();
    // Then rewrite.lock, before looking at the `cal` locks: each `cal` writer takes its own
    // lock and then checks this one, so the two sides can both refuse but never both run.
    // A lock file that cannot be opened (another user's) leaves the rewrite as it was
    // before the lock existed rather than refusing it for good.
    let job = match locks::acquire(index, locks::REWRITE_LOCK) {
        Ok(locks::Acquire::Acquired(lock)) => job.holding(lock),
        Ok(locks::Acquire::Held(_)) => anyhow::bail!("{ALREADY}"),
        Err(e) => {
            app_log!("[cleanup] rewrite lock: {e}");
            job
        }
    };
    const EMBED_BUSY: &str = "The semantic index is being built";
    // The flags are per process: `cal watch` or `cal index` indexing, `cal embed` building,
    // or `cal distill --auto` storing facts, in another process would make the VACUUM fail
    // with "database is locked" after the busy timeout (or lose that process's write), so
    // refuse up front. This process's own build is asked to yield below instead.
    if let Some(why) = cal_writer_running(index, flags.embed.holds_process_lock()) {
        anyhow::bail!("{why}");
    }
    let busy = if flags.index.0.is_set() {
        Some("A reindex is running")
    } else if flags.catch_up.0.is_set() {
        Some("New sessions are being indexed")
    } else if flags.distill.0.is_set() {
        Some("Project memory is being distilled")
    } else if !request_embed_yield(flags) {
        Some(EMBED_BUSY)
    } else {
        None
    };
    if let Some(busy) = busy {
        anyhow::bail!("{busy}. Try again when it finishes."); // `job` drops: released
    }
    let deadline = std::time::Instant::now() + EMBED_YIELD_WAIT;
    while flags.embed.is_running() {
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("{EMBED_BUSY}. Try again when it finishes.");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(job)
}

/// Which `cal` process writing to `index` makes a whole-file rewrite refuse, as the
/// message for the user, or None when none is. `own_embed`: `embed.lock` is this process's
/// build's (see [`EmbedJob::holds_process_lock`]), which yields instead.
fn cal_writer_running(index: &std::path::Path, own_embed: bool) -> Option<&'static str> {
    if locks::holder(index, locks::WATCH_LOCK).is_some() {
        return Some("cal watch is writing; stop it first.");
    }
    if locks::holder(index, locks::DISTILL_LOCK).is_some() {
        return Some("cal distill --auto is writing; wait for it to finish.");
    }
    // The plugin's SessionStart guard starts one in the background, so this is routine
    // even on a machine where the user never typed `cal index`.
    if locks::holder(index, locks::INDEX_LOCK).is_some() {
        return Some(
            "cal index is updating the index (the Claude Code plugin starts one at session start); try again when it finishes.",
        );
    }
    // After watch.lock: `cal watch`'s own top-ups hold embed.lock too, and its line says
    // more. What is left is a terminal `cal embed`, which can run for an hour.
    if !own_embed && locks::holder(index, locks::EMBED_LOCK).is_some() {
        return Some("cal embed is building the semantic index; try again when it finishes.");
    }
    None
}

/// Release a [`claim_rewrite`] claim, then pick up the auto-distill drain it held off.
pub fn release_rewrite(host: &Host, job: JobGuard) {
    drop(job);
    auto_distill_kick(host);
}

/// What a `compact_index` pass did, for the Settings confirmation.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactSummary {
    pub rows: usize,
    pub bytes_saved: i64,
    /// The closing [`cleanup::vacuum`], once the trim is done (absent from the progress
    /// events). `ran: false` means the saved space stays inside the file, for later
    /// inserts to reuse, until enough is free for Reclaim space.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vacuum: Option<cleanup::VacuumReport>,
}

/// The body of "Shrink index", holding a [`claim_rewrite`] claim: cap tool output that was
/// indexed before the per-row cap existed (in batches, so other writers get turns), every
/// source's threads including those whose transcript is gone (the automatic pass leaves
/// those alone), then VACUUM when enough is free, by the same rule as Reclaim space
/// ([`cleanup::RECLAIM_MIN_BYTES`]). Dedicated connection for the same reason as
/// `vacuum_db`. Emits `compact:progress`. Blocking.
pub fn compact_index(sink: &dyn EventSink) -> anyhow::Result<CompactSummary> {
    let mut conn = db::open(&db::default_index_path())?;
    let mut summary = CompactSummary::default();
    let mut after = 0i64;
    loop {
        let r = cleanup::compact_tool_output_after(&mut conn, after, 2_000)?;
        after = r.last_id;
        summary.rows += r.rows;
        summary.bytes_saved += r.bytes_saved;
        emit(sink, "compact:progress", &summary);
        if r.done {
            break;
        }
    }
    // A superset of the background pass, so that pass has nothing left on later
    // launches and needn't rescan the table.
    if let Err(e) = cleanup::mark_tool_output_compacted(&conn) {
        app_log!("[cleanup] shrink index: {e:#}");
    }
    summary.vacuum = Some(cleanup::vacuum(&conn)?);
    Ok(summary)
}

/// Rows per background tool-output compaction pass ([`spawn_tool_output_compaction`]).
const BACKGROUND_COMPACT_ROWS: usize = 2_000;
/// How long after launch the compaction starts. The watcher's catch-up claims the writer
/// within moments of launch and takes 0.1-2 s (measured), so by then it is done and the
/// window has its first data; a catch-up deferred by a reindex is waited out below anyway.
const BACKGROUND_COMPACT_DELAY: Duration = Duration::from_secs(20);
/// Pause between passes, so the watcher and distill get the writer in between.
const BACKGROUND_COMPACT_GAP: Duration = Duration::from_secs(1);

/// Trim tool output indexed before the per-row cap existed, in the background after the
/// launch catch-up, so it no longer waits for a Shrink index click. Only threads whose
/// transcript is still on disk ([`cleanup::compact_tool_output_on_disk`]): a re-index of
/// those stores the same capped text anyway. For a thread whose transcript the agent has
/// since deleted, the index holds the only copy, and the cap would drop its middle for
/// good, so those wait for an explicit Shrink index, the user's choice. On a measured
/// 5.1 GB index the full trim was 22,619 rows and 245 MB of text, most of it from such
/// deleted transcripts; it took 11 passes of 2-11 s (62 s in all).
///
/// Passes of [`BACKGROUND_COMPACT_ROWS`] in short transactions, one log line each, until a
/// pass reaches the end of the table; then a marker in app_config skips it on later
/// launches. Each pass holds [`CompactJob`] like a Shrink index, so a rewrite never starts
/// alongside it and `jobs_status` reports it, and no pass starts while a reindex, the
/// catch-up, a semantic build or a whole-file rewrite holds the writer. A pass that trimmed
/// rows emits `index:updated` with no source kind (main.tsx refetches db_stats, so the
/// Reclaim row sees the freed pages). No VACUUM: later inserts reuse the freed pages, and
/// Reclaim space hands them back to the disk when the user wants that.
pub fn spawn_tool_output_compaction(host: &Host) {
    let host = Arc::clone(host);
    std::thread::spawn(move || {
        std::thread::sleep(BACKGROUND_COMPACT_DELAY);
        let mut conn = match db::open(&db::default_index_path()) {
            Ok(c) => c,
            Err(e) => {
                app_log!("[cleanup] tool output compaction: {e:#}");
                return;
            }
        };
        match cleanup::tool_output_compaction_pending(&conn) {
            Ok(true) => {}
            Ok(false) => return,
            Err(e) => {
                app_log!("[cleanup] tool output compaction: {e:#}");
                return;
            }
        }
        let mut on_disk = match cleanup::TranscriptsOnDisk::load(&conn) {
            Ok(t) => t,
            Err(e) => {
                app_log!("[cleanup] tool output compaction: {e:#}");
                return;
            }
        };
        app_log!(
            "[cleanup] tool output compaction: {} transcripts still on disk",
            on_disk.files()
        );
        let other_writer = |flags: &JobFlags| {
            flags.index.0.is_set() || flags.catch_up.0.is_set() || flags.embed.is_running()
        };
        let mut after = 0i64;
        for pass in 1.. {
            // Claim first, then check, as claim_rewrite does: a reindex or build that
            // started in between holds its own flag, and the claim is given back for it.
            let job = loop {
                if let Some(job) = JobGuard::try_acquire(&host.flags().compact.0) {
                    if !other_writer(host.flags()) {
                        break job;
                    }
                }
                std::thread::sleep(Duration::from_secs(2));
            };
            let clock = jobclock::JobClock::start();
            let r = match cleanup::compact_tool_output_on_disk(
                &mut conn,
                after,
                BACKGROUND_COMPACT_ROWS,
                &mut on_disk,
            ) {
                Ok(r) => r,
                // Busy behind another process's long write, or a real error: stop for this
                // launch; the next one resumes (the marker is not set).
                Err(e) => {
                    app_log!("[cleanup] tool output compaction pass {pass}: {e:#}");
                    return;
                }
            };
            drop(job);
            app_log!(
                "[cleanup] tool output compaction pass {pass}: {} rows, {:.1} MB trimmed in {}{}",
                r.rows,
                r.bytes_saved as f64 / 1_048_576.0,
                clock.thread_summary(),
                if r.done { " (done)" } else { "" }
            );
            if r.rows > 0 || r.done {
                emit(&*host, "index:updated", &());
            }
            if r.done {
                if let Err(e) = cleanup::mark_tool_output_compacted(&conn) {
                    app_log!("[cleanup] tool output compaction: {e:#}");
                }
                break;
            }
            after = r.last_id;
            std::thread::sleep(BACKGROUND_COMPACT_GAP);
        }
        db::shrink_memory(&conn);
    });
}

// ---- semantic index -----------------------------------------------------------------------

/// Embed pending recall facts (decisions / gotchas) if the embedding model is already on
/// this machine. A background fact embed never starts the 128 MB model download: that would
/// stall a "Remember" click or a distill for a minute with no explanation. Facts left
/// pending are embedded when the semantic-index build finishes, or at a later launch.
/// Blocking (inference); call from a worker thread.
pub fn embed_facts_if_model_ready(host: &dyn JobHost, tag: &str) {
    let embedder = host.embedder();
    if !(embedder.is_loaded() || embed::model_is_cached()) {
        return;
    }
    if let Err(e) = embed::embed_pending_facts(writer(host), embedder) {
        app_log!("[{tag}] embed facts: {e}");
    }
}

/// Who started a semantic-index build.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EmbedTrigger {
    /// A Build button (Search bar, palette, Settings): the whole backlog on the bulk pool.
    User,
    /// The watcher's top-up after agent activity (`embed_kick`): capped, on fewer threads.
    Background,
}

/// Most messages one background top-up embeds. About 46 h of one heavy user's sessions
/// came to 975 prose messages (about 13 s of CPU). A bigger backlog means a build that was
/// never finished, which is the Build button's job; the next top-up continues meanwhile.
const AUTO_EMBED_MAX_MESSAGES: i64 = 2_000;

/// Start a semantic-index build unless a writer job holds the lock or a build is running.
/// Returns whether it started. Emits `embed:progress`, `embed:error` (model unavailable /
/// systemic failure) and `embed:done`.
pub fn start_embed_build(host: &Host, trigger: EmbedTrigger) -> bool {
    let Some(running) = claim_embed_build(host.flags(), trigger) else {
        return false;
    };
    let Some(running) = take_embed_lock(host.flags(), &db::default_index_path(), running, trigger)
    else {
        return false;
    };
    let host = Arc::clone(host);
    std::thread::spawn(move || run_embed_build(&host, running, trigger));
    true
}

/// Attach `embed.lock` to a build this process claimed, or give the claim back (None) while
/// another process's build holds it: a terminal `cal embed` embedding the same backlog
/// alongside would only double the CPU and the write-lock turns. Quiet, like a build already
/// running here: a background top-up retries on a later tick, and a Build click's build is
/// the one `cal embed` is already running. Holding it makes `cal embed` refuse meanwhile.
fn take_embed_lock(
    flags: &JobFlags,
    index: &std::path::Path,
    running: JobGuard,
    trigger: EmbedTrigger,
) -> Option<JobGuard> {
    match locks::acquire(index, locks::EMBED_LOCK) {
        Ok(locks::Acquire::Acquired(lock)) => {
            flags
                .embed
                .lock_token
                .store(running.token, Ordering::SeqCst);
            Some(running.holding(lock))
        }
        Ok(locks::Acquire::Held(pid)) => {
            if trigger == EmbedTrigger::User {
                app_log!(
                    "[embed] another process is building the semantic index{}; it covers this request",
                    locks::pid_note(pid)
                );
            }
            flags.embed.user_requested.store(false, Ordering::SeqCst);
            None // `running` drops: released
        }
        // An unwritable lock file (another user's): build as before the lock existed.
        Err(e) => {
            app_log!("[embed] embed lock: {e}");
            Some(running)
        }
    }
}

/// Claim [`EmbedJob`] for a build, or None when it should not start (a build is already
/// running, or a reindex, the catch-up or a VACUUM is writing).
pub fn claim_embed_build(flags: &JobFlags, trigger: EmbedTrigger) -> Option<JobGuard> {
    // Note: a running distill does NOT block the embed build: the distill loop yields.
    let job = &flags.embed;
    let Some(running) = JobGuard::try_acquire(&job.running) else {
        if trigger == EmbedTrigger::User {
            job.user_requested.store(true, Ordering::SeqCst);
        }
        return None; // already running
    };
    job.cancel.store(false, Ordering::SeqCst);
    if trigger == EmbedTrigger::User {
        job.user_requested.store(true, Ordering::SeqCst);
    }
    // Claim first, then check, as the watcher's catch-up does: index_all and claim_rewrite
    // claim their flag and then look at EmbedJob, so each side sees the other and a build
    // never writes alongside a reindex it raced. (Checking first let both start.)
    if flags.index.0.is_set() || flags.catch_up.0.is_set() || flags.compact.0.is_set() {
        // A reindex or a VACUUM is writing: don't fight it for the lock (`running` drops).
        job.user_requested.store(false, Ordering::SeqCst);
        return None;
    }
    // Fresh counters: embedding_status falls back to a live count until the job's
    // snapshot below lands.
    job.done.store(0, Ordering::Relaxed);
    job.total.store(0, Ordering::Relaxed);
    Some(running)
}

/// The body of a build holding a [`claim_embed_build`] claim, on the calling thread (`cal
/// embed` runs it in the foreground). Ends with `embed:done`.
pub fn run_embed_build(host: &Host, running: JobGuard, trigger: EmbedTrigger) {
    let job = &host.flags().embed;
    let mut background = trigger == EmbedTrigger::Background;
    // An index from before 0036 gets its subagent partition first, so this build's vectors
    // go straight into it instead of being rewritten at the end (and a first build on a new
    // index, whose table is empty, pays nothing). Holding `running`, like the compaction
    // after the build: reindex, the catch-up and distill keep yielding to it.
    if !job.cancel.load(Ordering::SeqCst) && vec_partition_pending(&**host) {
        maintain_vectors(&**host);
    }
    loop {
        let clock = jobclock::JobClock::start();
        // The build's own model (see `Embedder::bulk_build`), dropped below with its
        // thread pool and the working memory its padded sub-batches grew. Queries keep
        // the shared 4-thread model, which the watcher's reaper unloads once idle. A
        // top-up's thread count follows its size.
        let bulk = host
            .embedder()
            .bulk_build(background.then(|| top_up_size(&**host)));
        let stats = run_embed_job(&**host, &bulk, background);
        if background && stats.model_failed {
            if let Ok(mut until) = job.auto_paused_until.lock() {
                *until = Some(std::time::Instant::now() + AUTO_EMBED_FAILURE_PAUSE);
            }
        }
        if stats.end == EmbedEnd::HandedOff {
            // A Build click mid top-up: finish the backlog on the full bulk pool, not
            // the background model's threads (63 against 88 chunks/s measured on 24
            // cores). The facts and the compaction run once, after the full pass.
            drop(bulk);
            app_log!(
                "[embed] background build: {} messages, then handed over to a user build",
                stats.messages
            );
            background = false;
            continue;
        }
        let cancelled = stats.end == EmbedEnd::Cancelled;
        // Embed any facts still waiting while the build's model is loaded, instead of
        // loading the shared one for them.
        if bulk.is_loaded() && !cancelled {
            if let Err(e) = embed::embed_pending_facts(writer(&**host), &bulk) {
                app_log!("[embed] embed facts: {e}");
            }
        }
        drop(bulk);
        let secs = clock.wall().as_secs_f64().max(0.001);
        app_log!(
            "[embed] {} build{}: {} messages, {} chunks ({} reused) in {} ({:.0} chunks/s)",
            if background { "background" } else { "user" },
            if cancelled {
                " (stopped for a reindex or rewrite)"
            } else {
                ""
            },
            stats.messages,
            stats.chunks,
            stats.reused,
            clock.process_summary(),
            stats.chunks as f64 / secs
        );
        // Still holding `running`: a vector compaction holds the write lock for seconds,
        // and reindex, the watcher catch-up and distill must keep yielding to it (the
        // status also keeps reporting a build, so Reindex stays disabled). Dedicated
        // connection, as in vacuum_db, so UI commands don't queue behind the shared
        // writer mutex meanwhile. Skipped when a reindex or rewrite is waiting for the
        // writer: the next build does it.
        if !job.cancel.load(Ordering::SeqCst) {
            maintain_vectors(&**host);
        }
        // A Build click in the tail above (facts, compaction) found `running` held and
        // only set the flag, after the batch loop had stopped reading it: run the full
        // build it asked for now, so its `embed:done` is not a capped top-up's.
        if background
            && !job.cancel.load(Ordering::SeqCst)
            && owes_full_build(
                stats.end,
                stats.reported,
                job.user_requested.load(Ordering::SeqCst),
            )
        {
            background = false;
            continue;
        }
        break;
    }
    // The build's batches went through the shared writer: hand back its page cache.
    shrink_writer(&**host);
    // A click between this and the release below is the one left unserved (it only sets
    // the flag); it makes the next top-up hand over to a full build at once.
    job.user_requested.store(false, Ordering::SeqCst);
    drop(running);
    emit(&**host, "embed:done", &());
    // Like index_all: a distill drain that yielded to the build resumes now.
    auto_distill_kick(host);
}

/// Release the shared writer's page cache (up to 64 MiB) after a burst of writes (an embed
/// build, a distill drain); it refills on demand. Skipped if the writer is busy.
pub fn shrink_writer(host: &dyn JobHost) {
    if let Some(db) = host.writer() {
        if let Some(conn) = db::try_lock_conn(&db.0) {
            db::shrink_memory(&conn);
        }
    }
}

/// The watcher's 60 s tick: unload the shared model once it has been idle for
/// `embed::APP_IDLE_UNLOAD` (the next query reloads it in about 150 ms). Also during a
/// semantic build: the build has its own model, and keeping the shared one through a build
/// of hours held two models' weights, arenas and thread pools the whole time.
pub fn embed_reap_tick(host: &dyn JobHost) {
    host.embedder().unload_if_idle();
}

/// Whether any embeddable message is still waiting for vectors. One probe of 0026's
/// covering (role, tool_name, embedded) index, so it is cheap on every watcher kick. Built
/// from `EMBEDDABLE_SQL` verbatim so the planner keeps matching that index.
fn messages_pending_embed_sql() -> String {
    format!(
        "SELECT EXISTS(SELECT 1 FROM messages m WHERE m.embedded = 0 AND {})",
        embed::EMBEDDABLE_SQL
    )
}

/// The watcher's auto-embed, after about a minute without new agent writes (and at the end
/// of the launch catch-up): top up the semantic index with the new messages, so semantic,
/// hybrid, related and Ask see them without a manual Build. Only for users who already
/// built a semantic index and have the model on disk (never starts the 128 MB download),
/// and only when something is pending.
///
/// The same kick partitions the vectors of an index from before 0036 (see
/// [`cleanup::rebuild_vec_chunks`]) when nothing is waiting to embed: a top-up does that
/// itself before its first batch. So it runs once, after the launch catch-up, on the app's
/// and `cal watch`'s watcher only (never a hook or the MCP server), and it needs no model.
pub fn embed_kick(host: &Host) -> EmbedKick {
    let job = &host.flags().embed;
    let Some(pool) = host.readers() else {
        return EmbedKick::NotNeeded;
    };
    // Runs on the watcher thread: r2d2's default `get` waits up to 30 s when UI reads hold
    // every reader, and file events, catch-up retries and the reaper tick would wait too.
    // Busy re-arms the timer instead.
    let (has_vectors, pending, partitioned) = match pool.0.get_timeout(Duration::from_millis(100)) {
        Ok(conn) => {
            let exists = |sql: &str| {
                conn.query_row(sql, [], |r| r.get::<_, bool>(0))
                    .unwrap_or(false)
            };
            (
                exists("SELECT EXISTS(SELECT 1 FROM vec_chunk_owner)"),
                exists(&messages_pending_embed_sql()),
                embed::vec_partitioned(&conn),
            )
        }
        Err(_) => return EmbedKick::Busy,
    };
    let top_up =
        has_vectors && pending && !paused_now(&job.auto_paused_until) && embed::model_is_cached();
    let started = match kick_plan(
        has_vectors,
        top_up,
        partitioned,
        paused_now(&job.rebuild_paused_until),
    ) {
        KickPlan::Nothing => return EmbedKick::NotNeeded,
        KickPlan::TopUp => start_embed_build(host, EmbedTrigger::Background),
        KickPlan::Rebuild => start_vec_rebuild(host),
    };
    if started {
        EmbedKick::Started
    } else {
        EmbedKick::Busy
    }
}

/// What [`embed_kick`] starts.
#[derive(Debug, PartialEq, Eq)]
enum KickPlan {
    /// A background top-up, which partitions an old-shape table before its first batch.
    TopUp,
    /// Only the 0036 partition ([`start_vec_rebuild`]).
    Rebuild,
    Nothing,
}

/// [`embed_kick`]'s decision, from what it read: whether the index has vectors, whether a
/// top-up can run (something pending, the model on disk, top-ups not paused), whether the
/// table is already partitioned and whether rebuilds are paused after a failure. The
/// partition must not wait for something to embed: an index with nothing pending would
/// otherwise keep its old shape (and its crowded default searches) indefinitely.
fn kick_plan(has_vectors: bool, top_up: bool, partitioned: bool, rebuild_paused: bool) -> KickPlan {
    if !has_vectors {
        KickPlan::Nothing
    } else if top_up {
        KickPlan::TopUp
    } else if partitioned || rebuild_paused {
        KickPlan::Nothing
    } else {
        KickPlan::Rebuild
    }
}

/// Whether a pause set until `until` is still on.
fn paused_now(until: &Mutex<Option<std::time::Instant>>) -> bool {
    until
        .lock()
        .is_ok_and(|until| until.is_some_and(|t| std::time::Instant::now() < t))
}

/// Partition the vectors on a thread of its own, holding [`EmbedJob`] as a build does (so
/// reindex, the catch-up, distill and the rewrites keep out of its write lock), then send
/// `embed:done` so a status poll that saw it running settles. A Build click that landed
/// meanwhile only set `user_requested`: it gets its full build now.
///
/// It holds `embed.lock` like a build ([`take_embed_lock`]): a terminal `cal embed` then
/// refuses instead of queueing behind the rebuild's write lock, and the build a Build click
/// hands over to below is one other processes can see. Held by another process means that
/// process's build partitions the table before its first batch, so this one steps aside.
fn start_vec_rebuild(host: &Host) -> bool {
    let Some(running) = claim_embed_build(host.flags(), EmbedTrigger::Background) else {
        return false;
    };
    let Some(running) = take_embed_lock(
        host.flags(),
        &db::default_index_path(),
        running,
        EmbedTrigger::Background,
    ) else {
        return false;
    };
    let host = Arc::clone(host);
    std::thread::spawn(move || {
        maintain_vectors(&*host);
        if host.flags().embed.user_requested.load(Ordering::SeqCst) {
            run_embed_build(&host, running, EmbedTrigger::User);
            return;
        }
        drop(running);
        emit(&*host, "embed:done", &());
        auto_distill_kick(&host);
    });
    true
}

/// About how many messages a background top-up will embed: those waiting, up to its cap
/// ([`AUTO_EMBED_MAX_MESSAGES`]). A bounded walk of 0026's covering index (as
/// `messages_pending_embed_sql`), so it never counts a whole backlog.
fn top_up_size(host: &dyn JobHost) -> i64 {
    let sql = top_up_size_sql();
    host.writer()
        .map(|db| db::lock_conn(&db.0))
        .and_then(|conn| {
            conn.query_row(&sql, [AUTO_EMBED_MAX_MESSAGES], |r| r.get(0))
                .ok()
        })
        .unwrap_or(AUTO_EMBED_MAX_MESSAGES)
}

fn top_up_size_sql() -> String {
    format!(
        "SELECT COUNT(*) FROM (SELECT 1 FROM messages m WHERE m.embedded = 0 AND {} LIMIT ?1)",
        embed::EMBEDDABLE_SQL
    )
}

/// Whether the vector table still has its pre-0036 shape. Through the shared writer (a
/// cached schema lookup, no vector read).
fn vec_partition_pending(host: &dyn JobHost) -> bool {
    host.writer()
        .map(|db| db::lock_conn(&db.0))
        .is_some_and(|conn| !embed::vec_partitioned(&conn))
}

/// Vector upkeep on a dedicated connection, as `vacuum_db` does, so UI commands don't queue
/// behind the shared writer mutex meanwhile: owner rows for any vectors written without one,
/// then [`cleanup::rebuild_vec_chunks`] (the 0036 partition once, a compaction when deletes
/// left the table mostly holes). The caller holds [`EmbedJob`]. After a rebuild failed
/// (too little free disk), the next ones wait out [`VEC_REBUILD_FAILURE_PAUSE`]: every
/// top-up would only repeat the failure and its log line.
fn maintain_vectors(host: &dyn JobHost) {
    maintain_vectors_at(
        &db::default_index_path(),
        &host.flags().embed.rebuild_paused_until,
    );
}

/// [`maintain_vectors`] on the index at `path`, pausing rebuilds through `paused_until`.
fn maintain_vectors_at(path: &std::path::Path, paused_until: &Mutex<Option<std::time::Instant>>) {
    let maintenance = jobclock::JobClock::start();
    let rebuild = !paused_now(paused_until);
    match db::open(path) {
        Ok(mut conn) => {
            if !finish_embed_build(&mut conn, rebuild) {
                if let Ok(mut until) = paused_until.lock() {
                    *until = Some(std::time::Instant::now() + VEC_REBUILD_FAILURE_PAUSE);
                }
            }
        }
        Err(e) => app_log!("[embed] vector maintenance: {e:#}"),
    }
    if maintenance.wall() >= Duration::from_secs(1) {
        app_log!(
            "[embed] vector maintenance: {}",
            maintenance.thread_summary()
        );
    }
}

/// How long the watcher's vector rebuild waits after one failed (most likely too little
/// free disk: it needs about three times the vector table, see
/// `cleanup::REBUILD_SPACE_FACTOR`).
const VEC_REBUILD_FAILURE_PAUSE: Duration = Duration::from_secs(60 * 60);

/// What `embed_kick` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedKick {
    Started,
    /// No semantic index yet, no model on disk, or nothing waiting for vectors.
    NotNeeded,
    /// A reindex, catch-up, VACUUM or build holds the writer: the watcher retries later.
    Busy,
}

/// Vector maintenance on its own connection (see [`maintain_vectors`]), with the rebuild
/// when `rebuild`. Returns false when the rebuild failed.
fn finish_embed_build(conn: &mut rusqlite::Connection, rebuild: bool) -> bool {
    // Safety net for #27: owner rows for any vectors written without one.
    if let Err(e) = cleanup::reconcile_vec_owners(conn) {
        app_log!("[embed] owner reconcile: {e}");
    }
    // The 0036 partition once (default searches stop losing main-thread hits behind
    // subagent vectors), then a compaction whenever deletes (0029's tool-vector purge)
    // have left the table mostly holes: ~35% faster KNN. A schema lookup and two small
    // COUNTs, a no-op, when neither is due.
    let mut rebuilt = false;
    let ok = match rebuild.then(|| cleanup::rebuild_vec_chunks(conn)) {
        None => true,
        Some(Ok(Some(c))) => {
            rebuilt = true;
            if c.partitioned {
                // The one-time 0036 rebuild rewrote about twice the vector table, which the
                // pre-upgrade snapshot (an APFS clone) now holds alone: it has served.
                if let Some(path) = conn.path().filter(|p| !p.is_empty()) {
                    crate::backup::after_partition(conn, std::path::Path::new(path));
                }
            }
            let held = Duration::from_millis(c.held_ms);
            app_log!(
                "[embed] {} {} vectors into {} chunks (was {}), write lock held {:.1} s{}",
                if c.partitioned {
                    "partitioned"
                } else {
                    "compacted"
                },
                c.vectors,
                c.chunks_after,
                c.chunks_before,
                held.as_secs_f64(),
                if held >= db::BUSY_TIMEOUT {
                    ", past the busy timeout: a write on another app connection meanwhile \
                     may have failed with \"database is locked\""
                } else {
                    ""
                }
            );
            true
        }
        Some(Ok(None)) => true,
        Some(Err(e)) => {
            app_log!("[embed] vector rebuild: {e:#}");
            false
        }
    };
    // Fold the WAL back after the build's many small writes (PASSIVE never blocks).
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");
    if rebuilt || wal_bytes(conn) > WAL_TRUNCATE_OVER {
        truncate_wal(conn);
    }
    ok
}

/// A WAL file over this size is truncated after a build ([`truncate_wal`]). A checkpoint
/// copies the WAL back into the index but leaves the file at its peak size, and normal
/// writes keep it near 4 MB (SQLite checkpoints every 1,000 pages).
const WAL_TRUNCATE_OVER: u64 = 64 << 20;

/// How long [`truncate_wal`] waits for readers. A TRUNCATE checkpoint holds off new
/// writers while it waits, so not the connection's usual 15 s.
const WAL_TRUNCATE_WAIT: Duration = Duration::from_secs(1);

/// The size of the index's WAL file, 0 when there is none (or no file at all).
fn wal_bytes(conn: &rusqlite::Connection) -> u64 {
    conn.path()
        .filter(|p| !p.is_empty())
        .and_then(|p| std::fs::metadata(format!("{p}-wal")).ok())
        .map_or(0, |m| m.len())
}

/// Give a large WAL's disk back. The vector rebuild writes its scratch copy and the new
/// table through the WAL: 1,128 MiB on a 324k-vector index, which a PASSIVE checkpoint
/// never shrinks, so it stayed on disk until the app quit. Best effort: a reader on an
/// older snapshot stops it, and a later build (the file is still large) tries again.
fn truncate_wal(conn: &rusqlite::Connection) {
    let _ = conn.busy_timeout(WAL_TRUNCATE_WAIT);
    let done = cleanup::checkpoint_truncate(conn).unwrap_or(false);
    let _ = conn.busy_timeout(db::BUSY_TIMEOUT);
    if !done {
        app_log!("[embed] WAL truncate held back by a reader; a later build retries");
    }
}

/// How one build pass ended.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum EmbedEnd {
    /// Nothing is left to embed.
    Drained,
    /// A background top-up embedded its [`AUTO_EMBED_MAX_MESSAGES`].
    Capped,
    /// A Build click landed during a background top-up: continue as a full build.
    HandedOff,
    /// A reindex or an index rewrite asked the background top-up to stop.
    Cancelled,
    /// The model could not load or went away, or the build had to stop (a lock standoff,
    /// every message failing inference).
    #[default]
    Stopped,
}

/// What one build pass embedded, for its log line, and how it ended.
#[derive(Default)]
struct EmbedRunStats {
    messages: i64,
    chunks: usize,
    /// Chunks whose vector was copied, not inferred (a repeated text).
    reused: usize,
    end: EmbedEnd,
    /// The build stopped because the model could not load or went away.
    model_failed: bool,
    /// It sent `embed:error`, so a Build click already learned why it stopped.
    reported: bool,
}

/// Whether a background pass that ended with `end` owes a full build before the build sends
/// `embed:done`. A Build click during the pass hands it over (`HandedOff`). One in the pass's
/// tail (facts, the vector compaction) found the build running and only set
/// `user_requested`, after the batch loop had stopped reading it, so the following
/// `embed:done` read "Semantic index is up to date" over a capped or failed top-up. Not after
/// a drained pass (that toast is true) or a cancel (a reindex or rewrite needs the writer),
/// nor when the click already got this pass's `embed:error`.
fn owes_full_build(end: EmbedEnd, reported: bool, user_requested: bool) -> bool {
    match end {
        EmbedEnd::HandedOff => true,
        EmbedEnd::Drained | EmbedEnd::Cancelled => false,
        EmbedEnd::Capped | EmbedEnd::Stopped => user_requested && !reported,
    }
}

/// The body of the embedding job (see `start_embed_build`), on the build's own `embedder`.
/// Returns when the queue is empty, a background pass is done (its cap, a Build click, a
/// cancel), the model is unavailable, or the build had to stop; the caller does the cleanup.
///
/// Only a build the user asked for reports `embed:error` to the UI (and its telemetry): a
/// background top-up that fails is logged, and the next Build click shows the reason.
fn run_embed_job(
    host: &dyn JobHost,
    embedder: &embed::Embedder,
    background: bool,
) -> EmbedRunStats {
    // Smaller batches keep each locked write + each inference short, so the UI
    // (and ambient-recall) stay snappy while the job runs in the background. 128 also
    // widens embed::infer_bulk's length-sort window: +9-15% chunks/s over 64, for a
    // locked store_batch write about twice as long.
    const BATCH: usize = 128;
    let limit = background.then_some(AUTO_EMBED_MAX_MESSAGES);
    let db = writer(host);
    let job = &host.flags().embed;
    let mut stats = EmbedRunStats::default();
    let reported = std::cell::Cell::new(false);
    let report = |msg: String| {
        if !background || job.user_requested.load(Ordering::SeqCst) {
            emit(host, "embed:error", &msg);
            reported.set(true);
        }
    };
    // Checked before the model loads and before each batch: a background top-up stops for
    // a reindex or a rewrite (request_embed_yield), and hands over to a full build when a
    // Build click lands.
    let interrupted = || -> Option<EmbedEnd> {
        if !background {
            return None;
        }
        if job.cancel.load(Ordering::SeqCst) {
            report(
                "The semantic build stopped for a reindex or an index rewrite. Click Build \
                 when that finishes."
                    .to_string(),
            );
            return Some(EmbedEnd::Cancelled);
        }
        job.user_requested
            .load(Ordering::SeqCst)
            .then_some(EmbedEnd::HandedOff)
    };
    if let Some(end) = interrupted() {
        stats.end = end;
        stats.reported = reported.get();
        return stats;
    }

    // Load (or first-download) the model BEFORE claiming any work. A load failure
    // (offline first run, proxy, full disk, unwritable cache dir) must stop the job with
    // every message still pending; the old per-batch error path mistook it for bad
    // batches and marked the whole corpus embedded with no vectors.
    if let Err(e) = embedder.ensure_loaded() {
        app_log!("[embed] model unavailable, build stopped: {e}");
        report(e.to_string());
        stats.model_failed = true;
        stats.reported = reported.get();
        return stats;
    }
    // The model works again (a repaired or re-downloaded cache): lift a pause a failed
    // top-up set, whichever build got here, instead of leaving top-ups off for the hour.
    if let Ok(mut until) = job.auto_paused_until.lock() {
        *until = None;
    }

    // Snapshot totals ONCE; then increment a running counter per batch so progress
    // events carry counts without re-running COUNT(*) scans under the lock.
    let (mut done, total) = embed::embed_progress(&db::lock_conn(&db.0)).unwrap_or((0, 0));
    job.done.store(done, Ordering::Relaxed);
    job.total.store(total, Ordering::Relaxed);
    let progress = |done: i64| {
        job.done.store(done, Ordering::Relaxed);
        emit(host, "embed:progress", &EmbedProgressEvent { done, total });
    };

    // Consecutive batches we couldn't persist because another writer held the lock.
    // We re-queue instead of aborting (the manual Reindex is now mutually exclusive,
    // but the file watcher still indexes incrementally), bailing only after a long
    // standoff so a truly stuck writer can't spin us forever.
    let mut deferrals = 0u32;
    // Consecutive multi-message batches where EVERY message failed inference on its own:
    // systemic, not one bad input, so stop instead of marking the corpus.
    let mut all_failed_streak = 0u32;
    // Chunk texts this pass stored: a repeat copies the stored vector instead of running
    // the model (embed::ChunkMemo).
    let mut memo = embed::ChunkMemo::default();
    loop {
        if let Some(end) = interrupted() {
            stats.end = end;
            break;
        }
        // 1. Locked, fast: claim the next batch of pending messages. Each stop below also
        //    reports, so a Build click (and `cal embed`'s exit code) learns the build did
        //    not finish instead of reading the `embed:done` that follows as "up to date".
        let rows = {
            let conn = db::lock_conn(&db.0);
            match embed::pending_batch(&conn, BATCH) {
                Ok(r) => r,
                Err(e) => {
                    app_log!("[embed] {e}");
                    report(format!("the semantic build stopped: {e}"));
                    break;
                }
            }
        };
        if rows.is_empty() {
            stats.end = EmbedEnd::Drained;
            break;
        }
        // 2. UNLOCKED: inference, except for chunk texts this pass already stored, whose
        //    vectors are read back first (point lookups, briefly locked). embed_rows retries
        //    a failing batch one message at a time, so only inputs that fail on their own
        //    land in `failed`; store_batch below marks them embedded without vectors (FTS
        //    still finds them).
        let hits = memo.reusable(&rows);
        let known = if hits.is_empty() {
            std::collections::HashMap::new()
        } else {
            memo.read_vectors(&db::lock_conn(&db.0), &hits)
        };
        let batch = match embed::embed_rows_with(embedder, &rows, &known) {
            Ok(b) => b,
            Err(e) => {
                // The model itself became unavailable: stop and leave this batch pending.
                app_log!("[embed] model unavailable, build stopped: {e}");
                report(e.to_string());
                stats.model_failed = true;
                break;
            }
        };
        if rows.len() > 1 && batch.failed.len() == rows.len() {
            all_failed_streak += 1;
            if all_failed_streak >= 3 {
                let msg = "embedding failed for every message; build stopped (nothing was marked)";
                app_log!("[embed] {msg}");
                report(msg.to_string());
                break;
            }
            std::thread::sleep(Duration::from_secs(1));
            continue; // retry the same batch; nothing was marked
        }
        all_failed_streak = 0;
        if !batch.failed.is_empty() {
            app_log!(
                "[embed] skipping {} message(s) that failed inference",
                batch.failed.len()
            );
        }
        // 3. Locked, fast: persist the vectors + mark the messages embedded.
        //    Another writer (the MCP server, `cal`, a second app instance) can
        //    hold the write lock; retry "database is locked" instead of aborting
        //    the whole job, releasing the mutex between tries so the UI breathes.
        let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
        let mut stored = false;
        for _ in 0..10 {
            let res = {
                let mut conn = db::lock_conn(&db.0);
                embed::store_batch(&mut conn, &ids, &batch.owners, &batch.vecs)
            };
            match res {
                Ok(rowids) => {
                    memo.remember(&batch.keys, &rowids);
                    stored = true;
                    break;
                }
                Err(e) if db::is_busy(&e) => {
                    std::thread::sleep(Duration::from_millis(300));
                }
                Err(e) => {
                    app_log!("[embed] {e}");
                    break;
                }
            }
        }
        if !stored {
            deferrals += 1;
            if deferrals > 40 {
                app_log!("[embed] giving up: the database stayed locked too long");
                report(
                    "the semantic build stopped: another process kept the index locked too \
                     long; try again when it finishes"
                        .to_string(),
                );
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
            continue; // re-queue this batch; don't kill the whole job
        }
        deferrals = 0;
        done += rows.len() as i64;
        progress(done);
        stats.messages += rows.len() as i64;
        stats.chunks += batch.owners.len();
        stats.reused += batch.reused;
        if limit.is_some_and(|l| stats.messages >= l) {
            stats.end = EmbedEnd::Capped;
            break;
        }
    }
    stats.reported = reported.get();
    stats
}

// ---- harvest and the ledger ---------------------------------------------------------------

/// Import Claude Code auto memory on `conn` (the watcher's writer: after its launch
/// catch-up, then on its minute tick). Cheap when no topic file changed: a stat per file
/// and no write. A busy index waits for the next tick.
pub fn harvest_claude_memory_tick(host: &Host, conn: &rusqlite::Connection) {
    let now = chrono::Utc::now().timestamp();
    match knowledge::harvest::harvest_claude_memory(conn, now) {
        Ok(report) => harvest_landed(host, report),
        Err(e) if db::is_busy(&e) => {}
        Err(e) => app_log!("[harvest] claude memory: {e:#}"),
    }
}

/// Fold the value ledger's past days into `ledger.daily` on `conn` (the watcher's writer, at
/// launch and on its minute tick). A busy index waits for the next tick.
pub fn ledger_rollup_tick(conn: &rusqlite::Connection) {
    match ledger::rollup(conn) {
        Ok(_) => {}
        Err(e) if db::is_busy(&e) => {}
        Err(e) => app_log!("[ledger] rollup: {e:#}"),
    }
}

/// After a harvest pass changed facts: log it, have open views refetch project memory (the
/// null `index:updated` payload is main.tsx's housekeeping contract), and embed the new
/// facts for recall when the model is on disk. The embed runs on its own thread because it
/// takes the writer and may load the model, and callers are the watcher loop and an async
/// command.
pub fn harvest_landed(host: &Host, report: knowledge::harvest::HarvestReport) {
    if !report.changed() {
        return;
    }
    app_log!(
        "[harvest] claude memory: {} imported, {} removed",
        report.imported,
        report.removed
    );
    emit(&**host, "index:updated", &());
    if report.imported > 0 {
        let host = Arc::clone(host);
        std::thread::spawn(move || embed_facts_if_model_ready(&*host, "harvest"));
    }
}

// ---- the watcher's graph upkeep: commit links per batch, issue clusters per minute -------

/// The threads an index batch just wrote, the most recently active sessions first: the rows
/// with `last_indexed_at` at or after the batch start, off 0034's index, sorted by
/// `updated_at` (a sort over the batch's rows only; the COALESCE keeps SQLite from walking
/// idx_threads_updated over every thread instead). Ordering by `last_indexed_at` kept the
/// threads written last in the pass, so after a launch catch-up the sources scanned after
/// Claude Code took every slot. Subagent transcripts are skipped, as `gitlink` does; the `+`
/// keeps SQLite off idx_threads_subagent, an equality it otherwise prefers to the range.
pub(crate) const BATCH_THREADS_SQL: &str =
    "SELECT id, COALESCE(project_path, ''), COALESCE(project_key, project_path, '')
     FROM threads WHERE last_indexed_at >= ?1 AND +is_subagent = 0
     ORDER BY COALESCE(updated_at, 0) DESC, id DESC LIMIT ?2";

/// Threads one batch queues for linking. A batch during an active session writes one or
/// two; a launch catch-up can write hundreds, and only the newest are worth a `git log` now.
/// The rest link only while Today lists them (it links the sessions in its own window) or on
/// a project's whole-repo scan (Scan git, `cal commits`): nothing links older history on its
/// own, since no linking rule measured precise enough to spread over it
/// (`gitlink::TUNING`).
const BATCH_LINK_MAX_THREADS: i64 = 50;
/// Least time between two links of the same checkout: an active session's transcript lands
/// in a batch every few seconds, and `git log` over its window each time would be waste.
pub const COMMIT_LINK_EVERY: Duration = Duration::from_secs(60);
/// Tries before the linker drops a checkout whose `git log` keeps outrunning the budget or
/// meeting a busy index (a resumed months-old session in a big repo asks for months of log).
/// With the doubling backoff that is about half an hour; its sessions still link while Today
/// lists them or on a project's whole-repo scan.
const LINK_MAX_ATTEMPTS: u32 = 5;
/// Deadline for the two `git` calls that resolve a checkout's portable id. They run on the
/// watcher's thread, and a git hung on a sleeping network mount or a slow fsmonitor hook
/// must not hold indexing; a probe that runs out is retried on a later pass.
const PORTABLE_PROBE_DEADLINE: Duration = Duration::from_secs(3);

/// One checkout's queued work.
#[derive(Default)]
struct Queued {
    /// The project keys of its sessions, each resolved to a portable id once it links.
    keys: std::collections::BTreeSet<String>,
    threads: std::collections::BTreeSet<i64>,
    /// Passes in a row where this checkout was tried and wrote nothing.
    failures: u32,
    /// Not before this, after a failure.
    retry_at: Option<std::time::Instant>,
}

/// Keeps `thread_commits` current as sessions land, without the GUI: the watcher (and `cal
/// index`) queues the threads each index batch wrote, and links the checkouts that are due
/// (each at most once per [`COMMIT_LINK_EVERY`]) under Today's 2 s budget in total
/// ([`crate::today::COMMIT_BUDGET`]). A checkout that was tried and wrote nothing (git ran
/// past the deadline, or the index stayed locked) stays queued and is retried with a
/// doubling backoff, after the checkouts that have not failed, and dropped after
/// [`LINK_MAX_ATTEMPTS`].
#[derive(Default)]
pub struct CommitLinker {
    /// Checkout dir (canonical) -> its queued work.
    pending: std::collections::BTreeMap<String, Queued>,
    last_run: std::collections::HashMap<String, std::time::Instant>,
    /// Project keys of linked checkouts still to resolve to a portable id.
    portable: std::collections::BTreeSet<String>,
}

/// The checkout a session's commits land in: the nearest directory at or above its own cwd
/// that has a `.git` (a linked worktree's own root, whose branch the main checkout's log
/// does not walk), else the project key's checkout (the git root `canonical_project` found
/// when the cwd existed). A monorepo session started in `repo/apps/web` used to be skipped,
/// since only the cwd itself was checked. The walk stops below the home directory, as
/// `canonical_project` does, so a dotfiles `~/.git` never claims every session under home.
/// Canonicalized, so `/tmp/x`, `/private/tmp/x` and `x/` share one queue entry and one
/// once-a-minute slot.
pub(crate) fn checkout_for(path: &str, key: &str) -> Option<String> {
    let home = dirs::home_dir();
    let start = if path.is_empty() { key } else { path };
    let start = std::path::Path::new(start);
    let found = start
        .ancestors()
        .take_while(|dir| home.as_deref() != Some(*dir) || *dir == start)
        .find(|dir| !dir.as_os_str().is_empty() && dir.join(".git").exists())
        .map(|dir| dir.to_string_lossy().into_owned())
        .or_else(|| crate::today::git_checkout(key).map(str::to_string))?;
    Some(
        std::fs::canonicalize(&found)
            .map(|c| crate::gitlink::plain_path(&c.to_string_lossy()))
            .unwrap_or(found),
    )
}

/// The portable-id probe with a deadline: `git rev-parse --show-prefix` (where the key sits
/// below the toplevel) and `git remote get-url origin`, each killed at `deadline`. None when
/// `dir` is not a checkout here or a call ran out of time; a checkout with no `origin` is a
/// probe with no remote.
fn probe_remote_until(
    dir: &str,
    deadline: std::time::Instant,
) -> Option<knowledge::identity::RemoteProbe> {
    if !std::path::Path::new(&crate::gitlink::plain_path(dir)).is_dir() {
        return None;
    }
    let prefix =
        crate::gitlink::run_git_until(dir, &["rev-parse", "--show-prefix"], deadline).ok()??;
    let remote =
        match crate::gitlink::run_git_until(dir, &["remote", "get-url", "origin"], deadline) {
            Ok(Some(url)) => Some(url).filter(|u| !u.is_empty()),
            Ok(None) => return None,
            Err(_) => None, // no `origin`
        };
    Some(knowledge::identity::RemoteProbe {
        remote,
        subpath: prefix.trim_matches('/').to_string(),
    })
}

impl CommitLinker {
    /// Queue the threads written since `since` (unix secs, taken before the batch ran) whose
    /// session ran in a git checkout. Returns how many were queued.
    pub fn queue_since(
        &mut self,
        conn: &rusqlite::Connection,
        since: i64,
    ) -> anyhow::Result<usize> {
        let mut stmt = conn.prepare_cached(BATCH_THREADS_SQL)?;
        let rows = stmt
            .query_map(rusqlite::params![since, BATCH_LINK_MAX_THREADS], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut queued = 0;
        for (id, path, key) in rows {
            let Some(repo) = checkout_for(&path, &key) else {
                continue;
            };
            let entry = self.pending.entry(repo).or_default();
            if !key.is_empty() {
                entry.keys.insert(key);
            }
            queued += usize::from(entry.threads.insert(id));
        }
        Ok(queued)
    }

    /// Whether any queued checkout is due now, or a linked one still needs its portable id.
    pub fn has_due(&self, now: std::time::Instant) -> bool {
        !self.portable.is_empty() || self.pending.keys().any(|repo| self.is_due(repo, now))
    }

    fn is_due(&self, repo: &str, now: std::time::Instant) -> bool {
        if let Some(at) = self.pending.get(repo).and_then(|q| q.retry_at) {
            return now >= at;
        }
        self.last_run
            .get(repo)
            .is_none_or(|at| now.saturating_duration_since(*at) >= COMMIT_LINK_EVERY)
    }

    /// Link the due checkouts on `conn` (the watcher's writer) within `budget`, then cache
    /// linked checkouts' portable project ids while time is left. A checkout skipped for
    /// time stays queued for the next pass; one that was tried and wrote nothing backs off.
    /// Returns how many checkouts were linked.
    pub fn link_due(
        &mut self,
        conn: &rusqlite::Connection,
        budget: Duration,
    ) -> anyhow::Result<usize> {
        self.link_due_with(
            conn,
            budget,
            crate::gitlink::run_git_log_until,
            probe_remote_until,
        )
    }

    /// [`link_due`](Self::link_due) with the `git log` runner and the remote probe injected
    /// (tests pass fakes).
    pub(crate) fn link_due_with(
        &mut self,
        conn: &rusqlite::Connection,
        budget: Duration,
        git_log: impl FnMut(&str, i64, std::time::Instant) -> anyhow::Result<Option<String>>,
        mut probe: impl FnMut(&str, std::time::Instant) -> Option<knowledge::identity::RemoteProbe>,
    ) -> anyhow::Result<usize> {
        use crate::today::LinkStop;
        let started = std::time::Instant::now();
        // link_checkouts_with shortens the busy wait to the budget left. This connection is
        // the watcher's writer for the rest of the process, whose wait may be the sidecar's
        // 60 s rather than db::BUSY_TIMEOUT, so the value it had is what comes back.
        let busy_before: i64 =
            conn.pragma_query_value(None, "busy_timeout", |r| r.get::<_, i64>(0))?;
        let restore =
            || conn.busy_timeout(Duration::from_millis(busy_before.max(0).unsigned_abs()));

        let mut due: Vec<String> = self
            .pending
            .keys()
            .filter(|repo| self.is_due(repo, started))
            .cloned()
            .collect();
        // Checkouts that have not failed go first: a checkout whose log always runs out of
        // time would otherwise spend the whole budget on every pass ahead of the rest.
        due.sort_by_key(|repo| self.pending[repo].failures);
        let mut linked = 0;
        if !due.is_empty() {
            let batch: Vec<(&str, Vec<i64>)> = due
                .iter()
                .map(|repo| {
                    let ids = self.pending[repo].threads.iter().copied().collect();
                    (repo.as_str(), ids)
                })
                .collect();
            let result = crate::today::link_checkouts_with(conn, &batch, budget, git_log);
            restore()?;
            let (n, stop) = result?;
            linked = n;
            let now = std::time::Instant::now();
            for repo in &due[..n] {
                self.last_run.insert(repo.clone(), now);
                if let Some(q) = self.pending.remove(repo) {
                    self.portable.extend(q.keys);
                }
            }
            if stop == LinkStop::Failed {
                let repo = &due[n];
                let q = self
                    .pending
                    .get_mut(repo)
                    .expect("a due checkout is queued");
                q.failures += 1;
                if q.failures >= LINK_MAX_ATTEMPTS {
                    app_log!(
                        "[gitlink] {repo}: no link after {} tries (git log too slow or the index \
                         busy); its sessions link while Today lists them or on a project \
                         scan",
                        q.failures
                    );
                    self.pending.remove(repo);
                    self.last_run.insert(repo.clone(), now);
                } else {
                    q.retry_at = Some(now + COMMIT_LINK_EVERY * (1u32 << (q.failures - 1)));
                }
            }
        }

        // Portable ids while the budget lasts. Cheap once cached (one row read); a probe
        // runs git at most once a day per key, under its own deadline, and only one probe
        // runs per pass so the pass stays near its budget. The cache write waits for the lock
        // no longer than the budget left.
        let mut probed = false;
        while !probed && started.elapsed() < budget {
            let Some(key) = self.portable.pop_first() else {
                break;
            };
            conn.busy_timeout(budget.saturating_sub(started.elapsed()))?;
            let now = chrono::Utc::now().timestamp();
            let _ = knowledge::identity::portable_project_id_with(conn, &key, now, false, |k| {
                probed = true;
                probe(k, std::time::Instant::now() + PORTABLE_PROBE_DEADLINE)
            });
            restore()?;
        }
        Ok(linked)
    }
}

/// After an index batch: queue what it wrote and link the due checkouts. Never fails the
/// batch; a busy index or a slow git leaves the work queued for the next batch or tick.
pub fn link_commits_after_batch(
    linker: &mut CommitLinker,
    conn: &rusqlite::Connection,
    since: i64,
) {
    if let Err(e) = linker.queue_since(conn, since) {
        app_log!("[gitlink] queue: {e:#}");
        return;
    }
    link_commits_tick(linker, conn);
}

/// Link whatever is queued and due (the watcher's minute tick flushes checkouts that were
/// linked too recently when their batch landed, and retries those that backed off).
pub fn link_commits_tick(linker: &mut CommitLinker, conn: &rusqlite::Connection) {
    if !linker.has_due(std::time::Instant::now()) {
        return;
    }
    let clock = jobclock::JobClock::start();
    match linker.link_due(conn, crate::today::COMMIT_BUDGET) {
        // Logged only when slow: a pass usually links one or two checkouts at about 35 ms
        // each warm (4 checkouts took 0.14 s warm and 0.81 s cold on a 21k-thread index), so
        // a line here means git or the write lock was slow.
        Ok(n) if clock.wall() >= Duration::from_millis(500) => {
            app_log!(
                "[gitlink] linked {n} checkout(s) in {}",
                clock.thread_summary()
            )
        }
        Ok(_) => {}
        Err(e) if db::is_busy(&e) => {}
        Err(e) => app_log!("[gitlink] link: {e:#}"),
    }
}

/// Least time between two `issue_clusters` refreshes while sessions keep writing: about 0.7 s
/// of mining each, and readers trust a refresh for [`crate::issues::CLUSTERS_FRESH_SECS`].
pub const CLUSTER_REFRESH_EVERY: Duration = Duration::from_secs(5 * 60);
/// A refresh at least this slow gets a log line: a warm one takes about 0.7 s and a cold
/// one 3.7 s on a 21k-thread index, and a line per refresh was up to 288 a day.
const CLUSTER_REFRESH_SLOW: Duration = Duration::from_secs(2);

/// Whether an `issue_clusters` refresh is due on `conn` now (see [`crate::issues::clusters_due`]).
pub fn clusters_due(conn: &rusqlite::Connection) -> bool {
    crate::issues::clusters_due(
        conn,
        chrono::Utc::now().timestamp(),
        CLUSTER_REFRESH_EVERY.as_secs() as i64,
    )
}

/// One refresh in flight per process.
static CLUSTERS_RUNNING: AtomicBool = AtomicBool::new(false);

/// The watcher's minute tick: when a refresh is due ([`clusters_due`] on the watcher's
/// connection, a few point reads) and no job holding the writer is running, refresh
/// `issue_clusters` on a thread of its own: the mining (3.7 s cold) on a read-only
/// connection, so neither the watcher thread nor its writer waits on it, then the short
/// write through the host's writer. `last_try` spaces the attempts by
/// [`CLUSTER_REFRESH_EVERY`], so a refresh that keeps failing is not retried every minute.
pub fn issue_clusters_kick(
    host: &Host,
    conn: &rusqlite::Connection,
    last_try: &mut Option<std::time::Instant>,
) {
    if last_try.is_some_and(|at| at.elapsed() < CLUSTER_REFRESH_EVERY)
        || writer_job_running(host.flags())
        || CLUSTERS_RUNNING.load(Ordering::SeqCst)
        || !clusters_due(conn)
    {
        return;
    }
    *last_try = Some(std::time::Instant::now());
    if CLUSTERS_RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    let host = Arc::clone(host);
    std::thread::spawn(move || {
        struct Release;
        impl Drop for Release {
            fn drop(&mut self) {
                CLUSTERS_RUNNING.store(false, Ordering::SeqCst);
            }
        }
        let _release = Release;
        let clock = jobclock::JobClock::start();
        let result = db::open_readonly(&db::default_index_path())
            .and_then(|reader| refresh_clusters_off_writer(&reader, writer(&*host), host.flags()));
        log_cluster_refresh(result, &clock);
    });
}

/// Mine on `reader`, then store through `writer` unless a job that holds the writer started
/// meanwhile (a reindex, a catch-up, a semantic build, a rewrite): then it yields, writing
/// nothing, and a later tick tries again. Some(clusters stored), or None when it yielded.
pub(crate) fn refresh_clusters_off_writer(
    reader: &rusqlite::Connection,
    writer: &db::Db,
    flags: &JobFlags,
) -> anyhow::Result<Option<usize>> {
    let now = chrono::Utc::now().timestamp();
    let clusters = crate::issues::mine_for_refresh(reader, now)?;
    if writer_job_running(flags) {
        return Ok(None);
    }
    let conn = lock_anyhow(writer)?;
    Ok(Some(crate::issues::store_clusters(&conn, &clusters, now)?))
}

/// The log line for a refresh: only a slow or failed one (a busy index stays quiet).
pub(crate) fn log_cluster_refresh(
    result: anyhow::Result<Option<usize>>,
    clock: &jobclock::JobClock,
) {
    match result {
        Ok(Some(n)) if clock.wall() >= CLUSTER_REFRESH_SLOW => {
            app_log!(
                "[issues] {n} clusters refreshed in {}",
                clock.thread_summary()
            );
        }
        Ok(_) => {}
        Err(e) if db::is_busy(&e) => {}
        Err(e) => app_log!("[issues] refresh: {e:#}"),
    }
}

/// `cal index`'s graph upkeep for the pass it just ran (`since`: unix secs before the
/// pass), under the watcher's bounds: commit links for the newest threads the pass wrote
/// (Today's 2 s budget), then the issue clusters when a refresh is due. Without it a machine
/// that keeps the index fresh only through the plugin's `cal index --if-stale` never got
/// either, and its `cal issues` always mined live.
pub fn graph_upkeep_after_pass(conn: &rusqlite::Connection, since: i64) {
    let mut linker = CommitLinker::default();
    link_commits_after_batch(&mut linker, conn, since);
    if clusters_due(conn) {
        let clock = jobclock::JobClock::start();
        log_cluster_refresh(crate::issues::refresh_clusters(conn).map(Some), &clock);
    }
}

/// One-time TODO backfill for indexes from before TODOs were always on (guarded by
/// `knowledge.todos_backfilled`, so later launches return at once). The watcher calls this
/// once its launch catch-up is done, so the two never compete for the writer; the backfill
/// then pauses between batches while a reindex, catch-up, semantic build or rewrite runs, as
/// a distill drain does. On the same thread, so the two never compete for the writer, the
/// one-time re-judge of older TODOs (`knowledge::regate_todos_once`: the assistant checklist
/// gate and follow-ups, resumable, done once `knowledge.todos_regated` says so).
pub fn todo_backfill_kick(host: &Host) {
    let host = Arc::clone(host);
    std::thread::spawn(move || {
        let db = writer(&*host);
        let clock = jobclock::JobClock::start();
        let pause = || writer_job_running(host.flags());
        match knowledge::backfill_todos_once(db, chrono::Utc::now().timestamp(), &pause) {
            Ok(true) => {
                app_log!("[knowledge] todo backfill in {}", clock.thread_summary());
                emit(&*host, "knowledge:todos-ready", &());
            }
            Ok(false) => {}
            Err(e) => app_log!("[knowledge] todo backfill: {e}"),
        }
        let clock = jobclock::JobClock::start();
        match knowledge::regate_todos_once(db, chrono::Utc::now().timestamp(), &pause) {
            Ok(Some(r)) => {
                app_log!(
                    "[knowledge] todo re-judge: {} threads, {} checklist TODOs dropped, {} \
                     follow-ups added in {}",
                    r.threads,
                    r.dropped,
                    r.added,
                    clock.thread_summary()
                );
                if r.dropped + r.added > 0 {
                    emit(&*host, "knowledge:todos-ready", &());
                }
            }
            Ok(None) => {}
            Err(e) => app_log!("[knowledge] todo re-judge: {e}"),
        }
    });
}

/// One-time `file_edits` backfill for threads indexed before 0034 (guarded by 0034's
/// `file_edits.backfill_pending` flag, so later launches return at once): a Reindex never
/// re-parses an unchanged transcript, so without it every older session had no rows. Then,
/// on the same thread so the two never compete for the writer, the one-time token usage
/// backfill for Codex, Gemini CLI, Qwen Code and OpenCode sessions indexed before those
/// sources recorded usage (`indexer::usage_backfill`, done once `usage.backfill_done` is
/// set). The watcher calls this after its launch catch-up, as it does the TODO backfill;
/// both pause between batches while a reindex, catch-up, semantic build or rewrite runs.
pub fn file_edits_backfill_kick(host: &Host) {
    let host = Arc::clone(host);
    std::thread::spawn(move || {
        let db = writer(&*host);
        let pause = || writer_job_running(host.flags());
        if let Err(e) = indexer::file_edits::backfill_file_edits_once(db, &pause) {
            app_log!("[index] file edits backfill: {e:#}");
        }
        // The spend views' caches expire on their own TTL (`search::cache`), so the filled
        // usage shows up on the next read after that without a signal.
        if let Err(e) = indexer::usage_backfill::backfill_usage_once(db, &pause) {
            app_log!("[index] usage backfill: {e:#}");
        }
    });
}

// ---- distill ------------------------------------------------------------------------------

/// Threads per auto-distill round before re-checking the gates (so a reindex/embed can
/// preempt promptly, and the first run on a big history doesn't commit to it all at once).
const AUTO_DISTILL_BATCH: i64 = 25;

/// Distill every not-yet-distilled thread in a project, in the BACKGROUND, so the project's
/// memory fills in. Paced + cancellable; yields to a reindex / embed build if one starts.
/// Emits distill:progress / distill:done.
pub fn distill_project(host: &Host, project: String) {
    if writer_job_running(host.flags()) {
        return; // a reindex / embed build / VACUUM is writing: start later
    }
    let Some(job) = JobGuard::try_acquire(&host.flags().distill.0) else {
        return; // already running
    };
    // The guard moves into the task: tokio drops a panicked task's future, which clears it.
    let task_host = Arc::clone(host);
    host.spawn(Box::pin(async move {
        let host = task_host;
        let ids = match host.readers() {
            Some(pool) => {
                read_blocking(pool, move |conn| {
                    knowledge::project_pending_threads(conn, &project)
                })
                .await
            }
            None => Err(anyhow::anyhow!("the index is not open yet")),
        };
        match ids {
            Ok(ids) => {
                let started = std::time::Instant::now();
                let queued = ids.len();
                match run_distill(&host, &job, ids).await {
                    Ok(n) => app_log!(
                        "[distill] project: {n} of {queued} threads distilled in {}",
                        jobclock::fmt_duration(started.elapsed())
                    ),
                    Err(e) => app_log!("[distill] {e}"),
                }
            }
            Err(e) => app_log!("[distill] worklist: {e}"),
        }
        drop(job);
        shrink_writer(&*host);
        emit(&*host, "distill:done", &());
    }));
}

/// Distill a worklist of thread ids with the resolved engine. Paced (so a cloud provider
/// isn't hammered), cancellable (`job` stops being current once cancel_distill frees the
/// flag), and yields to a reindex / embed build / VACUUM. Shared by the project distill and
/// the background auto-distill. The DB, keychain and embedding work runs on the blocking
/// pool; only the LLM call is awaited on the async runtime.
/// Returns how many threads it distilled.
pub async fn run_distill(host: &Host, job: &JobGuard, ids: Vec<i64>) -> anyhow::Result<usize> {
    run_distill_counted(host, job, ids)
        .await
        .map(|o| o.distilled)
}

/// What a [`run_distill`] pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DistillOutcome {
    pub distilled: usize,
    /// Threads whose LLM call or store failed (each also keeps its `knowledge_error`).
    pub failed: usize,
    /// The last of those failures, for a caller that reports them (`cal distill --auto`).
    pub last_error: Option<String>,
    /// Decisions and gotchas the grounding gate dropped (`agent::parse_grounded`).
    pub ungrounded: usize,
}

/// [`run_distill`] that also counts the failures, so `cal distill --auto` can tell an
/// offline engine or an expired key (every call failed) from an empty worklist.
async fn run_distill_counted(
    host: &Host,
    job: &JobGuard,
    ids: Vec<i64>,
) -> anyhow::Result<DistillOutcome> {
    let mut outcome_total = DistillOutcome::default();
    if ids.is_empty() {
        return Ok(outcome_total);
    }
    let pool = host
        .readers()
        .ok_or_else(|| anyhow::anyhow!("the index is not open yet"))?;
    let (provider, model, key, base_url) =
        read_blocking(pool, crate::resolve_distill_engine).await?;
    let total = ids.len() as i64;
    emit(
        &**host,
        "distill:progress",
        &DistillProgressEvent { done: 0, total },
    );
    for (i, tid) in ids.into_iter().enumerate() {
        // Stop if canceled (even when a newer distill has claimed the flag since), or yield
        // to a reindex / embed build / VACUUM.
        if !job.is_current() || writer_job_running(host.flags()) {
            break;
        }
        // The existing memory is listed per thread, just before its call: a backlog distills
        // newest first, so each thread sees what the ones before it just wrote.
        let input = read_blocking(pool, move |conn| {
            agent::DistillInput::load(conn, tid, context::DEFAULT_BUDGET_CHARS)
        })
        .await?;
        let Some(input) = input else { continue };
        let outcome = agent::distill(
            &provider,
            &model,
            key.as_deref(),
            base_url.as_deref(),
            &input,
        )
        .await;
        let stored = {
            let host = Arc::clone(host);
            blocking(move || {
                let db = writer(&*host);
                match outcome {
                    Ok((result, grounding)) => {
                        if let Some(line) = grounding.summary_line() {
                            app_log!("[distill] thread {tid}: {line}");
                        }
                        {
                            let mut conn = lock_anyhow(db)?;
                            knowledge::store_distilled(
                                &mut conn,
                                tid,
                                &result,
                                &input.refs,
                                chrono::Utc::now().timestamp(),
                            )?;
                        }
                        embed_facts_if_model_ready(&*host, "distill");
                        Ok(Ok(grounding.dropped.len()))
                    }
                    Err(e) => {
                        let msg = short_error(&e);
                        let conn = lock_anyhow(db)?;
                        let _ = knowledge::set_error(&conn, tid, &msg);
                        Ok(Err(msg))
                    }
                }
            })
            .await?
        };
        match stored {
            Ok(ungrounded) => {
                outcome_total.distilled += 1;
                outcome_total.ungrounded += ungrounded;
            }
            Err(msg) => {
                outcome_total.failed += 1;
                outcome_total.last_error = Some(msg);
            }
        }
        emit(
            &**host,
            "distill:progress",
            &DistillProgressEvent {
                done: (i as i64) + 1,
                total,
            },
        );
        // Gentle pacing so a cloud provider isn't hammered.
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    if outcome_total.distilled > 0 {
        host.distill_completed(outcome_total.distilled);
    }
    Ok(outcome_total)
}

/// Claim [`DistillJob`] and reserve the next auto-distill batch, or None when there is
/// nothing to do: a writer job or a distill is running, distillation or auto-distill is
/// off, or nothing is eligible within today's allowance.
fn claim_auto_distill_round(host: &dyn JobHost) -> Option<(JobGuard, Vec<i64>)> {
    if writer_job_running(host.flags()) {
        return None;
    }
    // Own the distill job BEFORE building the worklist: building it spends today's allowance,
    // so a kick that only then found a distill running (a manual "Build memory", an auto
    // batch in flight) burned up to a batch of the daily cap without distilling anything.
    let job = JobGuard::try_acquire(&host.flags().distill.0)?; // a distill is already running
    let ids = (|| {
        let db = host.writer()?;
        let conn = db::lock_conn(&db.0);
        let cfg = knowledge::get_config(&conn).ok()?;
        if !cfg.enabled || !cfg.auto_distill {
            return None;
        }
        // The writer lock is required: this reserves today's allowance in app_config.
        knowledge::auto_distill_worklist(&conn, AUTO_DISTILL_BATCH, chrono::Utc::now().timestamp())
            .ok()
    })()
    .unwrap_or_default();
    if ids.is_empty() {
        return None; // `job` drops here, releasing the flag
    }
    Some((job, ids))
}

/// One auto-distill round on a claimed batch. Returns what it distilled (a round that
/// could not start at all, a keychain or read-pool error, counts as one failure carrying
/// that error) and whether a Cancel ended it.
async fn auto_distill_round(host: &Host, job: JobGuard, ids: Vec<i64>) -> (DistillOutcome, bool) {
    // Wall time only: the round is mostly waiting on the LLM, and its CPU (the local
    // engine's process, the fact embeds) lands on other threads and processes.
    let started = std::time::Instant::now();
    let queued = ids.len();
    let distilled = match run_distill_counted(host, &job, ids).await {
        Ok(o) => {
            app_log!(
                "[auto-distill] round: {} of {queued} threads distilled in {}{}",
                o.distilled,
                jobclock::fmt_duration(started.elapsed()),
                match o.ungrounded {
                    0 => String::new(),
                    n => format!(", {n} ungrounded facts dropped"),
                }
            );
            o
        }
        Err(e) => {
            app_log!("[auto-distill] {e}");
            DistillOutcome {
                distilled: 0,
                failed: 1,
                last_error: Some(short_error(&e)),
                ungrounded: 0,
            }
        }
    };
    // A Cancel ends the drain; re-kicking would start the next batch right away.
    let cancelled = !job.is_current();
    // Released before the re-kick, which would otherwise find the flag still held.
    drop(job);
    emit(&**host, "distill:done", &());
    (distilled, cancelled)
}

/// Kick a background auto-distill drain: distill pending threads corpus-wide so the
/// knowledge surfaces (Ask / recall / Project Memory) self-populate. No-op unless
/// distillation AND auto-distill are enabled and nothing else is running. Drains in
/// batches, re-checking the gates each round, and yields to reindex / embed. Bounded:
/// only threads active in the last 30 days, and at most the per-day allowance
/// (`knowledge::auto_distill_worklist`), so the re-kick chain ends when the day's
/// budget is spent instead of draining a whole history onto a paid key.
///
/// Returning early while a writer job runs loses nothing: every one of them kicks again
/// once it releases its flag (index_all, index_source, the watcher's launch catch-up, the
/// semantic build, Shrink index / Reclaim space), so the launch kick, which usually lands
/// during the catch-up, runs when that pass ends. Returns whether a round started.
pub fn auto_distill_kick(host: &Host) -> bool {
    if !host.kicks_auto_distill() {
        return false;
    }
    // `cal distill --auto` drains in another process; JobFlags only see this one's. Its
    // worklist reserves a count, not thread ids, so a second drain would distill the same
    // threads and charge the daily cap twice. Whatever it leaves is picked up by the next
    // kick after it exits.
    if locks::holder(&db::default_index_path(), locks::DISTILL_LOCK).is_some() {
        return false;
    }
    let Some((job, ids)) = claim_auto_distill_round(&**host) else {
        return false;
    };
    let task_host = Arc::clone(host);
    host.spawn(Box::pin(async move {
        let host = task_host;
        let (_, cancelled) = auto_distill_round(&host, job, ids).await;
        // More may remain (and nothing preempted us): drain the next batch.
        if cancelled || !auto_distill_kick(&host) {
            // The drain ended (nothing left, the day's budget spent, a cancel, or a writer
            // job preempted it): hand back the writer's page cache.
            shrink_writer(&*host);
        }
    }));
    true
}

/// `cal distill --auto`: one drain of the auto-distill worklist in the foreground, round by
/// round under the daily cap, until nothing is eligible, today's allowance is spent, a
/// writer job preempts it, or a round distilled nothing. The same rounds as
/// [`auto_distill_kick`]'s chain, under the same switches (distillation and the
/// auto-distill toggle). Stopping on an empty round matters offline or with an expired
/// key: each round reserves up to [`AUTO_DISTILL_BATCH`] threads of the allowance, so
/// re-claiming after failures spent the whole day's cap in seconds and distilled nothing.
pub async fn auto_distill_drain(host: &Host) -> DistillOutcome {
    let mut total = DistillOutcome::default();
    while let Some((job, ids)) = claim_auto_distill_round(&**host) {
        let (round, cancelled) = auto_distill_round(host, job, ids).await;
        total.distilled += round.distilled;
        total.failed += round.failed;
        total.ungrounded += round.ungrounded;
        if round.last_error.is_some() {
            total.last_error = round.last_error;
        }
        if cancelled || round.distilled == 0 {
            break;
        }
    }
    shrink_writer(&**host);
    total
}

#[cfg(test)]
mod tests {
    use super::{
        messages_pending_embed_sql, owes_full_build, top_up_size_sql, EmbedEnd, JobFlag, JobGuard,
    };
    use std::sync::Arc;

    /// Shrink index and Reclaim space refuse while any `cal` writer holds its lock: a VACUUM
    /// alongside one fails with "database is locked" after the busy timeout, or makes it fail.
    #[test]
    fn a_rewrite_refuses_while_a_cal_writer_runs() {
        use crate::locks;
        // One index per lock: a lock just dropped can stay held for an instant by a child
        // another test is forking (the fd is inherited until its exec closes it).
        for (name, says) in [
            (locks::WATCH_LOCK, "cal watch"),
            (locks::DISTILL_LOCK, "cal distill --auto"),
            (locks::INDEX_LOCK, "cal index"),
            (locks::EMBED_LOCK, "cal embed"),
        ] {
            let index = lock_dir(name);
            assert_eq!(super::cal_writer_running(&index, false), None, "{name}");
            let locks::Acquire::Acquired(_lock) = locks::acquire(&index, name).unwrap() else {
                panic!("{name} is free");
            };
            let why = super::cal_writer_running(&index, false).expect(name);
            assert!(why.starts_with(says), "{name}: {why}");
        }
        // embed.lock held by this process's own build: that one yields instead.
        let index = lock_dir("own_embed");
        let locks::Acquire::Acquired(_lock) = locks::acquire(&index, locks::EMBED_LOCK).unwrap()
        else {
            panic!("embed.lock is free");
        };
        assert_eq!(super::cal_writer_running(&index, true), None);
    }

    /// `index.db` in a fresh temp dir of its own, for one lock case, removed on drop.
    fn lock_dir(tag: &str) -> crate::db::test_support::TestDir {
        crate::db::test_support::TestDir::new(&format!("jobs_writers_{tag}"))
    }

    /// Generous, as in the locks tests: a child another test thread is spawning holds a
    /// dropped lock until its exec.
    const FREED: std::time::Duration = std::time::Duration::from_secs(5);

    /// The app's builds (a Build click, the watcher's top-ups) and a terminal `cal embed` never
    /// embed side by side: the app's build gives its claim back while another process holds
    /// embed.lock, and holds it itself otherwise, which a rewrite reads as this process's
    /// build (asked to yield) rather than `cal embed`.
    #[test]
    fn an_app_build_skips_while_cal_embed_runs_and_holds_embed_lock_itself() {
        use super::{cal_writer_running, claim_embed_build, take_embed_lock, EmbedTrigger};
        use crate::locks;
        use std::sync::atomic::Ordering;
        let index = lock_dir("app_build");
        let flags = super::JobFlags::default();

        let locks::Acquire::Acquired(cal_embed) =
            locks::acquire(&index, locks::EMBED_LOCK).unwrap()
        else {
            panic!("embed.lock is free");
        };
        let running = claim_embed_build(&flags, EmbedTrigger::User).expect("nothing runs here");
        assert!(take_embed_lock(&flags, &index, running, EmbedTrigger::User).is_none());
        assert!(!flags.embed.is_running(), "the claim is given back");
        assert!(!flags.embed.user_requested.load(Ordering::SeqCst));
        assert!(cal_writer_running(&index, flags.embed.holds_process_lock())
            .is_some_and(|why| why.starts_with("cal embed")));
        drop(cal_embed);
        assert!(locks::wait_until_free(&index, locks::EMBED_LOCK, FREED));

        let running = claim_embed_build(&flags, EmbedTrigger::Background).unwrap();
        let running = take_embed_lock(&flags, &index, running, EmbedTrigger::Background)
            .expect("embed.lock is free");
        assert!(
            locks::holder(&index, locks::EMBED_LOCK).is_some(),
            "cal embed sees it"
        );
        assert!(flags.embed.holds_process_lock());
        assert_eq!(
            cal_writer_running(&index, flags.embed.holds_process_lock()),
            None
        );
        drop(running);
        assert!(!flags.embed.holds_process_lock(), "released with the claim");
        assert!(locks::wait_until_free(&index, locks::EMBED_LOCK, FREED));
    }

    /// Shrink index / Reclaim space hold rewrite.lock for the whole rewrite (the `cal`
    /// writers refuse under it), refuse beside a terminal `cal embed` giving both claims
    /// back, and ask this process's own background top-up to yield instead.
    #[test]
    fn a_rewrite_holds_rewrite_lock_and_refuses_beside_cal_embed() {
        use super::{claim_embed_build, claim_rewrite_at, take_embed_lock, EmbedTrigger};
        use crate::locks;
        use std::sync::atomic::Ordering;
        let index = lock_dir("rewrite");
        let flags = super::JobFlags::default();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();

        let job = rt.block_on(claim_rewrite_at(&flags, &index)).unwrap();
        assert!(locks::holder(&index, locks::REWRITE_LOCK).is_some());
        drop(job);
        assert!(!flags.compact.0.is_set());
        assert!(locks::wait_until_free(&index, locks::REWRITE_LOCK, FREED));

        let locks::Acquire::Acquired(cal_embed) =
            locks::acquire(&index, locks::EMBED_LOCK).unwrap()
        else {
            panic!("embed.lock is free");
        };
        let why = rt
            .block_on(claim_rewrite_at(&flags, &index))
            .err()
            .expect("cal embed is building")
            .to_string();
        assert!(why.starts_with("cal embed"), "{why}");
        assert!(!flags.compact.0.is_set());
        assert!(locks::wait_until_free(&index, locks::REWRITE_LOCK, FREED));
        drop(cal_embed);
        assert!(locks::wait_until_free(&index, locks::EMBED_LOCK, FREED));

        // This process's top-up holds embed.lock and stops when asked.
        let running = claim_embed_build(&flags, EmbedTrigger::Background).unwrap();
        let running = take_embed_lock(&flags, &index, running, EmbedTrigger::Background).unwrap();
        std::thread::scope(|s| {
            s.spawn(|| {
                while !flags.embed.cancel.load(Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                drop(running);
            });
            let job = rt
                .block_on(claim_rewrite_at(&flags, &index))
                .expect("the top-up yields");
            assert!(!flags.embed.is_running());
            drop(job);
        });
    }

    /// A thread in `cwd` with one Edit call, written through the indexer.
    fn linked_thread(
        conn: &mut rusqlite::Connection,
        ext: &str,
        cwd: &str,
        updated_at: i64,
    ) -> i64 {
        use crate::indexer::{source_id, upsert_thread, ParsedMessage, ParsedThread};
        let sid = source_id(conn, "claude_code").unwrap();
        let t = ParsedThread {
            external_id: ext.into(),
            project_path: Some(cwd.into()),
            updated_at: Some(updated_at),
            messages: vec![
                ParsedMessage {
                    role: "user".into(),
                    text: "fix the parser".into(),
                    tool_name: None,
                    ts: Some(1_000),
                },
                ParsedMessage {
                    role: "assistant".into(),
                    text: format!(r#"Edit: {{"file_path":"{cwd}/src/parse.rs"}}"#),
                    tool_name: Some("Edit".into()),
                    ts: Some(1_100),
                },
            ],
            ..Default::default()
        };
        upsert_thread(conn, sid, &t).unwrap();
        conn.query_row(
            "SELECT id FROM threads WHERE external_id = ?1",
            [ext],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// A temp dir holding a `.git`, and its canonical spelling (what the linker keys on).
    fn temp_checkout(tag: &str) -> (std::path::PathBuf, String) {
        let repo = std::env::temp_dir().join(format!("calli_jobs_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let canon = std::fs::canonicalize(&repo)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        (repo, canon)
    }

    fn no_probe(_: &str, _: std::time::Instant) -> Option<crate::knowledge::identity::RemoteProbe> {
        None
    }

    /// The watcher's per-batch linking: the batch's threads in a git checkout are linked
    /// once, a checkout linked within the last minute waits for its turn, and the query that
    /// finds the batch's threads stays on 0034's index.
    #[test]
    fn a_batch_links_its_threads_commits_once_per_interval() {
        let (repo, canon) = temp_checkout("link");
        let repo_s = repo.to_string_lossy().into_owned();
        let mut conn = crate::db::open(&temp_db("link")).unwrap();
        // A writer whose wait is not the default (the watcher's sidecar connection).
        conn.busy_timeout(super::Duration::from_millis(61_000))
            .unwrap();
        let since = chrono::Utc::now().timestamp();
        linked_thread(&mut conn, "s1", &repo_s, 1_000);
        // Outside a checkout: never queued.
        linked_thread(&mut conn, "s2", "/no/such/checkout", 1_000);

        let mut linker = super::CommitLinker::default();
        assert_eq!(linker.queue_since(&conn, since).unwrap(), 1);
        let mut runs = 0;
        let log = "\u{1e}COMMIT\u{1f}abcdef0123456789abcdef0123456789abcdef01\u{1f}1200\u{1f}fix parser\nsrc/parse.rs\n";
        let n = linker
            .link_due_with(
                &conn,
                super::Duration::from_secs(2),
                |dir, _, _| {
                    runs += 1;
                    assert_eq!(dir, canon);
                    Ok(Some(log.to_string()))
                },
                |_, _| {
                    Some(crate::knowledge::identity::RemoteProbe {
                        remote: Some("git@github.com:me/app.git".into()),
                        subpath: String::new(),
                    })
                },
            )
            .unwrap();
        assert_eq!((n, runs), (1, 1));
        let linked: i64 = conn
            .query_row("SELECT COUNT(*) FROM thread_commits", [], |r| r.get(0))
            .unwrap();
        assert_eq!(linked, 1);
        // The busy wait the budget shortened is the writer's own again, not the default.
        let busy: i64 = conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        assert_eq!(busy, 61_000);
        // The checkout's key was resolved (and cached) after it linked.
        assert!(linker.portable.is_empty());
        let cached: Option<String> = conn
            .query_row("SELECT portable_id FROM projects", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cached.as_deref(), Some("github.com/me/app"));

        // The next batch a few seconds later queues it again but waits out the interval.
        assert_eq!(linker.queue_since(&conn, since).unwrap(), 1);
        assert!(!linker.has_due(std::time::Instant::now()));
        let n = linker
            .link_due_with(
                &conn,
                super::Duration::from_secs(2),
                |_, _, _| panic!("not due yet"),
                no_probe,
            )
            .unwrap();
        assert_eq!(n, 0);

        let p: String = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {}", super::BATCH_THREADS_SQL))
            .unwrap()
            .query_map(rusqlite::params![0, 50], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join(" | ");
        // The range probe, then a sort of the batch's rows only (never a walk of
        // idx_threads_updated over every thread).
        assert!(
            p.contains("SEARCH threads USING INDEX idx_threads_last_indexed"),
            "{p}"
        );
        assert!(!p.contains("idx_threads_updated"), "{p}");
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// A checkout whose `git log` runs past the deadline is not recorded as linked: it backs
    /// off (doubling), lets the other checkouts go first, and is dropped after the last try.
    #[test]
    fn a_checkout_that_times_out_backs_off_and_yields_its_turn() {
        let (slow, slow_canon) = temp_checkout("slow");
        let (fast, fast_canon) = temp_checkout("fast");
        let mut conn = crate::db::open(&temp_db("slowlink")).unwrap();
        let since = chrono::Utc::now().timestamp();
        linked_thread(&mut conn, "slow", &slow.to_string_lossy(), 1_000);
        let mut linker = super::CommitLinker::default();
        linker.queue_since(&conn, since).unwrap();
        let budget = super::Duration::from_secs(2);

        let n = linker
            .link_due_with(&conn, budget, |_, _, _| Ok(None), no_probe)
            .unwrap();
        assert_eq!(n, 0);
        let q = &linker.pending[&slow_canon];
        assert_eq!(q.failures, 1);
        assert!(!linker.has_due(std::time::Instant::now()), "backing off");
        assert!(linker.has_due(std::time::Instant::now() + super::COMMIT_LINK_EVERY));

        // Due again together with a fresh checkout: the fresh one is tried first.
        linked_thread(&mut conn, "fast", &fast.to_string_lossy(), 2_000);
        linker.queue_since(&conn, since).unwrap();
        linker.pending.get_mut(&slow_canon).unwrap().retry_at = None;
        let mut order = Vec::new();
        let n = linker
            .link_due_with(
                &conn,
                budget,
                |dir, _, _| {
                    order.push(dir.to_string());
                    Ok((dir == fast_canon).then(String::new))
                },
                no_probe,
            )
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(order, [fast_canon.clone(), slow_canon.clone()]);
        assert_eq!(linker.pending[&slow_canon].failures, 2);
        let wait = linker.pending[&slow_canon].retry_at.unwrap() - std::time::Instant::now();
        assert!(wait > super::COMMIT_LINK_EVERY, "doubled: {wait:?}");

        // The last try drops it, and it is not tried again before the interval.
        for _ in 2..super::LINK_MAX_ATTEMPTS {
            linker.pending.get_mut(&slow_canon).unwrap().retry_at = None;
            linker
                .link_due_with(&conn, budget, |_, _, _| Ok(None), no_probe)
                .unwrap();
        }
        assert!(linker.pending.is_empty());
        let _ = std::fs::remove_dir_all(&slow);
        let _ = std::fs::remove_dir_all(&fast);
    }

    /// A session started in a monorepo subdirectory (no `.git` of its own) queues under the
    /// checkout above it, and two spellings of one checkout share one entry.
    #[test]
    fn subdirectory_sessions_queue_under_their_checkout() {
        let (repo, canon) = temp_checkout("mono");
        let web = repo.join("apps/web");
        std::fs::create_dir_all(&web).unwrap();
        let mut conn = crate::db::open(&temp_db("mono")).unwrap();
        let since = chrono::Utc::now().timestamp();
        linked_thread(&mut conn, "web", &web.to_string_lossy(), 1_000);
        linked_thread(
            &mut conn,
            "root",
            &format!("{}/", repo.to_string_lossy()),
            1_000,
        );
        let mut linker = super::CommitLinker::default();
        assert_eq!(linker.queue_since(&conn, since).unwrap(), 2);
        assert_eq!(linker.pending.keys().collect::<Vec<_>>(), [&canon]);
        assert_eq!(linker.pending[&canon].threads.len(), 2);
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// The batch's newest sessions win the 50 slots, not the threads written last in the
    /// pass.
    #[test]
    fn a_batch_queues_its_most_recent_sessions() {
        let mut conn = crate::db::open(&temp_db("recent")).unwrap();
        let since = chrono::Utc::now().timestamp();
        let newest = linked_thread(&mut conn, "newest", "/r/a", 9_000);
        let older = linked_thread(&mut conn, "older", "/r/b", 1_000);
        // Written last in the pass, but the older session.
        conn.execute(
            "UPDATE threads SET last_indexed_at = last_indexed_at + 5 WHERE id = ?1",
            [older],
        )
        .unwrap();
        let ids: Vec<i64> = conn
            .prepare(super::BATCH_THREADS_SQL)
            .unwrap()
            .query_map(rusqlite::params![since, 50], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(ids, [newest, older]);
    }

    /// The watcher's bounded probe agrees with what `knowledge::identity` reads from git: the
    /// origin URL and where the directory sits below the toplevel; a deadline already past
    /// gives no answer (git is killed), never a cached "no remote".
    #[test]
    fn the_bounded_remote_probe_reads_origin_and_subpath() {
        let dir = std::env::temp_dir().join(format!("calli_jobs_probe_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("pkg/web")).unwrap();
        let git = |args: &[&str]| {
            crate::gitlink::git_command()
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        if !git(&["init", "-q"]) {
            return; // no git on this machine
        }
        let sub = dir.join("pkg/web").to_string_lossy().into_owned();
        let soon = || std::time::Instant::now() + super::PORTABLE_PROBE_DEADLINE;
        let p = super::probe_remote_until(&sub, soon()).expect("a checkout");
        assert_eq!((p.remote, p.subpath.as_str()), (None, "pkg/web"));
        assert!(git(&[
            "remote",
            "add",
            "origin",
            "git@github.com:me/mono.git"
        ]));
        let p = super::probe_remote_until(&sub, soon()).expect("a checkout");
        assert_eq!(p.remote.as_deref(), Some("git@github.com:me/mono.git"));
        let past = std::time::Instant::now() - super::Duration::from_millis(1);
        assert_eq!(super::probe_remote_until(&sub, past), None);
        assert_eq!(
            super::probe_remote_until("/no/such/dir/anywhere", soon()),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The off-writer refresh stores what it mined, and yields (writing nothing) when a job
    /// that holds the writer started while it mined.
    #[test]
    fn the_cluster_refresh_yields_to_writer_jobs() {
        let path = temp_db("clusters");
        let conn = crate::db::open(&path).unwrap();
        conn.execute(
            "INSERT INTO threads (source_id, external_id, title, project_path, last_indexed_at)
             VALUES (1, 't', 't', '/r/app', 1)",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        let now = chrono::Utc::now().timestamp();
        for seq in 0..2 {
            conn.execute(
                "INSERT INTO messages (thread_id, seq, role, text, ts)
                 VALUES (?1, ?2, 'tool', 'error: linker failed', ?3)",
                rusqlite::params![tid, seq, now - 60],
            )
            .unwrap();
        }
        assert!(super::clusters_due(&conn), "never refreshed");
        let writer = crate::db::Db(std::sync::Mutex::new(conn));
        let reader = crate::db::open_readonly(&path).unwrap();
        let flags = super::JobFlags::default();
        let busy = JobGuard::try_acquire(&flags.index.0).unwrap();
        assert_eq!(
            super::refresh_clusters_off_writer(&reader, &writer, &flags).unwrap(),
            None
        );
        drop(busy);
        assert_eq!(
            super::refresh_clusters_off_writer(&reader, &writer, &flags).unwrap(),
            Some(1)
        );
        let conn = writer.0.lock().unwrap();
        assert!(
            !super::clusters_due(&conn),
            "just refreshed, nothing indexed since"
        );
    }

    /// Measures the 0034 upgrade, an issue-cluster refresh and a batch link on a real index.
    /// It works on a copy of the file `CALLIMACHUS_DB` names (a clone on APFS, so instant),
    /// never on that file: the upgrade and the rows it writes would otherwise leave the dev
    /// or real index past what the installed app can open, for anyone running every ignored
    /// test with `CALLIMACHUS_DB` exported.
    /// `CALLIMACHUS_DB=/path/to/index.db <test binary> graph_upkeep_on_a_real_index
    /// --ignored --nocapture`.
    #[test]
    #[ignore]
    fn graph_upkeep_on_a_real_index() {
        let src = std::path::PathBuf::from(
            std::env::var("CALLIMACHUS_DB").expect("CALLIMACHUS_DB names an index to copy"),
        );
        let dir = std::env::temp_dir().join(format!("calli_graph_upkeep_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let copy = dir.join("index.db");
        std::fs::copy(&src, &copy).unwrap();
        let wal = src.with_extension("db-wal");
        if wal.exists() {
            std::fs::copy(&wal, dir.join("index.db-wal")).unwrap();
        }
        let path = copy.to_string_lossy().into_owned();
        eprintln!("working on a copy of {} at {path}", src.display());
        let raw = rusqlite::Connection::open(&path).unwrap();
        let before: i64 = raw
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        drop(raw);
        let t = std::time::Instant::now();
        let conn = crate::db::open(std::path::Path::new(&path)).unwrap();
        eprintln!("open + migrate from {before}: {:?}", t.elapsed());
        let (facts, uids, seqs): (i64, i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), COUNT(DISTINCT uid), COUNT(source_seq) FROM facts",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        eprintln!("facts {facts}, distinct uids {uids}, with source_seq {seqs}");
        assert_eq!(facts, uids);

        let t = std::time::Instant::now();
        let n = crate::issues::refresh_clusters(&conn).unwrap();
        eprintln!("refresh_clusters: {n} clusters in {:?}", t.elapsed());

        let newest: i64 = conn
            .query_row("SELECT MAX(last_indexed_at) FROM threads", [], |r| r.get(0))
            .unwrap();
        let mut linker = super::CommitLinker::default();
        let t = std::time::Instant::now();
        let queued = linker.queue_since(&conn, newest - 6 * 3600).unwrap();
        eprintln!("queued {queued} threads in {:?}", t.elapsed());
        let t = std::time::Instant::now();
        let linked = linker.link_due(&conn, crate::today::COMMIT_BUDGET).unwrap();
        eprintln!("linked {linked} checkouts in {:?}", t.elapsed());

        // The one-time file_edits backfill over every thread indexed before 0034, as the
        // watcher and `cal index` run it (into this copy).
        let db = crate::db::Db(std::sync::Mutex::new(conn));
        let t = std::time::Instant::now();
        let r = crate::indexer::file_edits::backfill_file_edits(&db, &|| false).unwrap();
        eprintln!("file_edits backfill: {r:?} in {:?}", t.elapsed());
        let conn = db.0.into_inner().unwrap();
        let mut by_op = conn
            .prepare("SELECT op, COUNT(*) FROM file_edits GROUP BY op ORDER BY 2 DESC")
            .unwrap();
        for row in by_op
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .unwrap()
        {
            eprintln!("  {:?}", row.unwrap());
        }
        drop(by_op);
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn temp_db(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("calli_jobs_{tag}_{}.db", std::process::id()));
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        p
    }

    fn thread(ext: &str, msgs: usize) -> crate::indexer::ParsedThread {
        crate::indexer::ParsedThread {
            external_id: ext.into(),
            title: Some(ext.into()),
            updated_at: Some(1_000),
            created_at: Some(1_000),
            messages: (0..msgs)
                .map(|i| crate::indexer::ParsedMessage {
                    role: if i % 2 == 0 { "user" } else { "assistant" }.into(),
                    text: format!("message {i}"),
                    tool_name: None,
                    ts: Some(1_000),
                })
                .collect(),
            ..Default::default()
        }
    }

    /// The watcher's auto-embed probes this after every quiet minute of agent activity, so
    /// it must stay an index probe on a 1.4M-message table.
    #[test]
    fn pending_messages_check_searches_an_index() {
        let p = temp_db("msgs_pending_plan");
        let conn = crate::db::open(&p).unwrap();
        let sql = messages_pending_embed_sql();
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            plan.iter()
                .any(|d| d.starts_with("SEARCH m USING COVERING INDEX")),
            "{plan:?}"
        );
        assert!(!plan.iter().any(|d| d.starts_with("SCAN m")), "{plan:?}");
        // So must the top-up's size, which picks its thread count.
        let size_sql = top_up_size_sql();
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {size_sql}"))
            .unwrap()
            .query_map([2_000], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            plan.iter()
                .any(|d| d.starts_with("SEARCH m USING COVERING INDEX")),
            "{plan:?}"
        );
        assert!(!plan.iter().any(|d| d.starts_with("SCAN m")), "{plan:?}");
        let pending =
            |c: &rusqlite::Connection| c.query_row(&sql, [], |r| r.get::<_, bool>(0)).unwrap();
        let size = |c: &rusqlite::Connection, cap: i64| -> i64 {
            c.query_row(&size_sql, [cap], |r| r.get(0)).unwrap()
        };
        assert!(!pending(&conn));
        assert_eq!(size(&conn, 2_000), 0);
        let mut conn = conn;
        let sid = crate::indexer::source_id(&conn, "codex").unwrap();
        crate::indexer::upsert_thread(&mut conn, sid, &thread("a", 2)).unwrap();
        assert!(pending(&conn));
        assert_eq!(size(&conn, 2_000), 2);
        assert_eq!(size(&conn, 1), 1, "capped");
    }

    #[test]
    fn job_guard_releases_its_flag_on_drop_and_on_panic() {
        let flag = Arc::new(JobFlag::default());
        let guard = JobGuard::try_acquire(&flag).expect("a free flag can be claimed");
        assert!(flag.is_set());
        assert!(JobGuard::try_acquire(&flag).is_none(), "claimed twice");
        drop(guard);
        assert!(!flag.is_set());

        // A job that panics (a scan parser, ort) must not leave the flag stuck.
        let guard = JobGuard::try_acquire(&flag).unwrap();
        let joined = std::thread::spawn(move || {
            let _guard = guard;
            panic!("job panicked (expected by this test)");
        })
        .join();
        assert!(joined.is_err());
        assert!(!flag.is_set(), "a panicked job kept its flag");
        assert!(JobGuard::try_acquire(&flag).is_some());
    }

    /// Cancel, then start a new job while the old one is still finishing its last step: the
    /// old job must see that it was cancelled, and its drop must not release the new claim.
    #[test]
    fn a_cancelled_guard_cannot_release_a_newer_jobs_flag() {
        let flag = Arc::new(JobFlag::default());
        let old = JobGuard::try_acquire(&flag).unwrap();
        assert!(old.is_current());
        flag.clear(); // cancel_distill
        assert!(
            !old.is_current(),
            "the cancelled job stops at its next check"
        );

        let new = JobGuard::try_acquire(&flag).expect("a cancel frees the flag");
        assert!(!old.is_current(), "a newer claim isn't the old job's");
        drop(old);
        assert!(
            flag.is_set(),
            "the stale guard left the new job's claim alone"
        );
        assert!(new.is_current());
        drop(new);
        assert!(!flag.is_set());
    }

    /// A host that only records the auto-distill kick, and whether the writer flag was
    /// already released when it came.
    struct KickProbe {
        flags: super::JobFlags,
        embedder: crate::embed::Embedder,
        kicked_after_release: std::sync::atomic::AtomicBool,
    }

    impl super::EventSink for KickProbe {
        fn emit(&self, _event: &str, _payload: serde_json::Value) {}
    }

    impl super::JobHost for KickProbe {
        fn flags(&self) -> &super::JobFlags {
            &self.flags
        }
        fn writer(&self) -> Option<&crate::db::Db> {
            None
        }
        fn readers(&self) -> Option<&crate::db::ReadPool> {
            None
        }
        fn embedder(&self) -> &crate::embed::Embedder {
            &self.embedder
        }
        fn spawn(&self, _task: super::Task) {}
        fn kicks_auto_distill(&self) -> bool {
            let released = !self.flags.index.0.is_set();
            self.kicked_after_release
                .store(released, std::sync::atomic::Ordering::SeqCst);
            false
        }
    }

    /// A single-source reindex whose scanner panics still releases IndexJob and then kicks
    /// auto-distill, so a drain that yielded to it resumes.
    #[test]
    fn a_panicking_source_scan_still_releases_and_kicks() {
        let probe = Arc::new(KickProbe {
            flags: super::JobFlags::default(),
            embedder: crate::embed::Embedder::cached_only(),
            kicked_after_release: Default::default(),
        });
        let host: super::Host = probe.clone();
        let job = super::claim_index_source(&probe.flags).unwrap();
        let joined = std::thread::spawn(move || {
            let _release = super::ReleaseThenKick {
                job: Some(job),
                host: &host,
            };
            panic!("scan panicked (expected by this test)");
        })
        .join();
        assert!(joined.is_err());
        assert!(!probe.flags.index.0.is_set(), "the claim was released");
        assert!(
            probe
                .kicked_after_release
                .load(std::sync::atomic::Ordering::SeqCst),
            "the kick ran, after the release"
        );
    }

    /// The watcher's kick partitions an old-shape table even when nothing waits to embed (a
    /// top-up would do it first), and leaves it alone once done or while paused.
    #[test]
    fn the_kick_partitions_an_index_with_nothing_to_embed() {
        use super::{kick_plan, KickPlan};
        // (has vectors, a top-up can run, partitioned, rebuild paused)
        assert_eq!(kick_plan(true, false, false, false), KickPlan::Rebuild);
        assert_eq!(
            kick_plan(true, true, false, false),
            KickPlan::TopUp,
            "the top-up partitions before its first batch"
        );
        assert_eq!(kick_plan(true, true, true, true), KickPlan::TopUp);
        assert_eq!(
            kick_plan(true, false, true, false),
            KickPlan::Nothing,
            "done"
        );
        assert_eq!(
            kick_plan(true, false, false, true),
            KickPlan::Nothing,
            "paused after a failure"
        );
        assert_eq!(
            kick_plan(false, false, false, false),
            KickPlan::Nothing,
            "no semantic index"
        );
    }

    /// The job path's vector upkeep: a rebuild that fails rolls back and pauses the next
    /// ones (each quiet minute would only repeat it); after the pause the table gets its
    /// partition, keeps every vector, and the WAL the rebuild wrote is truncated.
    #[test]
    fn vector_upkeep_pauses_after_a_failure_then_partitions_and_truncates_the_wal() {
        let path = temp_db("upkeep");
        let conn = crate::db::open(&path).unwrap();
        conn.execute(
            "INSERT INTO threads (source_id, external_id, title) VALUES (1, 'upkeep', 't')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        for i in 0..50_usize {
            conn.execute(
                "INSERT INTO messages (thread_id, seq, role, text, embedded)
                 VALUES (?1, ?2, 'user', 'v', 1)",
                rusqlite::params![tid, i as i64],
            )
            .unwrap();
            let mid = conn.last_insert_rowid();
            let mut v = vec![0.0_f32; crate::embed::DIM];
            v[i] = 1.0;
            conn.execute(
                "INSERT INTO vec_chunks (message_id, embedding) VALUES (?1, ?2)",
                rusqlite::params![mid, crate::embed::vec_to_bytes(&v)],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO vec_chunk_owner (chunk_rowid, message_id) VALUES (?1, ?2)",
                [conn.last_insert_rowid(), mid],
            )
            .unwrap();
        }
        let vectors = |c: &rusqlite::Connection| -> i64 {
            c.query_row("SELECT COUNT(*) FROM vec_chunks", [], |r| r.get(0))
                .unwrap()
        };
        // A view named like the rebuild's scratch table makes its transaction fail.
        conn.execute_batch("CREATE VIEW vec_compact_scratch AS SELECT 1 AS x;")
            .unwrap();
        let paused = std::sync::Mutex::new(None);
        super::maintain_vectors_at(&path, &paused);
        assert!(
            paused.lock().unwrap().is_some(),
            "a failure pauses rebuilds"
        );
        assert!(!crate::embed::vec_partitioned(&conn), "rolled back");
        assert_eq!(vectors(&conn), 50);

        conn.execute_batch("DROP VIEW vec_compact_scratch;")
            .unwrap();
        super::maintain_vectors_at(&path, &paused);
        assert!(!crate::embed::vec_partitioned(&conn), "still paused");

        *paused.lock().unwrap() = None;
        super::maintain_vectors_at(&path, &paused);
        assert!(crate::embed::vec_partitioned(&conn));
        assert!(paused.lock().unwrap().is_none());
        assert_eq!(vectors(&conn), 50);
        let wal = std::fs::metadata(format!("{}-wal", path.display())).map_or(0, |m| m.len());
        assert_eq!(wal, 0, "the rebuild's WAL is given back");

        drop(conn);
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(path.with_extension(ext));
        }
    }

    /// The tail-window sequence: a capped top-up exits its batch loop, a Build click then
    /// finds the build running and only sets `user_requested`, and the build reaches its
    /// release. It must run the full build first, or the click's `embed:done` toasts "up to
    /// date" over a capped backlog.
    #[test]
    fn a_click_in_a_top_ups_tail_gets_a_full_build() {
        assert!(owes_full_build(EmbedEnd::Capped, false, true));
        assert!(!owes_full_build(EmbedEnd::Capped, false, false), "no click");
        // A failed top-up the click did not hear about: the full build reports the reason.
        assert!(owes_full_build(EmbedEnd::Stopped, false, true));
        assert!(
            !owes_full_build(EmbedEnd::Stopped, true, true),
            "already told"
        );
        // A click mid-pass always continues as a full build.
        assert!(owes_full_build(EmbedEnd::HandedOff, false, true));
        // The backlog is empty, so the toast is true; or a reindex is waiting for the writer.
        assert!(!owes_full_build(EmbedEnd::Drained, false, true));
        assert!(!owes_full_build(EmbedEnd::Cancelled, false, true));
    }
}
