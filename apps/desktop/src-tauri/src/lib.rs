// First, so `app_log!` is in scope for every module declared after it.
#[macro_use]
mod applog;
pub(crate) mod jobclock;
pub(crate) mod memtrim;

pub mod agent;
mod error;
mod indexer;
pub mod secrets;

// Public so the sidecar binaries (MCP server, `cal` CLI) can reuse the core.
pub mod cleanup;
pub mod cli_core;
pub mod context;
pub mod cost;
pub mod db;
pub mod embed;
pub mod export;
pub mod gitlink;
pub mod integration;
pub mod issues;
pub mod knowledge;
// Local-only counts of what the hooks and MCP server did; never telemetry (module docs).
pub mod ledger;
pub mod mcp_server;
// The first-run checklist's saved answers; GUI only.
mod onboarding;
pub mod search;
pub mod snapshot;
// Opt-in analytics, GUI only: constructed in `run()` and nowhere else (see its module docs).
pub mod telemetry;

use error::AppResult;
use search::{SearchFilters, ThreadDetail, ThreadSummary};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, WindowEvent};

/// The background embedding job: a run guard plus its live progress counters, so
/// `embedding_status` can answer from memory while a build runs instead of re-running the
/// two COUNT(*) scans on every poll.
#[derive(Default)]
struct EmbedJob {
    running: Arc<JobFlag>,
    done: AtomicI64,
    total: AtomicI64,
    /// The user asked for the running build: it was started by a Build click, or one landed
    /// during a capped background top-up, which then hands over to a full build so the
    /// click's "up to date" toast is true. Such a build is never cancelled. Cleared when
    /// the build releases `running`.
    user_requested: AtomicBool,
    /// A reindex or an index rewrite asked the running background top-up to stop after its
    /// current batch (see [`request_embed_yield`]). Reset when a build starts.
    cancel: AtomicBool,
    /// Background top-ups stay off until then after one stopped because the model would
    /// not load (a corrupt cache): retrying every quiet minute would only repeat it.
    auto_paused_until: Mutex<Option<std::time::Instant>>,
}

/// How long background top-ups pause after the model failed to load in one.
const AUTO_EMBED_FAILURE_PAUSE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

impl EmbedJob {
    fn is_running(&self) -> bool {
        self.running.is_set()
    }
}

/// Longest a reindex or an index rewrite waits for a background top-up to stop. It stops
/// after its current batch: 128 messages at about 1.7 vectors each (317k vectors for 190k
/// embedded messages on the measured index), about 3.5 s at the background model's 63
/// chunks/s on a 24-core machine, longer on a smaller one.
const EMBED_YIELD_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Ask a running background top-up to stop after its current batch, so a reindex or an
/// index rewrite gets the writer instead of being refused for a build the user never
/// started. False when the user asked for the running build (never cancelled). Call it
/// holding the caller's own flag (IndexJob / CompactJob): a build claims EmbedJob and then
/// checks those, so no new build starts after this, and a build that claimed first is the
/// one this sees and stops.
fn request_embed_yield(app: &AppHandle) -> bool {
    let job = app.state::<EmbedJob>();
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
fn wait_for_embed_yield(app: &AppHandle) -> bool {
    let deadline = std::time::Instant::now() + EMBED_YIELD_WAIT;
    let job = app.state::<EmbedJob>();
    while job.is_running() {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    true
}

/// Guards against more than one user-started re-index (Reindex, a single source) running
/// at a time. The watcher's launch catch-up holds [`CatchUpJob`] instead, so the search bar
/// and the Reindex button only report reindexes the user started.
#[derive(Default)]
struct IndexJob(Arc<JobFlag>);

/// Held while the watcher's launch catch-up rescans every source. It holds the write lock
/// like a reindex, so every gate that waits for [`IndexJob`] waits for this too, but it is
/// reported apart (`JobsStatus::catch_up`): it runs on every launch, takes 0.1-2 s, and
/// showing it as "Indexing..." flashed a 0% bar and a disabled Reindex button each time.
#[derive(Default)]
pub(crate) struct CatchUpJob(pub(crate) Arc<JobFlag>);

/// Guards against more than one project-distill running at a time; clearing it mid-run
/// (via `cancel_distill`) also signals the running job to stop.
#[derive(Default)]
struct DistillJob(Arc<JobFlag>);

/// Held while Shrink index / Reclaim space rewrites the whole file, so a second rewrite,
/// a reindex, the watcher catch-up, a semantic build or a distill doesn't start mid-VACUUM.
#[derive(Default)]
struct CompactJob(Arc<JobFlag>);

/// One background job's busy flag: the token of the [`JobGuard`] holding it, 0 when free.
/// Tokens are never reused, so a guard can tell its own claim from a newer job's.
#[derive(Default)]
pub(crate) struct JobFlag(AtomicU64);

impl JobFlag {
    pub(crate) fn is_set(&self) -> bool {
        self.0.load(Ordering::SeqCst) != 0
    }

    /// Free the flag whoever holds it (a cancel). The holder sees [`JobGuard::is_current`]
    /// turn false and stops, and its drop then leaves a newer job's claim alone.
    fn clear(&self) {
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
pub(crate) struct JobGuard {
    flag: Arc<JobFlag>,
    token: u64,
}

impl JobGuard {
    /// Claim `flag`, or None if a job already holds it.
    pub(crate) fn try_acquire(flag: &Arc<JobFlag>) -> Option<Self> {
        let token = NEXT_JOB_TOKEN.fetch_add(1, Ordering::Relaxed);
        flag.0
            .compare_exchange(0, token, Ordering::SeqCst, Ordering::SeqCst)
            .ok()?;
        Some(Self {
            flag: Arc::clone(flag),
            token,
        })
    }

    /// Whether this guard still owns its flag: false once a cancel freed it, even if
    /// another job has claimed it since.
    fn is_current(&self) -> bool {
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

/// Live mirror of the `window.close_to_tray` preference, so the window-close handler can
/// decide without touching the DB on every close. Seeded at startup, kept in sync by
/// `set_close_to_tray`.
#[derive(Default)]
struct CloseToTray(AtomicBool);

/// Keeps the user's quick edits (star, tags, fact pin / hide / edit, TODO done, provider URL)
/// in the order the UI sent them. Each async command runs its write in its own blocking task,
/// and std's `Mutex` hands the writer out in no particular order, so while the writer is busy
/// (an index or embed write, a retry behind another process's lock) "tags [A]" then "tags
/// [A, B]" could land in reverse and leave the stale list saved. tokio's `Mutex` grants its
/// lock in request order, so holding it across the write serializes them FIFO.
#[derive(Default)]
struct EditOrder(tokio::sync::Mutex<()>);

/// Tracks frontend + backend startup readiness so the splash window is dismissed only
/// once BOTH are ready (the Tauri splashscreen pattern).
#[derive(Default)]
struct SetupState(Mutex<SetupFlags>);
#[derive(Default)]
struct SetupFlags {
    frontend: bool,
    backend: bool,
    /// Set once the splash closed and main was shown, so later calls (the frontend's own
    /// fallback timer, the backend backstop) are no-ops.
    revealed: bool,
    /// When [`show_splash`] put the splash up, for [`SPLASH_MIN_VISIBLE`].
    splash_shown: Option<std::time::Instant>,
}

/// How long after the main window is created the backend reveals it even if the frontend
/// never reported ready (a bundle that failed to load, a render that threw before mount).
const REVEAL_BACKSTOP: std::time::Duration = std::time::Duration::from_secs(10);

/// How long a launch may take before the splash appears. A normal launch reveals the main
/// window in about 300 ms (backend ready in 77-93 ms, the frontend's first data 191-214 ms
/// later), and a splash shown for that long and then replaced by a second window read as a
/// flicker; so the splash is created hidden and only a slower launch (a cold cache, a slow
/// machine) or a migration shows it.
const SPLASH_DELAY: std::time::Duration = std::time::Duration::from_millis(400);

/// Shortest time a splash that did appear stays up. A launch that reveals just after
/// [`SPLASH_DELAY`] (about 450 ms on a slower machine) showed the splash for 50 ms and then
/// the main window: the same flash the delay removes.
const SPLASH_MIN_VISIBLE: std::time::Duration = std::time::Duration::from_millis(600);

/// How much longer a splash shown at `shown` must stay up at `now` (zero if never shown).
fn splash_hold(shown: Option<std::time::Instant>, now: std::time::Instant) -> std::time::Duration {
    shown.map_or(std::time::Duration::ZERO, |t| {
        SPLASH_MIN_VISIBLE.saturating_sub(now.saturating_duration_since(t))
    })
}

/// Show the (hidden) splash unless the main window was already revealed. Decided under the
/// SetupState lock, which `complete_setup` takes to set `revealed` before it closes the
/// splash, so a splash shown here is always closed by the reveal, never shown after it.
/// `show` only posts to the event loop, so holding the lock across it can't deadlock the
/// main thread's `set_complete`.
fn show_splash(app: &AppHandle) {
    let Some(state) = app.try_state::<SetupState>() else {
        return;
    };
    let Ok(mut flags) = state.0.lock() else {
        return;
    };
    if flags.revealed || flags.splash_shown.is_some() {
        return;
    }
    if let Some(splash) = app.get_webview_window("splashscreen") {
        let _ = splash.show();
        flags.splash_shown = Some(std::time::Instant::now());
    }
}

/// Mark a startup task done; when BOTH frontend and backend are ready, close the splash
/// window and reveal the main window, once the splash (if it appeared) has been up for
/// [`SPLASH_MIN_VISIBLE`]. Idempotent.
fn complete_setup(app: &AppHandle, task: &str) {
    let (reveal, shown) = {
        let Some(state) = app.try_state::<SetupState>() else {
            return;
        };
        let Ok(mut flags) = state.0.lock() else {
            return;
        };
        match task {
            "frontend" => flags.frontend = true,
            "backend" => flags.backend = true,
            _ => {}
        }
        let ready = flags.frontend && flags.backend && !flags.revealed;
        if ready {
            flags.revealed = true;
        }
        (ready, flags.splash_shown)
    };
    if !reveal {
        return;
    }
    let finish = |app: &AppHandle| {
        if let Some(splash) = app.get_webview_window("splashscreen") {
            let _ = splash.close();
        }
        if let Some(main) = app.get_webview_window("main") {
            let _ = main.show();
            let _ = main.set_focus();
        }
    };
    let hold = splash_hold(shown, std::time::Instant::now());
    if hold.is_zero() {
        finish(app);
    } else {
        // `set_complete` runs on the main thread: wait on another one. `revealed` is
        // already set, so nothing shows the splash again meanwhile.
        let app = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(hold);
            finish(&app);
        });
    }
}

/// Frontend signals it has loaded its initial data (the other half of `complete_setup`).
#[tauri::command]
fn set_complete(app: AppHandle, task: String) {
    complete_setup(&app, &task);
}

/// Run blocking work (SQLite, files, git, the keychain, model inference) on Tauri's
/// blocking thread pool. Plain `#[tauri::command] fn`s run on the main thread, so a slow
/// query there froze the whole window (beachball / "Not Responding") and serialized every
/// other IPC call behind it; async commands hand the work off with this instead.
async fn blocking<T, F>(f: F) -> AppResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| anyhow::anyhow!("background task failed: {e}"))?
        .map_err(Into::into)
}

/// [`blocking`] with a pooled read-only connection checked out on the worker thread.
async fn read_blocking<T, F>(pool: &db::ReadPool, f: F) -> AppResult<T>
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

/// [`blocking`] with the shared writer connection, locked on the worker thread.
async fn write_blocking<T, F>(app: &AppHandle, f: F) -> AppResult<T>
where
    T: Send + 'static,
    F: FnOnce(&mut rusqlite::Connection) -> anyhow::Result<T> + Send + 'static,
{
    let app = app.clone();
    blocking(move || {
        let db = app.state::<db::Db>();
        let mut conn = lock_anyhow(&db)?;
        f(&mut conn)
    })
    .await
}

/// Run one of the user's quick edits after every edit sent before it (see [`EditOrder`]).
/// The write future is lazy, so nothing touches the database until this edit's turn.
async fn in_edit_order<T>(
    app: &AppHandle,
    write: impl std::future::Future<Output = AppResult<T>>,
) -> AppResult<T> {
    let order = app.state::<EditOrder>();
    let _turn = order.0.lock().await;
    write.await
}

/// Queue an analytics event if telemetry is live (a no-op otherwise, including before
/// startup finished managing the state).
fn track(app: &AppHandle, event: telemetry::Event) {
    if let Some(t) = app.try_state::<telemetry::TelemetryState>() {
        t.track(event);
    }
}

/// The in-flight chat stream (at most one): its id and cancellation token. A new send
/// cancels the previous stream, and each stream only clears the slot it still owns, so a
/// late-finishing stream can't wipe the token of the one that replaced it (which would
/// leave that one impossible to stop).
#[derive(Default)]
struct ChatGeneration {
    seq: AtomicU64,
    slot: Mutex<Option<(u64, tokio_util::sync::CancellationToken)>>,
}

/// Pending shell-command approvals, keyed by tool call id. `approve_tool` resolves
/// the matching one-shot, unblocking the awaiting tool execution.
#[derive(Default)]
struct PendingApprovals(
    Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<bool>>>,
);

impl PendingApprovals {
    /// Deny every waiting approval (a cancelled stream must not stay parked on one).
    fn deny_all(&self) {
        if let Ok(mut pending) = self.0.lock() {
            for (_, tx) in pending.drain() {
                let _ = tx.send(false);
            }
        }
    }
}

/// Chars of a thread the chat agent's `get_thread` tool receives. `chat_stream` clips each
/// tool result to about this size anyway, so packing to it lets `pack_thread` choose what
/// fits (drop tool output, elide the middle) instead of the clip keeping only the head.
const CHAT_GET_THREAD_BUDGET: usize = 12_000;

/// Most results the chat agent's `search_history` tool may ask for.
const CHAT_SEARCH_MAX: u64 = 20;

/// Execute one tool call requested by the in-app agent. Read-only index tools run
/// immediately; `run_shell` emits an approval request and waits for the user.
async fn run_tool(
    app: AppHandle,
    ch: tauri::ipc::Channel<agent::ChatChunk>,
    call: genai::chat::ToolCall,
) -> anyhow::Result<String> {
    let name = call.fn_name.clone();
    let args = call.fn_arguments.clone();
    let arg_str = |k: &str| {
        args.get(k)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };

    // Announce the call.
    let announce = match name.as_str() {
        "search_history" => format!("search: {}", arg_str("query")),
        "get_thread" => format!(
            "thread #{}",
            args.get("thread_id").and_then(|v| v.as_i64()).unwrap_or(0)
        ),
        "run_shell" => arg_str("command"),
        _ => name.clone(),
    };
    let _ = ch.send(agent::ChatChunk {
        kind: "tool_call",
        text: announce,
        tool_id: Some(call.call_id.clone()),
        tool_name: Some(name.clone()),
    });

    let result = |text: String| agent::ChatChunk {
        kind: "tool_result",
        text,
        tool_id: Some(call.call_id.clone()),
        tool_name: Some(name.clone()),
    };

    match name.as_str() {
        "search_history" => {
            let query = arg_str("query");
            let limit = args
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(10)
                .clamp(1, CHAT_SEARCH_MAX) as u32;
            let pool = app.state::<db::ReadPool>();
            let json = read_blocking(&pool, move |conn| chat_search_json(conn, &query, limit))
                .await
                .map_err(error::AppError::into_inner)?;
            let _ = ch.send(result(format!(
                "{} results",
                json.matches("\"threadId\"").count()
            )));
            Ok(json)
        }
        "get_thread" => {
            let tid = args.get("thread_id").and_then(|v| v.as_i64()).unwrap_or(0);
            let pool = app.state::<db::ReadPool>();
            let packed = read_blocking(&pool, move |conn| {
                Ok(context::pack_thread(conn, tid, CHAT_GET_THREAD_BUDGET)?
                    .unwrap_or_else(|| "thread not found".to_string()))
            })
            .await
            .map_err(error::AppError::into_inner)?;
            let _ = ch.send(result(format!(
                "loaded thread #{tid} ({} chars)",
                packed.len()
            )));
            Ok(packed)
        }
        "run_shell" => {
            let command = arg_str("command");
            // Request approval and wait for the user.
            let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
            app.state::<PendingApprovals>()
                .0
                .lock()
                .map_err(|e| anyhow::anyhow!("approvals lock: {e}"))?
                .insert(call.call_id.clone(), tx);
            let _ = ch.send(agent::ChatChunk {
                kind: "tool_request",
                text: command.clone(),
                tool_id: Some(call.call_id.clone()),
                tool_name: Some("run_shell".into()),
            });
            if !rx.await.unwrap_or(false) {
                let _ = ch.send(result("✗ denied by user".into()));
                return Ok("The user denied running this command.".into());
            }
            // `cmd /C` on Windows (no console window), `sh -c` elsewhere; spawn failures
            // come back as tool output instead of erroring the whole chat turn.
            let out = agent::run_shell_capture(&command).await;
            let _ = ch.send(result(out.clone()));
            Ok(out)
        }
        other => Ok(format!("unknown tool: {other}")),
    }
}

/// The chat agent's `search_history` result: keyword hits as JSON for the chat provider.
/// The index stores transcripts raw, so every string (title, snippet) is redacted here. The
/// FTS match sentinels come out first: highlights mean nothing to a model, and one landing
/// inside a key would split it so the redaction pattern no longer sees it.
fn chat_search_json(
    conn: &rusqlite::Connection,
    query: &str,
    limit: u32,
) -> anyhow::Result<String> {
    let mut hits = search::search(
        conn,
        query,
        &SearchFilters {
            limit: Some(limit),
            ..Default::default()
        },
    )?;
    for h in &mut hits {
        h.snippet = h.snippet.replace(['\u{1}', '\u{2}'], "");
    }
    Ok(context::to_redacted_json(&hits)?)
}

/// Lightweight counts for the dashboard / verification.
#[derive(Debug, Serialize)]
pub struct DbStats {
    pub threads: i64,
    pub messages: i64,
    pub sources: i64,
    /// Free space inside the file, for Settings' Reclaim disk space row (null if the
    /// PRAGMAs failed; the counts still answer).
    pub reclaimable: Option<cleanup::Reclaimable>,
}

fn lock<'a>(db: &'a db::Db) -> AppResult<std::sync::MutexGuard<'a, rusqlite::Connection>> {
    db.0.lock()
        .map_err(|e| anyhow::anyhow!("db lock poisoned: {e}").into())
}

/// Same as [`lock`] but anyhow-typed, for background helpers.
fn lock_anyhow(db: &db::Db) -> anyhow::Result<std::sync::MutexGuard<'_, rusqlite::Connection>> {
    db.0.lock()
        .map_err(|e| anyhow::anyhow!("db lock poisoned: {e}"))
}

/// Check out a pooled READ-ONLY connection. Use in read commands so they run concurrently
/// (WAL) instead of serializing behind the single writer mutex. Writes must use [`lock`].
fn read(pool: &db::ReadPool) -> AppResult<db::ReadConn> {
    pool.0
        .get()
        .map_err(|e| anyhow::anyhow!("read pool: {e}").into())
}

#[tauri::command]
async fn db_stats(pool: tauri::State<'_, db::ReadPool>) -> AppResult<DbStats> {
    read_blocking(&pool, |conn| Ok(db_counts(conn)?)).await
}

/// The header counts. Messages come from the per-thread `message_count` that upsert_thread
/// rewrites on every full and incremental write (only whole threads are ever deleted), so
/// one pass over `threads` answers both: COUNT(*) FROM messages walked a 17 MB index, 0.6 to
/// 0.8 s on a cold cache, and this query sits on the path to the first window reveal.
fn db_counts(conn: &rusqlite::Connection) -> rusqlite::Result<DbStats> {
    let (threads, messages) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(message_count), 0) FROM threads",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let sources = conn.query_row("SELECT COUNT(*) FROM sources", [], |r| r.get(0))?;
    Ok(DbStats {
        threads,
        messages,
        sources,
        // Header PRAGMAs, microseconds: fine on the path to the first reveal.
        reclaimable: cleanup::reclaimable(conn).ok(),
    })
}

/// Rich index analytics for the dashboard: per-source / per-role breakdowns,
/// date range, embedding coverage, and top projects. Cached for a few minutes while the
/// thread set is unchanged (`cal stats` and MCP read live).
#[tauri::command]
async fn index_stats(pool: tauri::State<'_, db::ReadPool>) -> AppResult<search::Stats> {
    read_blocking(&pool, search::stats_cached).await
}

/// Proactive Coach dashboard: a daily-activity heatmap + the last week's distilled
/// decisions and gotchas. The heatmap is cached; the week's facts are read live.
#[tauri::command]
async fn coach_overview(pool: tauri::State<'_, db::ReadPool>) -> AppResult<search::CoachOverview> {
    read_blocking(&pool, |conn| {
        search::coach_overview_cached(conn, chrono::Utc::now().timestamp())
    })
    .await
}

/// Recurring errors mined across all sessions in the last 180 days (most frequent first).
#[tauri::command]
async fn recurring_issues(
    pool: tauri::State<'_, db::ReadPool>,
    project: Option<String>,
) -> AppResult<Vec<issues::IssueCluster>> {
    read_blocking(&pool, move |conn| {
        let since = chrono::Utc::now().timestamp() - 180 * 86_400;
        issues::recurring_issues_cached(conn, project.as_deref(), since, 30)
    })
    .await
}

/// Estimated $ spend by model + the priciest threads (all-time), from captured token usage.
#[tauri::command]
async fn spend(
    pool: tauri::State<'_, db::ReadPool>,
    project: Option<String>,
) -> AppResult<cost::Spend> {
    read_blocking(&pool, move |conn| {
        cost::spend_cached(conn, 0, project.as_deref(), 8)
    })
    .await
}

/// The git commits a thread likely produced (inferred file-overlap links). Read-only.
#[tauri::command]
async fn thread_commits(
    pool: tauri::State<'_, db::ReadPool>,
    thread_id: i64,
) -> AppResult<Vec<gitlink::CommitLink>> {
    read_blocking(&pool, move |conn| gitlink::linked_commits(conn, thread_id)).await
}

/// (Re)compute git linkage for a thread's project by reading its `git log`, then return the
/// thread's commits. Runs on its own connection: `git log --name-only` can take seconds on
/// a big repo, and holding the shared writer mutex that long stalled every other write.
#[tauri::command]
async fn link_thread_commits(thread_id: i64) -> AppResult<Vec<gitlink::CommitLink>> {
    blocking(move || {
        let conn = db::open(&db::default_index_path())?;
        let repo: Option<String> = conn
            .query_row(
                "SELECT project_path FROM threads WHERE id = ?1",
                [thread_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten();
        if let Some(repo) = repo.filter(|p| !p.is_empty()) {
            gitlink::link_project(&conn, &repo)?;
        }
        gitlink::linked_commits(&conn, thread_id)
    })
    .await
}

/// Oldest-first list of threads with their size, for the storage-cleanup UI.
#[tauri::command]
async fn cleanup_candidates(
    pool: tauri::State<'_, db::ReadPool>,
    before: Option<i64>,
    sources: Option<Vec<String>>,
    limit: Option<i64>,
) -> AppResult<Vec<cleanup::CleanupRow>> {
    read_blocking(&pool, move |conn| {
        cleanup::candidates(
            conn,
            before,
            &sources.unwrap_or_default(),
            limit.unwrap_or(200),
        )
    })
    .await
}

/// Permanently delete the given threads (cascades to messages, FTS, vectors) and tombstone
/// them so the next scan doesn't re-import them.
#[tauri::command]
async fn delete_threads(app: AppHandle, ids: Vec<i64>) -> AppResult<usize> {
    write_blocking(&app, move |conn| cleanup::delete_threads(conn, &ids)).await
}

/// Reclaim disk space freed by deletes (VACUUM). Runs on a DEDICATED connection: VACUUM
/// rewrites the whole file and would otherwise hold the shared mutex for the entire
/// (multi-second) rewrite, freezing every other command. On its own connection it still
/// takes SQLite's write lock but never the Rust mutex, so the rest of the UI stays live.
/// Returns without writing when too little is free (`cleanup::RECLAIM_MIN_BYTES`).
#[tauri::command]
async fn vacuum_db(app: AppHandle) -> AppResult<cleanup::VacuumReport> {
    let job = claim_rewrite(&app).await?;
    let result = blocking(|| {
        let conn = db::open(&db::default_index_path())?;
        cleanup::vacuum(&conn)
    })
    .await;
    release_rewrite(&app, job);
    result
}

/// Claim [`CompactJob`] for a whole-file rewrite, or say why it can't start. VACUUM holds
/// the write lock for the whole rewrite, so a reindex or semantic build running alongside
/// stalls on it, and a second rewrite fails with "database is locked". A background
/// top-up is asked to stop and waited for (at most [`EMBED_YIELD_WAIT`]), since the watcher
/// starts one after most quiet minutes of agent activity.
async fn claim_rewrite(app: &AppHandle) -> anyhow::Result<JobGuard> {
    // Claim first, then check: a job that starts after this sees CompactJob and waits.
    let Some(job) = JobGuard::try_acquire(&app.state::<CompactJob>().0) else {
        anyhow::bail!("The index is already being rewritten. Try again when that finishes.");
    };
    const EMBED_BUSY: &str = "The semantic index is being built";
    let busy = if app.state::<IndexJob>().0.is_set() {
        Some("A reindex is running")
    } else if app.state::<CatchUpJob>().0.is_set() {
        Some("New sessions are being indexed")
    } else if app.state::<DistillJob>().0.is_set() {
        Some("Project memory is being distilled")
    } else if !request_embed_yield(app) {
        Some(EMBED_BUSY)
    } else {
        None
    };
    if let Some(busy) = busy {
        anyhow::bail!("{busy}. Try again when it finishes."); // `job` drops: released
    }
    let deadline = std::time::Instant::now() + EMBED_YIELD_WAIT;
    while app.state::<EmbedJob>().is_running() {
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("{EMBED_BUSY}. Try again when it finishes.");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    Ok(job)
}

/// Release a [`claim_rewrite`] claim, then pick up the auto-distill drain it held off.
fn release_rewrite(app: &AppHandle, job: JobGuard) {
    drop(job);
    auto_distill_kick(app);
}

/// Which background jobs hold their flags, so the UI can disable buttons that would collide
/// with one (Shrink index / Reclaim space refuse while any of the others runs). Field names
/// match `JobsStatus` in api.ts.
#[derive(Debug, Serialize)]
struct JobsStatus {
    /// A full or single-source reindex the user started.
    index: bool,
    /// The watcher's launch catch-up (see [`CatchUpJob`]): it blocks a whole-file rewrite
    /// like a reindex, but the search bar stays quiet for it.
    #[serde(rename = "catchUp")]
    catch_up: bool,
    /// A semantic-index build, including its closing vector compaction.
    embed: bool,
    /// A project distill or an auto-distill batch.
    distill: bool,
    /// Shrink index / Reclaim space.
    rewrite: bool,
}

/// Answers from the in-memory flags, so it is cheap to poll while a job runs.
#[tauri::command]
fn jobs_status(app: AppHandle) -> JobsStatus {
    JobsStatus {
        index: app.state::<IndexJob>().0.is_set(),
        catch_up: app.state::<CatchUpJob>().0.is_set(),
        embed: app.state::<EmbedJob>().is_running(),
        distill: app.state::<DistillJob>().0.is_set(),
        rewrite: app.state::<CompactJob>().0.is_set(),
    }
}

/// What a `compact_index` pass did, for the Settings confirmation.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct CompactSummary {
    rows: usize,
    bytes_saved: i64,
    /// The closing [`cleanup::vacuum`], once the trim is done (absent from the progress
    /// events). `ran: false` means the saved space stays inside the file, for later
    /// inserts to reuse, until enough is free for Reclaim space.
    #[serde(skip_serializing_if = "Option::is_none")]
    vacuum: Option<cleanup::VacuumReport>,
}

/// "Shrink index": cap tool output that was indexed before the per-row cap existed (in
/// batches, so other writers get turns), every source's threads including those whose
/// transcript is gone (the automatic pass leaves those alone), then VACUUM when enough is
/// free, by the same rule as Reclaim space ([`cleanup::RECLAIM_MIN_BYTES`]). Dedicated
/// connection for the same reason as `vacuum_db`. Emits `compact:progress`.
#[tauri::command]
async fn compact_index(app: AppHandle) -> AppResult<CompactSummary> {
    let job = claim_rewrite(&app).await?;
    let progress = app.clone();
    let result = blocking(move || {
        let mut conn = db::open(&db::default_index_path())?;
        let mut summary = CompactSummary::default();
        let mut after = 0i64;
        loop {
            let r = cleanup::compact_tool_output_after(&mut conn, after, 2_000)?;
            after = r.last_id;
            summary.rows += r.rows;
            summary.bytes_saved += r.bytes_saved;
            let _ = progress.emit("compact:progress", &summary);
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
    })
    .await;
    release_rewrite(&app, job);
    result
}

/// Relocated agent data roots, for Settings. A Finder/Dock-launched app never sees the
/// shell's `CLAUDE_CONFIG_DIR` / `CODEX_HOME`, so these overrides are how such users point
/// the indexer (and every other process that opens the index) at their real data.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DataRoots {
    /// Saved override ("" = unset).
    claude_config_dir: String,
    codex_home: String,
    /// What is actually used right now (override, else env var, else the default).
    effective_claude_config_dir: Option<String>,
    effective_codex_home: Option<String>,
}

fn data_roots(conn: &rusqlite::Connection) -> anyhow::Result<DataRoots> {
    let saved = |key: &str| -> anyhow::Result<String> {
        Ok(knowledge::app_config::get(conn, key)?.unwrap_or_default())
    };
    let shown = |p: Option<std::path::PathBuf>| p.map(|p| p.to_string_lossy().into_owned());
    Ok(DataRoots {
        claude_config_dir: saved(indexer::claude::CONFIG_DIR_SETTING)?,
        codex_home: saved(indexer::codex::CODEX_HOME_SETTING)?,
        effective_claude_config_dir: shown(indexer::claude::config_dir()),
        effective_codex_home: shown(indexer::codex::codex_root()),
    })
}

#[tauri::command]
async fn get_data_roots(pool: tauri::State<'_, db::ReadPool>) -> AppResult<DataRoots> {
    read_blocking(&pool, data_roots).await
}

/// Save (or clear, with "") the Claude config dir and Codex home overrides. Takes effect
/// for this process at once; the watcher picks up new roots within a minute. A path must
/// name an existing folder and is stored in its canonical form (see
/// `indexer::claude::normalize_data_root`); anything else is refused with the reason.
#[tauri::command]
async fn set_data_roots(
    app: AppHandle,
    claude_config_dir: Option<String>,
    codex_home: Option<String>,
) -> AppResult<DataRoots> {
    // Checked before taking the writer: canonicalize stats the path, which can stall on an
    // offline network share. None = leave unchanged, "" = clear.
    let values = blocking(move || {
        let check = |value: Option<String>| -> anyhow::Result<Option<String>> {
            value
                .map(|v| match v.trim() {
                    "" => Ok(String::new()),
                    raw => indexer::claude::normalize_data_root(raw),
                })
                .transpose()
        };
        Ok([
            (
                indexer::claude::CONFIG_DIR_SETTING,
                check(claude_config_dir)?,
            ),
            (indexer::codex::CODEX_HOME_SETTING, check(codex_home)?),
        ])
    })
    .await?;
    write_blocking(&app, move |conn| {
        for (key, value) in values {
            let Some(value) = value else { continue };
            if value.is_empty() {
                knowledge::app_config::delete(conn, key)?;
            } else {
                knowledge::app_config::put(conn, key, &value)?;
            }
        }
        indexer::claude::load_config_dir_setting(conn);
        indexer::codex::load_codex_home_setting(conn);
        data_roots(conn)
    })
    .await
}

/// Kick off a re-index of every source in the BACKGROUND on a dedicated connection, so
/// the button returns instantly and the UI stays responsive. Emits `index:done` (with
/// the report) when finished, or `index:error` when the scan failed; no-op if one is
/// already running.
#[tauri::command]
fn index_all(app: AppHandle) -> AppResult<()> {
    // Note: a running distill does NOT block reindex — the distill loop yields to us.
    let Some(job) = JobGuard::try_acquire(&app.state::<IndexJob>().0) else {
        return Ok(()); // already running
    };
    // Claim first, then check, so a semantic build starting meanwhile sees IndexJob and
    // backs off. A VACUUM or a build the user started is writing: don't fight it for the
    // lock (`job` drops). A background top-up stops after its batch instead: before, a
    // Reindex during one silently did nothing.
    if app.state::<CompactJob>().0.is_set() || !request_embed_yield(&app) {
        return Ok(());
    }
    std::thread::spawn(move || {
        if !wait_for_embed_yield(&app) {
            app_log!("[index] the semantic top-up is still running after 10 s; reindexing anyway");
        }
        wait_for_catch_up(&app);
        let started = std::time::Instant::now();
        let clock = jobclock::JobClock::start();
        // Dedicated connection (not the shared Mutex<Connection>): the scan never holds
        // the lock every other UI query needs — WAL lets readers proceed while we write.
        let prog = app.clone();
        let report = db::open(&db::default_index_path()).and_then(|mut c| {
            // Estimate the total from the existing thread count: on a re-index that's
            // accurate (≈ one file per thread), giving a real %; on a first run it's 0, so
            // the UI falls back to an indeterminate bar until rows start landing.
            let total_est: i64 = c
                .query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0))
                .unwrap_or(0);
            let r = indexer::scan_all_with_progress(&mut c, |seen, current| {
                let _ = prog.emit(
                    "index:progress",
                    IndexProgressEvent {
                        done: seen as i64,
                        total: total_est,
                        current: current.to_string(),
                    },
                );
            })?;
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
        let report = match report {
            Ok((r, (sources, threads))) => {
                app_log!(
                    "[index] reindex: {} threads indexed, {} unchanged, {} errors in {}",
                    r.threads_indexed,
                    r.threads_skipped,
                    r.errors,
                    clock.thread_summary()
                );
                track(
                    &app,
                    telemetry::Event::index_completed(
                        sources.max(0) as usize,
                        threads.max(0) as usize,
                        started.elapsed(),
                    ),
                );
                r
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
                let _ = app.emit("index:error", format!("{e:#}"));
                indexer::IndexReport::default()
            }
        };
        let _ = app.emit("index:done", report);
        // Newly indexed threads may need distilling — drain them if auto-distill is on.
        auto_distill_kick(&app);
    });
    Ok(())
}

/// Whether a user-started re-index is in progress (for the Reindex button state). The
/// launch catch-up is not one (see [`CatchUpJob`]): a Reindex clicked during it waits for it.
#[tauri::command]
fn indexing_status(job: tauri::State<'_, IndexJob>) -> bool {
    job.0.is_set()
}

/// How long a user-started reindex waits for the launch catch-up before scanning anyway.
/// The catch-up measured 0.1-0.2 s warm and about 2 s with a changed Cursor DB; the scans
/// are idempotent, so going ahead past a stuck one only repeats work.
const CATCH_UP_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Block until the watcher's launch catch-up releases [`CatchUpJob`]. Call it holding
/// [`IndexJob`]: the catch-up claims its flag and then checks IndexJob, so once this claim
/// is visible no new catch-up starts, and only one already past that check is waited for.
fn wait_for_catch_up(app: &AppHandle) {
    let deadline = std::time::Instant::now() + CATCH_UP_WAIT;
    let catch_up = app.state::<CatchUpJob>();
    while catch_up.0.is_set() {
        if std::time::Instant::now() >= deadline {
            app_log!("[index] the launch catch-up is still running after 30 s; reindexing anyway");
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Index a single source by kind ("claude_code" | "codex" | "cursor" | ...). Runs off the
/// main thread and shares the re-index guard with `index_all`, so the two never scan the
/// same files into the writer at once.
#[tauri::command]
async fn index_source(app: AppHandle, kind: String) -> AppResult<indexer::IndexReport> {
    if app.state::<CompactJob>().0.is_set() {
        return Err(
            anyhow::anyhow!("The index is being rewritten. Try again when that finishes.").into(),
        );
    }
    let Some(job) = JobGuard::try_acquire(&app.state::<IndexJob>().0) else {
        return Err(
            anyhow::anyhow!("A re-index is already running. Try again when it finishes.").into(),
        );
    };
    let waiter = app.clone();
    let result = blocking(move || {
        wait_for_catch_up(&waiter);
        // The shared source registry, so a new or renamed source kind can't miss this command.
        let scan = indexer::scanner_for(&kind)
            .ok_or_else(|| anyhow::anyhow!("unknown source kind: {kind}"))?;
        let mut conn = db::open(&db::default_index_path())?;
        scan(&mut conn, &mut || {})
    })
    .await;
    // Like index_all: a distill drain that yielded to this scan resumes once it is released.
    drop(job);
    auto_distill_kick(&app);
    result
}

#[tauri::command]
async fn search_threads(
    pool: tauri::State<'_, db::ReadPool>,
    gens: tauri::State<'_, search::SearchGen>,
    app: AppHandle,
    query: String,
    filters: Option<SearchFilters>,
) -> AppResult<search::SearchResponse> {
    let filters = filters.unwrap_or_default();
    // Taken in invocation order, before the blocking pool: this interrupts the searches
    // that this keystroke made stale.
    let gen = gens.begin();
    let pool = pool.0.clone();
    blocking(move || {
        let pool = db::ReadPool(pool);
        let gens = app.state::<search::SearchGen>();
        let embedder = app.state::<embed::Embedder>();
        search::ui_search(&pool, &gens, gen, &embedder, &query, &filters)
    })
    .await
}

/// Fire-and-forget: the user turned semantic search on, so load the model (from the cache
/// only) and pull the vector pages in before their first hybrid query. Returns at once.
#[tauri::command]
fn warm_semantic(app: AppHandle) {
    tauri::async_runtime::spawn_blocking(move || {
        let embedder = app.state::<embed::Embedder>();
        if let Err(e) = search::warm_semantic(&embedder, &db::default_index_path()) {
            app_log!("[search] warm: {e:#}");
        }
    });
}

/// One message's full text: a tool row the thread viewer got as a preview (`get_thread`).
#[tauri::command]
async fn message_text(pool: tauri::State<'_, db::ReadPool>, id: i64) -> AppResult<String> {
    read_blocking(&pool, move |conn| {
        search::message_text(conn, id)?.ok_or_else(|| anyhow::anyhow!("message not found"))
    })
    .await
}

/// Embedding progress for the UI: (embedded, total_embeddable, job_running).
#[derive(Debug, Serialize)]
struct EmbedStatus {
    done: i64,
    total: i64,
    running: bool,
}

/// Pushed on each embedded batch so the UI can show progress WITHOUT polling
/// `embedding_status` (which runs two locked COUNT(*) scans). Counts are tracked
/// incrementally by the job, not re-queried.
#[derive(Clone, Serialize)]
struct EmbedProgressEvent {
    done: i64,
    total: i64,
}

/// Pushed before each source during a background reindex, to drive a progress bar.
#[derive(Clone, Serialize)]
struct IndexProgressEvent {
    done: i64,
    total: i64,
    current: String,
}

/// Pushed per thread during a project distill, to drive a progress bar.
#[derive(Clone, Serialize)]
struct DistillProgressEvent {
    done: i64,
    total: i64,
}

#[tauri::command]
async fn embedding_status(
    pool: tauri::State<'_, db::ReadPool>,
    job: tauri::State<'_, EmbedJob>,
) -> AppResult<EmbedStatus> {
    // While a build runs, answer from the job's in-memory counters (updated per batch)
    // instead of re-running two COUNT(*) scans on every poll. Before the job has taken
    // its first snapshot (total still 0) fall through to the live count.
    if job.is_running() {
        let total = job.total.load(Ordering::Relaxed);
        if total > 0 {
            return Ok(EmbedStatus {
                done: job.done.load(Ordering::Relaxed),
                total,
                running: true,
            });
        }
    }
    let (done, total) = read_blocking(&pool, embed::embed_progress).await?;
    Ok(EmbedStatus {
        done,
        total,
        running: job.is_running(),
    })
}

/// Run a short DB write, retrying briefly on transient "database is locked" (another
/// process — the MCP server, a second app instance — momentarily holds the write
/// lock) instead of failing the user's click. The mutex is released between tries.
/// Sleeps between attempts, so call it from a blocking context (see `write_retry_async`).
fn write_retry<T>(
    db: &db::Db,
    mut op: impl FnMut(&rusqlite::Connection) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let mut last: Option<anyhow::Error> = None;
    for attempt in 0..8 {
        let res = {
            let conn = lock_anyhow(db)?;
            op(&conn)
        };
        match res {
            Ok(v) => return Ok(v),
            Err(e) if db::is_busy(&e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(120 * (attempt + 1)));
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("database stayed locked")))
}

/// [`write_retry`] on the blocking pool, for commands.
async fn write_retry_async<T, F>(app: &AppHandle, op: F) -> AppResult<T>
where
    T: Send + 'static,
    F: FnMut(&rusqlite::Connection) -> anyhow::Result<T> + Send + 'static,
{
    let app = app.clone();
    blocking(move || write_retry(&app.state::<db::Db>(), op)).await
}

/// Embed pending recall facts (decisions / gotchas) if the embedding model is already on
/// this machine. A background fact embed never starts the 128 MB model download: that would
/// stall a "Remember" click or a distill for a minute with no explanation. Facts left
/// pending are embedded when the semantic-index build finishes, or at a later launch.
/// Blocking (inference); call from a worker thread.
fn embed_facts_if_model_ready(app: &AppHandle, tag: &str) {
    let embedder = app.state::<embed::Embedder>();
    if !(embedder.is_loaded() || embed::model_is_cached()) {
        return;
    }
    if let Err(e) = embed::embed_pending_facts(&app.state::<db::Db>(), &embedder) {
        app_log!("[{tag}] embed facts: {e}");
    }
}

/// Kick off (or no-op if already running) a background job that embeds all pending
/// messages batch-by-batch, releasing the DB lock between batches. Emits
/// `embed:progress`, `embed:error` (model unavailable / systemic failure) and `embed:done`.
#[tauri::command]
fn build_embeddings(app: AppHandle) -> AppResult<()> {
    start_embed_build(&app, EmbedTrigger::User);
    Ok(())
}

/// Who started a semantic-index build.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EmbedTrigger {
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
/// Returns whether it started.
fn start_embed_build(app: &AppHandle, trigger: EmbedTrigger) -> bool {
    // Note: a running distill does NOT block the embed build — the distill loop yields.
    let job = app.state::<EmbedJob>();
    let Some(running) = JobGuard::try_acquire(&job.running) else {
        if trigger == EmbedTrigger::User {
            job.user_requested.store(true, Ordering::SeqCst);
        }
        return false; // already running
    };
    job.cancel.store(false, Ordering::SeqCst);
    if trigger == EmbedTrigger::User {
        job.user_requested.store(true, Ordering::SeqCst);
    }
    // Claim first, then check, as the watcher's catch-up does: index_all and claim_rewrite
    // claim their flag and then look at EmbedJob, so each side sees the other and a build
    // never writes alongside a reindex it raced. (Checking first let both start.)
    if app.state::<IndexJob>().0.is_set()
        || app.state::<CatchUpJob>().0.is_set()
        || app.state::<CompactJob>().0.is_set()
    {
        // A reindex or a VACUUM is writing: don't fight it for the lock (`running` drops).
        job.user_requested.store(false, Ordering::SeqCst);
        return false;
    }
    // Fresh counters: embedding_status falls back to a live count until the job's
    // snapshot below lands.
    job.done.store(0, Ordering::Relaxed);
    job.total.store(0, Ordering::Relaxed);
    let app = app.clone();
    std::thread::spawn(move || {
        let job = app.state::<EmbedJob>();
        let mut background = trigger == EmbedTrigger::Background;
        loop {
            let clock = jobclock::JobClock::start();
            // The build's own model (see `Embedder::bulk_build`), dropped below with its
            // thread pool and the working memory its padded sub-batches grew. Queries keep
            // the shared 4-thread model, which the watcher's reaper unloads once idle.
            let bulk = app.state::<embed::Embedder>().bulk_build(background);
            let stats = run_embed_job(&app, &bulk, background);
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
                if let Err(e) = embed::embed_pending_facts(&app.state::<db::Db>(), &bulk) {
                    app_log!("[embed] embed facts: {e}");
                }
            }
            drop(bulk);
            let secs = clock.wall().as_secs_f64().max(0.001);
            app_log!(
                "[embed] {} build{}: {} messages, {} chunks in {} ({:.0} chunks/s)",
                if background { "background" } else { "user" },
                if cancelled {
                    " (stopped for a reindex or rewrite)"
                } else {
                    ""
                },
                stats.messages,
                stats.chunks,
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
                let maintenance = jobclock::JobClock::start();
                match db::open(&db::default_index_path()) {
                    Ok(mut conn) => finish_embed_build(&mut conn),
                    Err(e) => app_log!("[embed] post-build maintenance: {e:#}"),
                }
                if maintenance.wall() >= std::time::Duration::from_secs(1) {
                    app_log!(
                        "[embed] post-build maintenance: {}",
                        maintenance.thread_summary()
                    );
                }
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
        shrink_writer(&app);
        // A click between this and the release below is the one left unserved (it only sets
        // the flag); it makes the next top-up hand over to a full build at once.
        job.user_requested.store(false, Ordering::SeqCst);
        drop(running);
        let _ = app.emit("embed:done", ());
        // Like index_all: a distill drain that yielded to the build resumes now.
        auto_distill_kick(&app);
    });
    true
}

/// Release the shared writer's page cache (up to 64 MiB) after a burst of writes (an embed
/// build, a distill drain); it refills on demand. Skipped if the writer is busy.
fn shrink_writer(app: &AppHandle) {
    if let Some(db) = app.try_state::<db::Db>() {
        if let Ok(conn) = db.0.try_lock() {
            db::shrink_memory(&conn);
        }
    }
}

/// The watcher's 60 s tick: unload the shared model once it has been idle for
/// `embed::APP_IDLE_UNLOAD` (the next query reloads it in about 150 ms). Also during a
/// semantic build: the build has its own model, and keeping the shared one through a build
/// of hours held two models' weights, arenas and thread pools the whole time.
pub(crate) fn embed_reap_tick(app: &AppHandle) {
    app.state::<embed::Embedder>().unload_if_idle();
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
pub(crate) fn embed_kick(app: &AppHandle) -> EmbedKick {
    let paused = app
        .state::<EmbedJob>()
        .auto_paused_until
        .lock()
        .is_ok_and(|until| until.is_some_and(|t| std::time::Instant::now() < t));
    if paused || !embed::model_is_cached() {
        return EmbedKick::NotNeeded;
    }
    let Some(pool) = app.try_state::<db::ReadPool>() else {
        return EmbedKick::NotNeeded;
    };
    // Runs on the watcher thread: r2d2's default `get` waits up to 30 s when UI reads hold
    // every reader, and file events, catch-up retries and the reaper tick would wait too.
    // Busy re-arms the timer instead.
    let (has_vectors, pending) = match pool.0.get_timeout(std::time::Duration::from_millis(100)) {
        Ok(conn) => {
            let exists = |sql: &str| {
                conn.query_row(sql, [], |r| r.get::<_, bool>(0))
                    .unwrap_or(false)
            };
            (
                exists("SELECT EXISTS(SELECT 1 FROM vec_chunk_owner)"),
                exists(&messages_pending_embed_sql()),
            )
        }
        Err(_) => return EmbedKick::Busy,
    };
    if !(has_vectors && pending) {
        EmbedKick::NotNeeded
    } else if start_embed_build(app, EmbedTrigger::Background) {
        EmbedKick::Started
    } else {
        EmbedKick::Busy
    }
}

/// What `embed_kick` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EmbedKick {
    Started,
    /// No semantic index yet, no model on disk, or nothing waiting for vectors.
    NotNeeded,
    /// A reindex, catch-up, VACUUM or build holds the writer: the watcher retries later.
    Busy,
}

/// Index maintenance after a semantic build, on its own connection.
fn finish_embed_build(conn: &mut rusqlite::Connection) {
    // Safety net for #27: owner rows for any vectors written without one.
    if let Err(e) = cleanup::reconcile_vec_owners(conn) {
        app_log!("[embed] owner reconcile: {e}");
    }
    // Rebuild vec_chunks once deletes (0029's tool-vector purge) have left it mostly
    // holes: ~35% faster KNN. Two small COUNTs and a no-op when it is dense.
    match cleanup::compact_vec_chunks(conn) {
        Ok(Some(c)) => app_log!(
            "[embed] compacted {} vectors into {} chunks (was {})",
            c.vectors,
            c.chunks_after,
            c.chunks_before
        ),
        Ok(None) => {}
        Err(e) => app_log!("[embed] vector compaction: {e}"),
    }
    // Fold the WAL back after the build's many small writes (PASSIVE never blocks).
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");
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
fn run_embed_job(app: &AppHandle, embedder: &embed::Embedder, background: bool) -> EmbedRunStats {
    // Smaller batches keep each locked write + each inference short, so the UI
    // (and ambient-recall) stay snappy while the job runs in the background. 128 also
    // widens embed::infer_bulk's length-sort window: +9-15% chunks/s over 64, for a
    // locked store_batch write about twice as long.
    const BATCH: usize = 128;
    let limit = background.then_some(AUTO_EMBED_MAX_MESSAGES);
    let db = app.state::<db::Db>();
    let job = app.state::<EmbedJob>();
    let mut stats = EmbedRunStats::default();
    let reported = std::cell::Cell::new(false);
    let report = |msg: String| {
        if !background || job.user_requested.load(Ordering::SeqCst) {
            let _ = app.emit("embed:error", msg);
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
    let (mut done, total) = match db.0.lock() {
        Ok(conn) => embed::embed_progress(&conn).unwrap_or((0, 0)),
        Err(_) => (0, 0),
    };
    job.done.store(done, Ordering::Relaxed);
    job.total.store(total, Ordering::Relaxed);
    let progress = |done: i64| {
        job.done.store(done, Ordering::Relaxed);
        let _ = app.emit("embed:progress", EmbedProgressEvent { done, total });
    };

    // Consecutive batches we couldn't persist because another writer held the lock.
    // We re-queue instead of aborting (the manual Reindex is now mutually exclusive,
    // but the file watcher still indexes incrementally), bailing only after a long
    // standoff so a truly stuck writer can't spin us forever.
    let mut deferrals = 0u32;
    // Consecutive multi-message batches where EVERY message failed inference on its own:
    // systemic, not one bad input, so stop instead of marking the corpus.
    let mut all_failed_streak = 0u32;
    loop {
        if let Some(end) = interrupted() {
            stats.end = end;
            break;
        }
        // 1. Locked, fast: claim the next batch of pending messages.
        let rows = {
            let Ok(conn) = db.0.lock() else { break };
            match embed::pending_batch(&conn, BATCH) {
                Ok(r) => r,
                Err(e) => {
                    app_log!("[embed] {e}");
                    break;
                }
            }
        };
        if rows.is_empty() {
            stats.end = EmbedEnd::Drained;
            break;
        }
        // 2. UNLOCKED: inference. embed_rows retries a failing batch one message at a
        //    time, so only inputs that fail on their own land in `failed`; store_batch
        //    below marks them embedded without vectors (FTS still finds them).
        let batch = match embed::embed_rows(embedder, &rows) {
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
            std::thread::sleep(std::time::Duration::from_secs(1));
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
                let Ok(mut conn) = db.0.lock() else { break };
                embed::store_batch(&mut conn, &ids, &batch.owners, &batch.vecs)
            };
            match res {
                Ok(()) => {
                    stored = true;
                    break;
                }
                Err(e) if db::is_busy(&e) => {
                    std::thread::sleep(std::time::Duration::from_millis(300));
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
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
            continue; // re-queue this batch; don't kill the whole job
        }
        deferrals = 0;
        done += rows.len() as i64;
        progress(done);
        stats.messages += rows.len() as i64;
        stats.chunks += batch.owners.len();
        if limit.is_some_and(|l| stats.messages >= l) {
            stats.end = EmbedEnd::Capped;
            break;
        }
    }
    stats.reported = reported.get();
    stats
}

#[cfg(test)]
mod embed_build_tests {
    use super::{owes_full_build, EmbedEnd};

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

#[tauri::command]
async fn recent_threads(
    pool: tauri::State<'_, db::ReadPool>,
    filters: Option<SearchFilters>,
) -> AppResult<Vec<ThreadSummary>> {
    read_blocking(&pool, move |conn| {
        search::recent_threads(conn, &filters.unwrap_or_default())
    })
    .await
}

/// Thread detail for the viewer, tool rows as previews (`message_text` has the rest). Off
/// the main thread: a long session still serializes to a megabyte or more.
#[tauri::command]
async fn get_thread(
    pool: tauri::State<'_, db::ReadPool>,
    thread_id: i64,
) -> AppResult<Option<ThreadDetail>> {
    read_blocking(&pool, move |conn| {
        search::thread_detail_preview(conn, thread_id)
    })
    .await
}

/// Star or unstar a thread ("collections").
#[tauri::command]
async fn set_star(app: AppHandle, thread_id: i64, starred: bool) -> AppResult<()> {
    in_edit_order(
        &app,
        write_blocking(&app, move |conn| search::set_star(conn, thread_id, starred)),
    )
    .await
}

/// Replace a thread's tags with the given set.
#[tauri::command]
async fn set_thread_tags(app: AppHandle, thread_id: i64, tags: Vec<String>) -> AppResult<()> {
    let write = write_blocking(&app, move |conn| {
        let now = chrono::Utc::now().timestamp();
        search::set_thread_tags(conn, thread_id, &tags, now)
    });
    in_edit_order(&app, write).await
}

/// All tags in the index with their thread counts, for the filter chips.
#[tauri::command]
async fn list_tags(pool: tauri::State<'_, db::ReadPool>) -> AppResult<Vec<(String, i64)>> {
    read_blocking(&pool, search::list_tags).await
}

/// Open TODOs across the corpus (free heuristic knowledge tier), newest first,
/// optionally scoped to a project-path substring and/or a source kind.
#[tauri::command]
async fn list_open_todos(
    pool: tauri::State<'_, db::ReadPool>,
    query: Option<String>,
    project: Option<String>,
    source: Option<String>,
) -> AppResult<Vec<knowledge::TodoFact>> {
    read_blocking(&pool, move |conn| {
        knowledge::list_open_todos(
            conn,
            query.as_deref(),
            project.as_deref(),
            source.as_deref(),
            500,
        )
    })
    .await
}

/// Current distillation engine config (enabled + provider/model).
#[tauri::command]
async fn knowledge_config(
    pool: tauri::State<'_, db::ReadPool>,
) -> AppResult<knowledge::KnowledgeConfig> {
    read_blocking(&pool, knowledge::get_config).await
}

/// Read a provider's saved custom base URL ("" = unset, use the provider default). Non-secret
/// config; used for local Ollama pointed at a remote host and for Ollama Cloud / authed proxies.
#[tauri::command]
async fn provider_base_url(
    pool: tauri::State<'_, db::ReadPool>,
    provider: String,
) -> AppResult<String> {
    read_blocking(&pool, move |conn| {
        Ok(knowledge::get_provider_base_url(conn, &provider)?.unwrap_or_default())
    })
    .await
}

/// Persist a provider's custom base URL. A blank string clears it (falls back to the default).
#[tauri::command]
async fn set_provider_base_url(app: AppHandle, provider: String, url: String) -> AppResult<()> {
    let write = write_blocking(&app, move |conn| {
        knowledge::set_provider_base_url(conn, &provider, &url)
    });
    in_edit_order(&app, write).await
}

/// Toggle background auto-distillation. Turning it ON kicks an immediate drain.
#[tauri::command]
async fn set_auto_distill(app: AppHandle, on: bool) -> AppResult<()> {
    write_blocking(&app, move |conn| knowledge::set_auto_distill(conn, on)).await?;
    if on {
        auto_distill_kick(&app);
    }
    Ok(())
}

/// The background auto-distill budget, for Settings / a confirm dialog before turning it on:
/// how many threads are eligible (active in the last 30 days, idle for 30 minutes and not yet
/// distilled), the
/// per-day cap, and what is left of today's allowance.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AutoDistillBudget {
    eligible_threads: usize,
    daily_cap: i64,
    remaining_today: i64,
    recent_days: i64,
}

fn auto_distill_budget_now(conn: &rusqlite::Connection) -> anyhow::Result<AutoDistillBudget> {
    let now = chrono::Utc::now().timestamp();
    Ok(AutoDistillBudget {
        eligible_threads: knowledge::auto_distill_eligible_count(conn, now)?,
        daily_cap: knowledge::auto_distill_daily_cap(conn)?,
        remaining_today: knowledge::auto_distill_remaining_today(conn, now)?,
        recent_days: knowledge::AUTO_DISTILL_RECENT_DAYS,
    })
}

#[tauri::command]
async fn auto_distill_budget(pool: tauri::State<'_, db::ReadPool>) -> AppResult<AutoDistillBudget> {
    read_blocking(&pool, auto_distill_budget_now).await
}

/// Settings > Agents: whether Claude Code auto memory is imported into project memory
/// (`knowledge::harvest`), and how many visible facts it contributed.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HarvestSettings {
    claude_memory: bool,
    imported_facts: i64,
}

#[tauri::command]
async fn harvest_settings(pool: tauri::State<'_, db::ReadPool>) -> AppResult<HarvestSettings> {
    read_blocking(&pool, |conn| {
        Ok(HarvestSettings {
            claude_memory: knowledge::harvest::claude_memory_enabled(conn)?,
            imported_facts: knowledge::harvest::imported_count(conn)?,
        })
    })
    .await
}

/// Turn Claude Code auto-memory import on (imports now) or off (removes every imported fact).
#[tauri::command]
async fn set_harvest_claude_memory(app: AppHandle, on: bool) -> AppResult<()> {
    let now = chrono::Utc::now().timestamp();
    let report = write_blocking(&app, move |conn| {
        knowledge::harvest::set_claude_memory_enabled(conn, on, now)
    })
    .await?;
    harvest_landed(&app, report);
    Ok(())
}

/// Import Claude Code auto memory on `conn` (the watcher's writer: after its launch
/// catch-up, then on its minute tick). Cheap when no topic file changed: a stat per file
/// and no write. A busy index waits for the next tick.
pub(crate) fn harvest_claude_memory_tick(app: &AppHandle, conn: &rusqlite::Connection) {
    let now = chrono::Utc::now().timestamp();
    match knowledge::harvest::harvest_claude_memory(conn, now) {
        Ok(report) => harvest_landed(app, report),
        Err(e) if db::is_busy(&e) => {}
        Err(e) => app_log!("[harvest] claude memory: {e:#}"),
    }
}

/// Fold the value ledger's past days into `ledger.daily` on `conn` (the watcher's writer, at
/// launch and on its minute tick). A busy index waits for the next tick.
pub(crate) fn ledger_rollup_tick(conn: &rusqlite::Connection) {
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
fn harvest_landed(app: &AppHandle, report: knowledge::harvest::HarvestReport) {
    if !report.changed() {
        return;
    }
    app_log!(
        "[harvest] claude memory: {} imported, {} removed",
        report.imported,
        report.removed
    );
    let _ = app.emit("index:updated", ());
    if report.imported > 0 {
        let app = app.clone();
        std::thread::spawn(move || embed_facts_if_model_ready(&app, "harvest"));
    }
}

/// Set the per-day auto-distill allowance (0 pauses auto-distill without turning it off).
#[tauri::command]
async fn set_auto_distill_daily_cap(app: AppHandle, cap: i64) -> AppResult<AutoDistillBudget> {
    let budget = write_blocking(&app, move |conn| {
        knowledge::set_auto_distill_daily_cap(conn, cap)?;
        auto_distill_budget_now(conn)
    })
    .await?;
    // Raising the cap (from 0, or past what today already spent) resumes the drain at once,
    // the way turning auto-distill on does. A no-op when it is off or a job is running.
    if budget.remaining_today > 0 {
        auto_distill_kick(&app);
    }
    Ok(budget)
}

/// Enable/disable LLM distillation and pick the engine (Settings). Enabling is the user's
/// consent. Heuristic TODOs are not touched either way: they are always on now, so turning
/// distillation off no longer clears them and turning it on no longer backfills them (the
/// one-time backfill runs after the launch catch-up, `knowledge::backfill_todos_once`).
#[tauri::command]
async fn set_knowledge_config(
    app: AppHandle,
    enabled: bool,
    provider: Option<String>,
    model: Option<String>,
) -> AppResult<()> {
    let engine = provider.clone();
    let was_enabled = write_blocking(&app, move |conn| {
        let prev = knowledge::get_config(conn)?.enabled;
        knowledge::set_config(conn, enabled, provider.as_deref(), model.as_deref())?;
        Ok(prev)
    })
    .await?;
    if enabled && !was_enabled {
        track(
            &app,
            telemetry::Event::KnowledgeEnabled {
                engine: engine.unwrap_or_else(|| "auto".into()),
            },
        );
    }
    Ok(())
}

/// Cloud providers the memory setup card offers when their API key is saved, in the order
/// the legacy "first saved API key" engine picks them ([`SYNTH_MODELS`] minus keyless Ollama).
const KEY_ENGINES: &[(&str, &str)] = &[
    ("anthropic", "Anthropic"),
    ("openai", "OpenAI"),
    ("gemini", "Gemini"),
    ("openrouter", "OpenRouter"),
    ("ollama_cloud", "Ollama Cloud"),
];

const OLLAMA_DEFAULT_BASE: &str = "http://localhost:11434";

/// The memory setup card's engine list: both CLIs (detected or not), local Ollama, then one
/// entry per saved key. `ollama_models` is the probe's answer (None = no server answered).
fn memory_engines(
    cli: &[agent::CliEngine],
    ollama_models: Option<&[String]>,
    ollama_base: Option<&str>,
    keyed: &[&str],
) -> Vec<knowledge::MemoryEngine> {
    let mut out: Vec<knowledge::MemoryEngine> = cli
        .iter()
        .map(|c| knowledge::MemoryEngine {
            id: c.id.to_string(),
            label: c.label.to_string(),
            kind: "cli",
            detected: c.installed,
            detail: c.path.clone().unwrap_or_else(|| "not found".into()),
        })
        .collect();
    let base = ollama_base
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .unwrap_or(OLLAMA_DEFAULT_BASE);
    out.push(knowledge::MemoryEngine {
        id: "ollama".into(),
        label: "Ollama".into(),
        kind: "ollama",
        detected: ollama_models.is_some(),
        detail: match ollama_models {
            Some(m) if m.len() == 1 => format!("Running at {base}, 1 model"),
            Some(m) => format!("Running at {base}, {} models", m.len()),
            None => format!("Not running at {base}"),
        },
    });
    out.extend(
        KEY_ENGINES
            .iter()
            .filter(|(id, _)| keyed.contains(id))
            .map(|(id, label)| knowledge::MemoryEngine {
                id: id.to_string(),
                label: label.to_string(),
                kind: "key",
                detected: true,
                detail: "API key saved".into(),
            }),
    );
    out
}

/// Whether `provider` is an engine the memory setup card lists: a CLI or local Ollama
/// (listed even when not detected), or a cloud provider with a saved key.
fn memory_engine_listed(provider: &str) -> bool {
    agent::is_cli_provider(provider)
        || provider == "ollama"
        || KEY_ENGINES
            .iter()
            .any(|(id, _)| *id == provider && secrets::has_key(id))
}

/// The memory setup card: current config, the detected engines (CLIs on PATH, a local
/// Ollama answering within 500 ms, saved keys), the one to preselect, and the auto-distill
/// budget. The three probes run concurrently, so the card waits for the slowest, usually the
/// login-shell PATH lookup: `agent::resolve_cli_bin` caches a binary it found, but not a miss,
/// so on a machine with neither CLI every call runs the lookup again.
#[tauri::command]
async fn memory_setup(pool: tauri::State<'_, db::ReadPool>) -> AppResult<knowledge::MemorySetup> {
    let (cfg, ollama_base, eligible_threads, daily_cap) = read_blocking(&pool, |conn| {
        let now = chrono::Utc::now().timestamp();
        Ok((
            knowledge::get_config(conn)?,
            knowledge::get_provider_base_url(conn, "ollama")?,
            knowledge::auto_distill_eligible_count(conn, now)?,
            knowledge::auto_distill_daily_cap(conn)?,
        ))
    })
    .await?;
    // Keychain reads block (and may prompt once on macOS), so they leave the async runtime.
    let keyed = blocking(|| {
        Ok(KEY_ENGINES
            .iter()
            .map(|(id, _)| *id)
            .filter(|id| secrets::has_key(id))
            .collect::<Vec<&'static str>>())
    });
    let (cli, ollama, keyed) = tokio::join!(
        agent::cli_engines(),
        agent::probe_ollama(ollama_base.as_deref()),
        keyed
    );
    let engines = memory_engines(&cli, ollama.as_deref(), ollama_base.as_deref(), &keyed?);
    Ok(knowledge::MemorySetup {
        enabled: cfg.enabled,
        provider: cfg.provider,
        model: cfg.model,
        recommended: knowledge::recommended_engine(&engines),
        engines,
        eligible_threads,
        daily_cap,
    })
}

/// One-click memory consent from the setup card: stores the named engine (never the legacy
/// "first saved API key" None), turns distillation and auto-distill on, and kicks the drain
/// the way the Settings auto-distill switch does. Settings keeps `set_knowledge_config`.
/// Already on with this same engine and model, it changes nothing: a stale card (or a
/// second click) must not turn back on an auto-distill the user switched off. Any other
/// choice is the user's explicit pick, so it is applied, a legacy enabled config included.
#[tauri::command]
async fn enable_memory(
    app: AppHandle,
    provider: String,
    model: Option<String>,
) -> AppResult<knowledge::KnowledgeConfig> {
    let check = provider.clone();
    if !blocking(move || Ok(memory_engine_listed(&check))).await? {
        return Err(anyhow::anyhow!(
            "{provider} is not an available memory engine: pick one the card lists, or save its API key in Settings > Providers"
        )
        .into());
    }
    let engine = provider.clone();
    let (was_enabled, applied, cfg) = write_blocking(&app, move |conn| {
        let prev = knowledge::get_config(conn)?;
        let model = model.as_deref().map(str::trim).filter(|m| !m.is_empty());
        if enable_memory_is_noop(&prev, &provider, model) {
            return Ok((true, false, prev));
        }
        knowledge::enable_memory(conn, &provider, model)?;
        Ok((prev.enabled, true, knowledge::get_config(conn)?))
    })
    .await?;
    if !was_enabled {
        track(&app, telemetry::Event::KnowledgeEnabled { engine });
    }
    if applied {
        auto_distill_kick(&app);
    }
    Ok(cfg)
}

/// Whether [`enable_memory`] has nothing to do: distillation is on with exactly this engine
/// and model.
fn enable_memory_is_noop(
    cfg: &knowledge::KnowledgeConfig,
    provider: &str,
    model: Option<&str>,
) -> bool {
    cfg.enabled && cfg.provider.as_deref() == Some(provider) && cfg.model.as_deref() == model
}

/// Cached distilled knowledge (summary/decisions/gotchas/todos) for one thread.
#[tauri::command]
async fn thread_knowledge(
    pool: tauri::State<'_, db::ReadPool>,
    thread_id: i64,
) -> AppResult<knowledge::ThreadKnowledge> {
    read_blocking(&pool, move |conn| {
        knowledge::get_thread_knowledge(conn, thread_id)
    })
    .await
}

/// First line of an LLM/provider error, capped, for storing on the thread. The raw error
/// can echo HTTP status, URLs and response bodies.
fn short_error(e: &anyhow::Error) -> String {
    e.to_string()
        .lines()
        .next()
        .unwrap_or("distillation failed")
        .chars()
        .take(160)
        .collect()
}

/// Distill one thread now (decisions/gotchas/summary) using the configured engine, and
/// return the fresh knowledge. The LLM call runs WITHOUT the DB lock held, and the DB and
/// keychain work runs on the blocking pool.
#[tauri::command]
async fn distill_thread(
    app: AppHandle,
    pool: tauri::State<'_, db::ReadPool>,
    thread_id: i64,
) -> AppResult<knowledge::ThreadKnowledge> {
    // Resolve engine + pack the transcript on a read connection.
    let (provider, model, key, base_url, packed) = read_blocking(&pool, move |conn| {
        let (provider, model, key, base_url) = resolve_distill_engine(conn)?;
        let packed = context::pack_thread(conn, thread_id, context::DEFAULT_BUDGET_CHARS)?
            .ok_or_else(|| anyhow::anyhow!("thread {thread_id} not found"))?;
        Ok((provider, model, key, base_url, packed))
    })
    .await?;

    let outcome = agent::distill(
        &provider,
        &model,
        key.as_deref(),
        base_url.as_deref(),
        &packed,
    )
    .await;
    blocking(move || {
        let db = app.state::<db::Db>();
        match outcome {
            Ok(distilled) => {
                {
                    let mut conn = lock_anyhow(&db)?;
                    let now = chrono::Utc::now().timestamp();
                    knowledge::store_distilled(&mut conn, thread_id, &distilled, now)?;
                }
                // Embed the new facts so they're recallable across threads (lock released
                // during inference). Best-effort: recall just lags if it fails.
                embed_facts_if_model_ready(&app, "knowledge");
                track(&app, telemetry::Event::distill_completed(1));
                let conn = lock_anyhow(&db)?;
                knowledge::get_thread_knowledge(&conn, thread_id)
            }
            Err(e) => {
                let conn = lock_anyhow(&db)?;
                knowledge::set_error(&conn, thread_id, &short_error(&e))?;
                Err(e)
            }
        }
    })
    .await
}

// ---- fact curation (pin / edit / hide) ----

/// Pin or unpin a distilled fact (pinned ranks first + survives re-distill).
#[tauri::command]
async fn set_fact_pinned(app: AppHandle, fact_id: i64, pinned: bool) -> AppResult<()> {
    let write = write_retry_async(&app, move |conn| {
        knowledge::set_fact_pinned(conn, fact_id, pinned)
    });
    in_edit_order(&app, write).await
}

/// Hide (soft-delete) or restore a fact. Hidden facts are kept as a tombstone so
/// re-distillation won't bring them back.
#[tauri::command]
async fn hide_fact(app: AppHandle, fact_id: i64, hidden: bool) -> AppResult<()> {
    let write = write_retry_async(&app, move |conn| {
        knowledge::set_fact_hidden(conn, fact_id, hidden)
    });
    in_edit_order(&app, write).await
}

/// Mark a TODO done (drops out of open lists) or reopen it.
#[tauri::command]
async fn set_todo_done(app: AppHandle, fact_id: i64, done: bool) -> AppResult<()> {
    let write = write_retry_async(&app, move |conn| {
        knowledge::set_todo_done(conn, fact_id, done)
    });
    in_edit_order(&app, write).await
}

/// Write-back: record a decision or gotcha for a project, then embed it for recall. A fact
/// the user types here is trusted, so it is pinned (`FactOrigin::User`); agent write-backs
/// through MCP / `cal remember` are not.
#[tauri::command]
async fn remember(app: AppHandle, project: String, kind: String, text: String) -> AppResult<()> {
    blocking(move || {
        let db = app.state::<db::Db>();
        write_retry(&db, |conn| {
            knowledge::record_fact_with_origin(
                conn,
                &project,
                &kind,
                &text,
                None,
                knowledge::FactOrigin::User,
                chrono::Utc::now().timestamp(),
            )
            .map(|_| ())
        })?;
        embed_facts_if_model_ready(&app, "remember");
        Ok(())
    })
    .await
}

/// Edit a fact's text. Re-embeds it so cross-thread recall matches the new wording.
#[tauri::command]
async fn edit_fact(app: AppHandle, fact_id: i64, text: String) -> AppResult<()> {
    let write = write_retry_async(&app, move |conn| knowledge::edit_fact(conn, fact_id, &text));
    in_edit_order(&app, write).await?;
    // Re-embed outside the edit queue, so model inference doesn't hold up the next edit.
    blocking(move || {
        embed_facts_if_model_ready(&app, "knowledge");
        Ok(())
    })
    .await
}

/// A pair of distilled decisions the LLM flagged as conflicting or superseding.
#[derive(Debug, Serialize)]
struct Conflict {
    #[serde(rename = "aId")]
    a_id: i64,
    #[serde(rename = "aText")]
    a_text: String,
    #[serde(rename = "bId")]
    b_id: i64,
    #[serde(rename = "bText")]
    b_text: String,
    reason: String,
}

/// Review a project's distilled decisions for conflicts / supersessions via the LLM.
#[tauri::command]
async fn detect_conflicts(
    pool: tauri::State<'_, db::ReadPool>,
    project: String,
) -> AppResult<Vec<Conflict>> {
    let (provider, model, key, base_url, decisions) = read_blocking(&pool, move |conn| {
        let (provider, model, key, base_url) = resolve_distill_engine(conn)?;
        let decisions = knowledge::project_decisions(conn, &project)?;
        Ok((provider, model, key, base_url, decisions))
    })
    .await?;
    if decisions.len() < 2 {
        return Ok(Vec::new());
    }
    let texts = conflict_review_texts(&decisions);
    let pairs = agent::find_conflicts(
        &provider,
        &model,
        key.as_deref(),
        base_url.as_deref(),
        &texts,
    )
    .await?;
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for p in pairs {
        if p.a < 1 || p.b < 1 || p.a > decisions.len() || p.b > decisions.len() || p.a == p.b {
            continue;
        }
        let (a, b) = if p.a < p.b { (p.a, p.b) } else { (p.b, p.a) };
        if !seen.insert((a, b)) {
            continue;
        }
        let (a_id, a_text) = decisions[a - 1].clone();
        let (b_id, b_text) = decisions[b - 1].clone();
        out.push(Conflict {
            a_id,
            a_text,
            b_id,
            b_text,
            reason: p.reason,
        });
    }
    Ok(out)
}

/// The decision texts the conflict review sends to the LLM, redacted (facts are stored as
/// distilled from raw transcripts). The returned conflicts still carry the stored text: they
/// only go to the app's own UI.
fn conflict_review_texts(decisions: &[(i64, String)]) -> Vec<String> {
    decisions
        .iter()
        .map(|(_, t)| context::redact_secrets(t).into_owned())
        .collect()
}

// ---- project memory ----

/// Distinct projects with distillation coverage, for the Project Memory picker.
#[tauri::command]
async fn list_projects(
    pool: tauri::State<'_, db::ReadPool>,
) -> AppResult<Vec<knowledge::ProjectInfo>> {
    read_blocking(&pool, knowledge::list_projects).await
}

/// Aggregated, durable memory (decisions/gotchas/open todos + coverage) for one project.
#[tauri::command]
async fn project_memory(
    pool: tauri::State<'_, db::ReadPool>,
    project: String,
) -> AppResult<knowledge::ProjectMemory> {
    read_blocking(&pool, move |conn| {
        knowledge::get_project_memory(conn, &project, 60)
    })
    .await
}

/// Flatten a project's facts into markdown notes for the LLM brief prompt, redacted (the
/// facts are stored unredacted and this text goes to the provider).
fn format_memory_notes(m: &knowledge::ProjectMemory) -> String {
    fn section(s: &mut String, title: &str, facts: &[knowledge::MemoryFact]) {
        if facts.is_empty() {
            return;
        }
        s.push_str(&format!("## {title}\n"));
        for f in facts {
            s.push_str(&format!("- {}\n", context::redact_secrets(f.text.trim())));
        }
        s.push('\n');
    }
    let mut s = String::new();
    section(&mut s, "Decisions", &m.decisions);
    section(&mut s, "Gotchas", &m.gotchas);
    section(&mut s, "Open TODOs", &m.open_todos);
    s
}

/// LLM-synthesized orientation brief from a project's aggregated facts. Needs distillation
/// enabled (it reuses the distill engine). Returns "" if there are no facts yet.
#[tauri::command]
async fn project_brief(pool: tauri::State<'_, db::ReadPool>, project: String) -> AppResult<String> {
    let key_project = project.clone();
    let (provider, model, key, base_url, notes) = read_blocking(&pool, move |conn| {
        let (provider, model, key, base_url) = resolve_distill_engine(conn)?;
        let mem = knowledge::get_project_memory_facts(conn, &key_project, 80)?;
        Ok((provider, model, key, base_url, format_memory_notes(&mem)))
    })
    .await?;
    if notes.trim().is_empty() {
        return Ok(String::new());
    }
    Ok(agent::project_brief(
        &provider,
        &model,
        key.as_deref(),
        base_url.as_deref(),
        &project,
        &notes,
    )
    .await?)
}

/// Write a project's memory (optionally with an LLM brief) to `<project>/.callimachus/
/// memory.md` so agents can be pointed at it. Returns the written path. Imported Claude Code
/// auto memory is left out: it is private to `~/.claude` and often about the user ("User
/// dislikes ..."), while this file sits in the repo where it can be committed and pushed.
#[tauri::command]
async fn write_project_memory_file(
    pool: tauri::State<'_, db::ReadPool>,
    project: String,
    with_brief: bool,
) -> AppResult<String> {
    let key_project = project.clone();
    let (mem, engine) = read_blocking(&pool, move |conn| {
        let mem = knowledge::get_project_memory_excluding(
            conn,
            &key_project,
            200,
            knowledge::harvest::EXTRACTOR_CLAUDE_MEMORY,
        )?;
        let engine = if with_brief {
            resolve_distill_engine(conn).ok()
        } else {
            None
        };
        Ok((mem, engine))
    })
    .await?;
    let brief = match engine {
        Some((provider, model, key, base_url)) => {
            let notes = format_memory_notes(&mem);
            if notes.trim().is_empty() {
                None
            } else {
                agent::project_brief(
                    &provider,
                    &model,
                    key.as_deref(),
                    base_url.as_deref(),
                    &project,
                    &notes,
                )
                .await
                .ok()
            }
        }
        None => None,
    };
    blocking(move || {
        let md = export::project_memory_md(&project, &mem, brief.as_deref());
        let dir = std::path::Path::new(&project).join(".callimachus");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("memory.md");
        std::fs::write(&path, md)?;
        Ok(path.to_string_lossy().into_owned())
    })
    .await
}

/// Write/refresh the Callimachus memory block in a project's agent context file (AGENTS.md
/// / CLAUDE.md), preserving the user's own content. So any agent that reads the file opens
/// with the project's distilled memory. `project` is the canonical key (a repo path).
///
/// The block is budgeted (it is committed and shipped to teammates), and the shared writer
/// refuses a file it can't read as UTF-8 instead of replacing it, writes through symlinks,
/// and swaps the file in atomically.
#[tauri::command]
async fn write_agent_memory_file(
    pool: tauri::State<'_, db::ReadPool>,
    project: String,
    filename: String,
) -> AppResult<String> {
    read_blocking(&pool, move |conn| {
        let budget = export::MemoryBudget::AGENTS_FILE;
        // Claude Code loads its own auto memory and reads this file too: no second copy.
        let mem = knowledge::get_project_memory_facts_excluding(
            conn,
            &project,
            budget.fetch_limit(),
            knowledge::harvest::EXTRACTOR_CLAUDE_MEMORY,
        )?;
        let body = export::agent_memory_md_budgeted(&project, &mem, None, &budget);
        let path = std::path::Path::new(&project).join(&filename);
        export::write_managed_block_file(&path, &body)?;
        Ok(path.to_string_lossy().into_owned())
    })
    .await
}

/// Whether a project distill is in progress (for the Build-memory button state).
#[tauri::command]
fn distilling_status(job: tauri::State<'_, DistillJob>) -> bool {
    job.0.is_set()
}

/// Stop a running project distill at the next thread boundary.
#[tauri::command]
fn cancel_distill(job: tauri::State<'_, DistillJob>) {
    job.0.clear();
}

/// Threads per auto-distill round before re-checking the gates (so a reindex/embed can
/// preempt promptly, and the first run on a big history doesn't commit to it all at once).
const AUTO_DISTILL_BATCH: i64 = 25;

/// Distill every not-yet-distilled thread in a project, in the BACKGROUND, so the project's
/// memory fills in. Paced + cancellable; yields to a reindex / embed build if one starts.
/// Emits distill:progress / distill:done.
#[tauri::command]
fn distill_project(app: AppHandle, project: String) -> AppResult<()> {
    if writer_job_running(&app) {
        return Ok(()); // a reindex / embed build / VACUUM is writing: start later
    }
    let Some(job) = JobGuard::try_acquire(&app.state::<DistillJob>().0) else {
        return Ok(()); // already running
    };
    // The guard moves into the task: tokio drops a panicked task's future, which clears it.
    tauri::async_runtime::spawn(async move {
        let worklist_app = app.clone();
        let ids = blocking(move || {
            let pool = worklist_app.state::<db::ReadPool>();
            let conn = read(&pool).map_err(error::AppError::into_inner)?;
            knowledge::project_pending_threads(&conn, &project)
        })
        .await;
        match ids {
            Ok(ids) => {
                let started = std::time::Instant::now();
                let queued = ids.len();
                match run_distill(&app, &job, ids).await {
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
        shrink_writer(&app);
        let _ = app.emit("distill:done", ());
    });
    Ok(())
}

/// Whether a job that holds the write lock for long stretches is running: a reindex (or the
/// watcher's catch-up), a semantic build, or a whole-file rewrite. Distill waits for these.
fn writer_job_running(app: &AppHandle) -> bool {
    app.state::<IndexJob>().0.is_set()
        || app.state::<CatchUpJob>().0.is_set()
        || app.state::<EmbedJob>().is_running()
        || app.state::<CompactJob>().0.is_set()
}

/// Distill a worklist of thread ids with the resolved engine. Paced (so a cloud provider
/// isn't hammered), cancellable (`job` stops being current once cancel_distill frees the
/// flag), and yields to a reindex / embed build / VACUUM. Shared by the project distill and
/// the background auto-distill. The DB, keychain and embedding work runs on the blocking
/// pool; only the LLM call is awaited on the async runtime.
/// Returns how many threads it distilled.
async fn run_distill(app: &AppHandle, job: &JobGuard, ids: Vec<i64>) -> anyhow::Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let to_anyhow = error::AppError::into_inner;
    let pool = app.state::<db::ReadPool>();
    let (provider, model, key, base_url) = read_blocking(&pool, resolve_distill_engine)
        .await
        .map_err(to_anyhow)?;
    let total = ids.len() as i64;
    let mut distilled_count = 0usize;
    let _ = app.emit("distill:progress", DistillProgressEvent { done: 0, total });
    for (i, tid) in ids.into_iter().enumerate() {
        // Stop if canceled (even when a newer distill has claimed the flag since), or yield
        // to a reindex / embed build / VACUUM.
        if !job.is_current() || writer_job_running(app) {
            break;
        }
        let packed = read_blocking(&pool, move |conn| {
            context::pack_thread(conn, tid, context::DEFAULT_BUDGET_CHARS)
        })
        .await
        .map_err(to_anyhow)?;
        let Some(packed) = packed else { continue };
        let outcome = agent::distill(
            &provider,
            &model,
            key.as_deref(),
            base_url.as_deref(),
            &packed,
        )
        .await;
        let stored = {
            let app = app.clone();
            blocking(move || {
                let db = app.state::<db::Db>();
                match outcome {
                    Ok(result) => {
                        {
                            let mut conn = lock_anyhow(&db)?;
                            knowledge::store_distilled(
                                &mut conn,
                                tid,
                                &result,
                                chrono::Utc::now().timestamp(),
                            )?;
                        }
                        embed_facts_if_model_ready(&app, "distill");
                        Ok(true)
                    }
                    Err(e) => {
                        let conn = lock_anyhow(&db)?;
                        let _ = knowledge::set_error(&conn, tid, &short_error(&e));
                        Ok(false)
                    }
                }
            })
            .await
            .map_err(to_anyhow)?
        };
        if stored {
            distilled_count += 1;
        }
        let _ = app.emit(
            "distill:progress",
            DistillProgressEvent {
                done: (i as i64) + 1,
                total,
            },
        );
        // Gentle pacing so a cloud provider isn't hammered.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    if distilled_count > 0 {
        track(app, telemetry::Event::distill_completed(distilled_count));
    }
    Ok(distilled_count)
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
fn auto_distill_kick(app: &AppHandle) -> bool {
    if writer_job_running(app) {
        return false;
    }
    // Own the distill job BEFORE building the worklist: building it spends today's allowance,
    // so a kick that only then found a distill running (a manual "Build memory", an auto
    // batch in flight) burned up to a batch of the daily cap without distilling anything.
    let Some(job) = JobGuard::try_acquire(&app.state::<DistillJob>().0) else {
        return false; // a distill is already running
    };
    let ids = (|| {
        let db = app.try_state::<db::Db>()?;
        let conn = db.0.lock().ok()?;
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
        return false; // `job` drops here, releasing the flag
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        // Wall time only: the round is mostly waiting on the LLM, and its CPU (the local
        // engine's process, the fact embeds) lands on other threads and processes.
        let started = std::time::Instant::now();
        let queued = ids.len();
        match run_distill(&app, &job, ids).await {
            Ok(n) => app_log!(
                "[auto-distill] round: {n} of {queued} threads distilled in {}",
                jobclock::fmt_duration(started.elapsed())
            ),
            Err(e) => app_log!("[auto-distill] {e}"),
        }
        // A Cancel ends the drain; re-kicking would start the next batch right away.
        let cancelled = !job.is_current();
        // Released before the re-kick below, which would otherwise find the flag still held.
        drop(job);
        let _ = app.emit("distill:done", ());
        // More may remain (and nothing preempted us) — drain the next batch.
        if cancelled || !auto_distill_kick(&app) {
            // The drain ended (nothing left, the day's budget spent, a cancel, or a writer
            // job preempted it): hand back the writer's page cache.
            shrink_writer(&app);
        }
    });
    true
}

/// Cross-thread semantic recall of distilled DECISIONS, matched to a query.
#[tauri::command]
async fn recall_decisions(
    app: AppHandle,
    query: String,
    project: Option<String>,
    limit: Option<u32>,
) -> AppResult<Vec<knowledge::RecallHit>> {
    recall_facts(app, "decision", query, project, limit).await
}

/// Cross-thread semantic recall of distilled GOTCHAS, matched to a query.
#[tauri::command]
async fn recall_gotchas(
    app: AppHandle,
    query: String,
    project: Option<String>,
    limit: Option<u32>,
) -> AppResult<Vec<knowledge::RecallHit>> {
    recall_facts(app, "gotcha", query, project, limit).await
}

/// "Have I done this before?" — prior SESSIONS similar to a task description, each rolled up
/// from its matching decisions/gotchas. Embed OUTSIDE the DB, then KNN on the read pool.
/// Cached-only: never waits on or starts a model download (empty without a model).
#[tauri::command]
async fn find_prior_work(
    app: AppHandle,
    query: String,
    project: Option<String>,
    limit: Option<u32>,
) -> AppResult<Vec<knowledge::PriorWork>> {
    blocking(move || {
        let embedder = app.state::<embed::Embedder>();
        let Some(qv) = embed::embed_query_cached_only(&embedder, &query)? else {
            return Ok(Vec::new());
        };
        let pool = app.state::<db::ReadPool>();
        let conn = read(&pool).map_err(error::AppError::into_inner)?;
        knowledge::find_prior_work(&conn, &qv, project.as_deref(), limit.unwrap_or(8) as usize)
    })
    .await
}

/// Shared recall path: embed the query OUTSIDE the DB (cached-only), then run the SQL KNN
/// on the pool.
async fn recall_facts(
    app: AppHandle,
    kind: &'static str,
    query: String,
    project: Option<String>,
    limit: Option<u32>,
) -> AppResult<Vec<knowledge::RecallHit>> {
    blocking(move || {
        let embedder = app.state::<embed::Embedder>();
        let Some(qv) = embed::embed_query_cached_only(&embedder, &query)? else {
            return Ok(Vec::new());
        };
        let pool = app.state::<db::ReadPool>();
        let conn = read(&pool).map_err(error::AppError::into_inner)?;
        knowledge::recall(
            &conn,
            &qv,
            kind,
            project.as_deref(),
            limit.unwrap_or(20) as usize,
        )
    })
    .await
}

/// A thread cited as a source in an "ask your history" answer.
#[derive(Debug, Clone, Serialize)]
pub struct AskSource {
    #[serde(rename = "threadId")]
    pub thread_id: i64,
    pub title: Option<String>,
    pub source: String,
    #[serde(rename = "projectPath")]
    pub project_path: Option<String>,
}

#[derive(Debug, Serialize)]
struct AskAnswer {
    answer: String,
    sources: Vec<AskSource>,
}

/// Prepared RAG inputs: the resolved engine + packed cited context, ready for the LLM.
pub struct AskPrep {
    pub provider: String,
    pub model: String,
    pub key: Option<String>,
    pub base_url: Option<String>,
    pub context: String,
    pub sources: Vec<AskSource>,
}

/// Chars of each cited thread packed into an Ask prompt (up to 5 threads).
const ASK_THREAD_BUDGET: usize = 2_500;

/// Retrieve + pack the top threads for a question and resolve the distill engine. SYNC (no
/// LLM call) so callers run it under a lock, drop the conn, then await `agent::answer`.
/// `qv` is the PRE-EMBEDDED query vector (embed before locking). None = no engine or no
/// relevant threads. Shared by the desktop command, `cal ask`, and the MCP ask tool.
///
/// Each thread is packed AROUND its matching messages (plus the turns next to them and the
/// closing turns), so the excerpt holds the answer, not just the opening prompts.
pub fn prepare_ask(
    conn: &rusqlite::Connection,
    question: &str,
    qv: Option<&[f32]>,
) -> anyhow::Result<Option<AskPrep>> {
    let (provider, model, key, base_url) = resolve_distill_engine(conn)?;
    let filters = SearchFilters {
        limit: Some(30),
        ..Default::default()
    };
    let hits = search::hybrid_vec(conn, question, qv, &filters)?;
    let mut focus: std::collections::HashMap<i64, Vec<i64>> = std::collections::HashMap::new();
    for h in &hits {
        focus.entry(h.thread_id).or_default().push(h.message_id);
    }
    let mut seen = std::collections::HashSet::new();
    let mut sources: Vec<AskSource> = Vec::new();
    let mut context = String::new();
    for h in &hits {
        if !seen.insert(h.thread_id) {
            continue;
        }
        if sources.len() >= 5 {
            break;
        }
        let excerpt = context::pack_thread_around(
            conn,
            h.thread_id,
            ASK_THREAD_BUDGET,
            focus.get(&h.thread_id).map(Vec::as_slice).unwrap_or(&[]),
        )?
        .unwrap_or_default();
        // The excerpt is packed (so redacted); the title heading it comes straight from the
        // index and goes to the provider too.
        context.push_str(&format!(
            "[thread {}] {}\n{excerpt}\n\n",
            h.thread_id,
            context::redact_secrets(h.title.as_deref().unwrap_or("untitled"))
        ));
        sources.push(AskSource {
            thread_id: h.thread_id,
            title: h.title.clone(),
            source: h.source.clone(),
            project_path: h.project_path.clone(),
        });
    }
    if sources.is_empty() {
        return Ok(None);
    }
    Ok(Some(AskPrep {
        provider,
        model,
        key,
        base_url,
        context,
        sources,
    }))
}

/// "Ask your history" (RAG): retrieve the most relevant threads for a question, pack
/// excerpts, and have the configured LLM answer with [thread N] citations.
#[tauri::command]
async fn ask_history(app: AppHandle, question: String) -> AppResult<AskAnswer> {
    let q = question.trim().to_string();
    if q.is_empty() {
        return Err(anyhow::anyhow!("ask needs a question").into());
    }
    // Embed (cached-only: keyword-only when the model isn't usable) and retrieve + pack on
    // the blocking pool; the LLM call runs after the connection is returned.
    let prep_q = q.clone();
    let prep = blocking(move || {
        let embedder = app.state::<embed::Embedder>();
        let qv = embed::embed_query_cached_only(&embedder, &prep_q).unwrap_or_else(|e| {
            app_log!("[ask] semantic arm skipped: {e}");
            None
        });
        let pool = app.state::<db::ReadPool>();
        let conn = read(&pool).map_err(error::AppError::into_inner)?;
        prepare_ask(&conn, &prep_q, qv.as_deref())
    })
    .await?;
    let Some(prep) = prep else {
        return Ok(AskAnswer {
            answer: "I couldn't find anything relevant in your history.".into(),
            sources: Vec::new(),
        });
    };
    let answer = agent::answer(
        &prep.provider,
        &prep.model,
        prep.key.as_deref(),
        prep.base_url.as_deref(),
        &q,
        &prep.context,
    )
    .await?;
    Ok(AskAnswer {
        answer,
        sources: prep.sources,
    })
}

/// Code-aware search: threads that mention a file path (substring, case-insensitive).
#[tauri::command]
async fn search_by_file(
    pool: tauri::State<'_, db::ReadPool>,
    path: String,
) -> AppResult<Vec<ThreadSummary>> {
    read_blocking(&pool, move |conn| {
        search::threads_with_file(conn, &path, 200)
    })
    .await
}

// ---- agent chat ----

/// Stream a chat completion. Tokens are pushed over `on_token`; the full reply is
/// returned and the conversation persisted as a searchable in_app thread.
#[tauri::command]
#[allow(clippy::too_many_arguments)] // Tauri injects app/generation/channel; the rest are the chat params
async fn send_chat(
    app: AppHandle,
    generation: tauri::State<'_, ChatGeneration>,
    on_token: tauri::ipc::Channel<agent::ChatChunk>,
    thread_id: String,
    provider: String,
    model: String,
    base_url: Option<String>,
    messages: Vec<agent::ChatMessage>,
) -> AppResult<String> {
    // CLI backends are keyless (they use the CLI's own logged-in auth); only keyed providers
    // need a stored key. The keychain can block (or prompt), so read it off the runtime.
    let key = if agent::is_cli_provider(&provider) {
        None
    } else {
        let p = provider.clone();
        blocking(move || secrets::get_key(&p)).await?
    };

    // Publish this stream's cancellation token. A second send supersedes (and stops) the
    // first instead of overwriting its token and leaving it unstoppable.
    let token = tokio_util::sync::CancellationToken::new();
    let my_id = generation.seq.fetch_add(1, Ordering::Relaxed) + 1;
    if let Some((_, prev)) = generation
        .slot
        .lock()
        .map_err(|e| anyhow::anyhow!("generation lock: {e}"))?
        .replace((my_id, token.clone()))
    {
        prev.cancel();
    }
    // Clear the slot on every exit path (including `?` early returns), but only while it
    // still holds THIS stream's token.
    struct ClearSlot<'a>(
        &'a Mutex<Option<(u64, tokio_util::sync::CancellationToken)>>,
        u64,
    );
    impl Drop for ClearSlot<'_> {
        fn drop(&mut self) {
            if let Ok(mut g) = self.0.lock() {
                if g.as_ref().is_some_and(|(id, _)| *id == self.1) {
                    *g = None;
                }
            }
        }
    }
    let _clear = ClearSlot(&generation.slot, my_id);

    // Tool executor runs through the app handle (so it can reach DB + approvals)
    // and the channel (to stream tool steps).
    let tool_app = app.clone();
    let tool_ch = on_token.clone();
    let full = agent::chat_stream(
        &provider,
        &model,
        base_url.as_deref(),
        key.as_deref(),
        &messages,
        agent::default_tools(),
        token,
        |kind, text| {
            let kind = match kind {
                agent::ChunkKind::Reasoning => "reasoning",
                agent::ChunkKind::Text => "text",
            };
            let _ = on_token.send(agent::ChatChunk::text(kind, text));
        },
        move |call| run_tool(tool_app.clone(), tool_ch.clone(), call),
    )
    .await?;
    let reply = full.clone();
    write_blocking(&app, move |conn| {
        agent::persist_chat(conn, &thread_id, &messages, &reply)
    })
    .await?;
    track(
        &app,
        telemetry::Event::ChatMessageSent {
            provider: provider.clone(),
        },
    );
    Ok(full)
}

/// Approve or deny a pending shell command the agent requested.
#[tauri::command]
fn approve_tool(
    approvals: tauri::State<'_, PendingApprovals>,
    tool_id: String,
    approved: bool,
) -> AppResult<()> {
    if let Some(tx) = approvals
        .0
        .lock()
        .map_err(|e| anyhow::anyhow!("approvals lock: {e}"))?
        .remove(&tool_id)
    {
        let _ = tx.send(approved);
    }
    Ok(())
}

/// Abort the in-flight chat stream (if any). The partial reply is still persisted. Any
/// shell command still waiting for approval is denied, so a stream parked on one can't
/// hang after the cancel.
#[tauri::command]
fn cancel_chat(
    generation: tauri::State<'_, ChatGeneration>,
    approvals: tauri::State<'_, PendingApprovals>,
) -> AppResult<()> {
    if let Some((_, token)) = generation
        .slot
        .lock()
        .map_err(|e| anyhow::anyhow!("generation lock: {e}"))?
        .take()
    {
        token.cancel();
    }
    approvals.deny_all();
    Ok(())
}

#[tauri::command]
async fn set_api_key(provider: String, key: String) -> AppResult<()> {
    blocking(move || secrets::set_key(&provider, &key)).await
}

#[tauri::command]
async fn delete_api_key(provider: String) -> AppResult<()> {
    blocking(move || secrets::delete_key(&provider)).await
}

#[tauri::command]
async fn provider_has_key(provider: String) -> AppResult<bool> {
    // Keyless engines (Ollama, CLI backends) have no key — answer without touching the keychain,
    // so selecting them never triggers a macOS keychain prompt.
    if is_keyless(&provider) {
        return Ok(false);
    }
    blocking(move || Ok(secrets::has_key(&provider))).await
}

/// List the models a provider currently offers (from its API). Free-text entry in
/// the UI still works; this populates the suggestions with real, current options.
#[tauri::command]
async fn list_models(provider: String, base_url: Option<String>) -> AppResult<Vec<String>> {
    if agent::is_cli_provider(&provider) {
        return Ok(agent::cli_models(&provider)); // keyless: suggest aliases, no API call
    }
    let p = provider.clone();
    let key = blocking(move || secrets::get_key(&p)).await?;
    Ok(agent::list_models(&provider, base_url.as_deref(), key.as_deref()).await?)
}

/// Smallest pack `thread_context` hands out; below this the budget ladder can't keep a
/// coherent head + tail.
const MIN_CONTEXT_BUDGET: usize = 4_000;

/// Pack a thread into an LLM-ready context blob (markdown + XML envelope, budgeted). Chat's
/// "Add to chat" asks for a compact pack (`budget_chars`), since it is re-sent every turn.
#[tauri::command]
async fn thread_context(
    pool: tauri::State<'_, db::ReadPool>,
    thread_id: i64,
    budget_chars: Option<usize>,
) -> AppResult<String> {
    let budget = budget_chars
        .map(|b| b.clamp(MIN_CONTEXT_BUDGET, context::DEFAULT_BUDGET_CHARS))
        .unwrap_or(context::DEFAULT_BUDGET_CHARS);
    read_blocking(&pool, move |conn| {
        context::pack_thread(conn, thread_id, budget)?
            .ok_or_else(|| anyhow::anyhow!("thread not found"))
    })
    .await
}

/// Obsidian-known vault folders, read from Obsidian's own config (no filesystem
/// scan). Checks the macOS / Linux / Windows config locations; only existing dirs
/// are returned. Empty when Obsidian isn't installed or registers no vaults.
#[tauri::command(async)]
fn obsidian_vaults() -> Vec<String> {
    let mut configs: Vec<std::path::PathBuf> = Vec::new();
    // Obsidian stores its config under the platform per-user config dir: macOS
    // `~/Library/Application Support/obsidian`, Windows `%APPDATA%\obsidian`,
    // Linux `~/.config/obsidian` — exactly `dirs::config_dir()`.
    if let Some(cfg) = dirs::config_dir() {
        configs.push(cfg.join("obsidian/obsidian.json"));
    }
    for cfg in configs {
        let Ok(text) = std::fs::read_to_string(&cfg) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let Some(vaults) = json.get("vaults").and_then(|v| v.as_object()) else {
            continue;
        };
        let mut paths: Vec<String> = vaults
            .values()
            .filter_map(|v| v.get("path").and_then(|p| p.as_str()))
            .filter(|p| std::path::Path::new(p).is_dir())
            .map(str::to_string)
            .collect();
        paths.sort();
        paths.dedup();
        if !paths.is_empty() {
            return paths;
        }
    }
    Vec::new()
}

/// Render `detail` (optionally with a synthesis block) to an Obsidian note inside
/// `vault_dir`, returning the written path. Shared by quick + synthesized export.
fn write_note(
    detail: &ThreadDetail,
    synthesis: Option<&str>,
    vault_dir: &str,
) -> anyhow::Result<String> {
    let md = export::to_obsidian(detail, synthesis);
    std::fs::create_dir_all(vault_dir)?;
    let path =
        std::path::Path::new(vault_dir).join(format!("{}.md", export::note_filename(detail)));
    std::fs::write(&path, md)?;
    Ok(path.to_string_lossy().into_owned())
}

/// Quick export: a thread → Obsidian note (transcript + `[[project]]` link), no LLM.
#[tauri::command]
async fn export_thread(
    app: AppHandle,
    pool: tauri::State<'_, db::ReadPool>,
    thread_id: i64,
    vault_dir: String,
) -> AppResult<String> {
    let path = read_blocking(&pool, move |conn| {
        let detail = search::thread_detail(conn, thread_id)?
            .ok_or_else(|| anyhow::anyhow!("thread not found"))?;
        write_note(&detail, None, &vault_dir)
    })
    .await?;
    track(
        &app,
        telemetry::Event::ExportObsidian { synthesized: false },
    );
    Ok(path)
}

/// Synthesis model per provider — the cheap/fast tier (this is summarization, not
/// hard reasoning). Default when a provider is pinned without a model, and the
/// auto-pick order for keyed providers.
const SYNTH_MODELS: &[(&str, &str)] = &[
    ("anthropic", "claude-haiku-4-5"),
    ("openai", "gpt-4o-mini"),
    ("gemini", "gemini-2.5-flash"),
    ("openrouter", "anthropic/claude-sonnet-4.6"),
    ("ollama", "llama3.1"),
    // Ollama Cloud (ollama.com). Small/cheap default; user-overridable in Settings.
    ("ollama_cloud", "gemma3:4b"),
];

/// First provider with a stored key (local `ollama` is keyless, so never auto-picked;
/// `ollama_cloud` is keyed, so it can be).
pub fn pick_synth_provider() -> Option<(&'static str, &'static str)> {
    SYNTH_MODELS
        .iter()
        .copied()
        .find(|(p, _)| secrets::has_key(p))
}

fn synth_default_model(provider: &str) -> Option<&'static str> {
    SYNTH_MODELS
        .iter()
        .find(|(p, _)| *p == provider)
        .map(|(_, m)| *m)
}

/// Keyless engines need no stored API key: local Ollama, and the CLI backends (which use the
/// user's logged-in `claude` / `codex` subscription).
fn is_keyless(provider: &str) -> bool {
    provider == "ollama" || agent::is_cli_provider(provider)
}

/// Resolve (provider, model) from an optional pinned choice, else auto-pick. A pinned KEYED
/// provider must have a stored key; keyless engines (Ollama, CLIs) don't. A CLI may run with an
/// empty model (the CLI picks its own default).
fn resolve_synth(provider: Option<&str>, model: Option<&str>) -> AppResult<(String, String)> {
    match provider.filter(|p| !p.is_empty()) {
        Some(p) => {
            if !is_keyless(p) && !secrets::has_key(p) {
                return Err(anyhow::anyhow!("no API key stored for {p}").into());
            }
            let model = model
                .filter(|m| !m.is_empty())
                .map(str::to_string)
                .or_else(|| synth_default_model(p).map(str::to_string))
                .unwrap_or_default();
            if model.is_empty() && !agent::is_cli_provider(p) {
                return Err(anyhow::anyhow!("pick a model for {p}").into());
            }
            Ok((p.to_string(), model))
        }
        None => {
            let (p, m) = pick_synth_provider().ok_or_else(|| {
                anyhow::anyhow!(
                    "the engine is the first saved API key, and none is saved: add one in Settings > Providers or pick an engine"
                )
            })?;
            Ok((p.to_string(), m.to_string()))
        }
    }
}

/// Whether any cloud provider key is stored — gates the "Synthesize" action.
#[tauri::command]
async fn can_synthesize() -> AppResult<bool> {
    blocking(|| Ok(pick_synth_provider().is_some())).await
}

/// The CLI LLM backends (Claude Code / Codex) and whether each is installed, so the distillation
/// settings can offer "use my CLI subscription instead of an API key".
#[tauri::command]
async fn cli_engines() -> Vec<agent::CliEngine> {
    agent::cli_engines().await
}

/// Resolve (provider, model, api_key, base_url) for distillation from the saved engine config,
/// gated on distillation being enabled. Shared by the Tauri command and `cal distill`. Local
/// Ollama is keyless; cloud providers (including Ollama Cloud) must have a stored key. `base_url`
/// is the provider's optional saved custom endpoint (local Ollama host, Ollama Cloud / proxy).
pub fn resolve_distill_engine(
    conn: &rusqlite::Connection,
) -> anyhow::Result<(String, String, Option<String>, Option<String>)> {
    let cfg = knowledge::get_config(conn)?;
    if !cfg.enabled {
        anyhow::bail!(
            "LLM distillation is off: turn on memory in Settings > Knowledge (a CLI subscription, local Ollama or an API key)"
        );
    }
    let (provider, model) = resolve_synth(cfg.provider.as_deref(), cfg.model.as_deref())
        .map_err(error::AppError::into_inner)?;
    let key = if is_keyless(&provider) {
        None
    } else {
        secrets::get_key(&provider)?
    };
    let base_url = knowledge::get_provider_base_url(conn, &provider)?;
    Ok((provider, model, key, base_url))
}

/// Synthesized export: pack the thread, run the chosen (or first available) LLM to
/// extract a summary + decisions / gotchas / TODOs, and write a knowledge-grade
/// Obsidian note (synthesis above the transcript) into `vault_dir`.
#[tauri::command]
async fn synthesize_export(
    app: AppHandle,
    pool: tauri::State<'_, db::ReadPool>,
    thread_id: i64,
    vault_dir: String,
    provider: Option<String>,
    model: Option<String>,
) -> AppResult<String> {
    // Pull detail + packed transcript (and the keychain key) on the blocking pool; the
    // connection is returned before the network call.
    let (provider, model, detail, packed, base_url, key) = read_blocking(&pool, move |conn| {
        let (provider, model) = resolve_synth(provider.as_deref(), model.as_deref())
            .map_err(error::AppError::into_inner)?;
        let detail = search::thread_detail(conn, thread_id)?
            .ok_or_else(|| anyhow::anyhow!("thread not found"))?;
        let packed = context::pack_thread(conn, thread_id, context::DEFAULT_BUDGET_CHARS)?
            .ok_or_else(|| anyhow::anyhow!("thread not found"))?;
        let base_url = knowledge::get_provider_base_url(conn, &provider)?;
        let key = secrets::get_key(&provider)?;
        Ok((provider, model, detail, packed, base_url, key))
    })
    .await?;
    let synthesis = agent::synthesize(
        &provider,
        &model,
        key.as_deref(),
        base_url.as_deref(),
        &packed,
    )
    .await?;
    let path = blocking(move || write_note(&detail, Some(&synthesis), &vault_dir)).await?;
    track(&app, telemetry::Event::ExportObsidian { synthesized: true });
    Ok(path)
}

/// Pack a thread and open it as context in a fresh CLI agent session (default: claude).
///
/// The context file carries ONE compact, budgeted memory block, and when it does, the
/// launched session runs with `CALLIMACHUS_SKIP_HOOK=1` so the SessionStart hook doesn't
/// inject a second copy.
#[tauri::command]
async fn open_thread_in_cli(
    pool: tauri::State<'_, db::ReadPool>,
    thread_id: i64,
    program: Option<String>,
) -> AppResult<String> {
    let program = program.unwrap_or_else(|| "claude".into());
    // Claude Code loads its own auto memory into the session, so for it the block leaves the
    // imported copy out, as `cal hook` does; another agent never sees that memory otherwise.
    let is_claude = std::path::Path::new(&program)
        .file_stem()
        .is_some_and(|s| s.eq_ignore_ascii_case("claude"));
    read_blocking(&pool, move |conn| {
        let packed = context::pack_thread(conn, thread_id, context::DEFAULT_BUDGET_CHARS)?
            .ok_or_else(|| anyhow::anyhow!("thread not found"))?;
        let project: Option<String> = conn.query_row(
            "SELECT project_path FROM threads WHERE id = ?1",
            [thread_id],
            |r| r.get::<_, Option<String>>(0),
        )?;
        // Prepend the project's distilled memory so the agent opens with what was already
        // decided + the gotchas to avoid, not just this one thread's transcript. Same "is
        // there anything" test as `cal hook` (pinned TODOs count too): the hook is skipped
        // only when this block was written, so a session never ends up with no memory.
        let memory = project
            .as_deref()
            .filter(|p| !p.is_empty())
            .and_then(|proj| {
                let key = knowledge::project_key(proj);
                let budget = export::MemoryBudget::COMPACT;
                let m = if is_claude {
                    knowledge::get_project_memory_facts_excluding(
                        conn,
                        &key,
                        budget.fetch_limit(),
                        knowledge::harvest::EXTRACTOR_CLAUDE_MEMORY,
                    )
                } else {
                    knowledge::get_project_memory_facts(conn, &key, budget.fetch_limit())
                }
                .ok()?;
                let pinned_todo = m.open_todos.iter().any(|t| t.pinned);
                if m.decisions.is_empty() && m.gotchas.is_empty() && !pinned_todo {
                    return None;
                }
                Some(format!(
                    "<project_memory project=\"{proj}\">\n{}\n</project_memory>",
                    export::agent_memory_md_budgeted(&key, &m, None, &budget)
                ))
            });
        let has_memory = memory.is_some();
        let packed = match memory {
            Some(block) => format!("{block}\n\n{packed}"),
            None => packed,
        };
        agent::cli_resume::launch_with_context_memory(
            &program,
            &packed,
            project.as_deref(),
            has_memory,
        )
    })
    .await
}

/// Relaunch the original CLI agent on an indexed thread (Claude Code / Codex).
#[tauri::command]
async fn resume_thread(pool: tauri::State<'_, db::ReadPool>, thread_id: i64) -> AppResult<()> {
    read_blocking(&pool, move |conn| {
        let (source, external_id, is_subagent, project): (String, String, bool, Option<String>) =
            conn.query_row(
                "SELECT s.kind, t.external_id, t.is_subagent, t.project_path
                 FROM threads t JOIN sources s ON s.id = t.source_id WHERE t.id = ?1",
                [thread_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0, r.get(3)?)),
            )?;
        agent::cli_resume::launch(&source, &external_id, is_subagent, project.as_deref())
    })
    .await
}

/// Whether "Resume" / "Open in CLI" can open a terminal on this platform.
#[tauri::command]
fn cli_launch_supported() -> bool {
    agent::cli_resume::launch_supported()
}

// ---- Claude Code integration (one-click /recall skill + MCP server) ----
//
// Every command passes `current_exe()`: integration resolves the stable path itself
// (`integration::stable_app_exe`: $APPIMAGE on Linux, and a refusal for macOS App
// Translocation / disk-image launches), so a temporary mount never lands in an MCP entry,
// a hook or the `cal` link.

fn current_exe() -> anyhow::Result<std::path::PathBuf> {
    Ok(std::env::current_exe()?)
}

/// Whether the `/recall` skill + `callimachus` MCP server are installed for Claude Code.
#[tauri::command]
async fn recall_integration_status() -> AppResult<integration::IntegrationStatus> {
    blocking(|| Ok(integration::status(&current_exe()?))).await
}

/// Install (or refresh) the `/recall` skill and register this app as Claude Code's
/// `callimachus` MCP server, with no terminal, cargo, or extra binary needed. Async: install
/// probes the user's login-shell PATH once (up to a few seconds).
#[tauri::command]
async fn install_recall_integration(app: AppHandle) -> AppResult<integration::IntegrationStatus> {
    let status = blocking(|| integration::install(&current_exe()?)).await?;
    track(
        &app,
        telemetry::Event::IntegrationInstalled {
            agent: "claude_code".into(),
        },
    );
    Ok(status)
}

/// Remove the skill file and the MCP registration.
#[tauri::command]
async fn uninstall_recall_integration() -> AppResult<()> {
    blocking(integration::uninstall).await
}

/// Toggle the opt-in proactive-recall hook (injects prior work into Claude before each prompt).
/// Separate from the base integration because it reads every prompt. Returns refreshed status.
#[tauri::command]
async fn set_proactive_recall(enabled: bool) -> AppResult<integration::IntegrationStatus> {
    blocking(move || integration::set_proactive_recall(&current_exe()?, enabled)).await
}

/// MCP-registration status for the other detected agents (Codex / Cursor / Gemini).
#[tauri::command]
async fn agent_integrations_status() -> AppResult<Vec<integration::AgentIntegration>> {
    blocking(|| Ok(integration::agent_status(&current_exe()?))).await
}

/// Register the `callimachus` MCP server with every detected non-Claude agent.
#[tauri::command]
async fn install_agent_integrations(
    app: AppHandle,
) -> AppResult<Vec<integration::AgentIntegration>> {
    let result = blocking(|| integration::install_agents(&current_exe()?)).await?;
    for a in result.iter().filter(|a| a.registered) {
        track(
            &app,
            telemetry::Event::IntegrationInstalled {
                agent: a.id.clone(),
            },
        );
    }
    Ok(result)
}

/// Remove the `callimachus` MCP registration and the Codex and Gemini CLI memory hooks
/// from the other agents.
#[tauri::command]
async fn uninstall_agent_integrations() -> AppResult<()> {
    blocking(integration::uninstall_agents_and_hooks).await
}

/// Settings > Agents: the Codex and Gemini CLI memory hooks (installed, and why not).
#[tauri::command]
async fn agent_hooks_status() -> AppResult<Vec<integration::AgentHooksStatus>> {
    blocking(|| Ok(integration::agent_hooks_status(&current_exe()?))).await
}

/// Add the memory hooks to `agent` (`codex` or `gemini`).
#[tauri::command]
async fn install_agent_hooks(agent: String) -> AppResult<integration::AgentHooksStatus> {
    blocking(move || integration::install_agent_hooks(&current_exe()?, &agent)).await
}

/// Remove the memory hooks from `agent`.
#[tauri::command]
async fn uninstall_agent_hooks(agent: String) -> AppResult<integration::AgentHooksStatus> {
    blocking(move || integration::uninstall_agent_hooks(&current_exe()?, &agent)).await
}

/// The SessionStart hook's settings: whether it injects the recent-sessions digest for a
/// repo with no memory yet, and the repos muted with `cal hook --mute`.
#[tauri::command]
async fn hook_settings(
    pool: tauri::State<'_, db::ReadPool>,
) -> AppResult<export::hook_settings::HookSettings> {
    read_blocking(&pool, export::hook_settings::get).await
}

/// Turn the SessionStart hook's recent-sessions digest on or off for every repo.
#[tauri::command]
async fn set_hook_digest(app: AppHandle, on: bool) -> AppResult<()> {
    write_blocking(&app, move |conn| {
        export::hook_settings::set_digest_enabled(conn, on)
    })
    .await
}

/// The first-run setup checklist's saved step states (`onboarding.steps`).
#[tauri::command]
async fn onboarding_state(
    pool: tauri::State<'_, db::ReadPool>,
) -> AppResult<onboarding::OnboardingSteps> {
    read_blocking(&pool, onboarding::get).await
}

/// Mark a checklist step done or skipped, or clear it (`state: null`) to reopen it. Returns
/// every step. The read-modify-write runs under the one writer lock, so two quick clicks
/// can't drop each other's step.
#[tauri::command]
async fn set_onboarding_step(
    app: AppHandle,
    step: String,
    state: Option<String>,
) -> AppResult<onboarding::OnboardingSteps> {
    write_blocking(&app, move |conn| {
        onboarding::set_step(conn, &step, state.as_deref())
    })
    .await
}

// ---- value ledger and transcript retention (ledger.rs) ----

/// What `value_ledger` returns: the Stats card "What Callimachus did for your agents this
/// week" and the retention guard under it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ValueLedger {
    week: ledger::Counts,
    all_time: u64,
    preserved: PreservedSessions,
}

/// Indexed sessions whose transcript Claude Code has since deleted, with the retention that
/// decides how many more will go.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PreservedSessions {
    threads: i64,
    source: &'static str,
    /// The user-level `cleanupPeriodDays`, or null when unset (or settings.json unreadable).
    cleanup_period_days: Option<i64>,
    /// What Claude Code applies: the setting, else its documented 30-day default.
    effective_days: i64,
    /// The value before Callimachus last changed it (null: it was unset), when a restore is
    /// on offer.
    previous_cleanup_days: Option<i64>,
    can_restore: bool,
}

/// app_config: the `cleanupPeriodDays` value (`{"days": n | null}`) Callimachus replaced,
/// so the Stats card can put it back. Cleared once restored.
const RETENTION_PREVIOUS_KEY: &str = "retention.claude_previous";

/// The remembered value, if any (`Some(None)`: it was unset). A remembered value below 1 day
/// reads as unset: Claude Code rejects one (`0` is a validation error that pauses its sweep)
/// and so does [`integration::set_claude_cleanup_period_days`], so restoring it would fail
/// forever with the button still on offer.
fn retention_previous(conn: &rusqlite::Connection) -> anyhow::Result<Option<Option<i64>>> {
    Ok(knowledge::app_config::get(conn, RETENTION_PREVIOUS_KEY)?
        .and_then(|v| serde_json::from_str::<serde_json::Value>(&v).ok())
        .map(|v| valid_cleanup_days(v.get("days").and_then(serde_json::Value::as_i64))))
}

/// A `cleanupPeriodDays` Claude Code accepts, else None (unset). A non-integer value (a
/// string, a float) already reads as None.
fn valid_cleanup_days(days: Option<i64>) -> Option<i64> {
    days.filter(|d| *d >= 1)
}

#[tauri::command]
async fn value_ledger(pool: tauri::State<'_, db::ReadPool>) -> AppResult<ValueLedger> {
    read_blocking(&pool, |conn| {
        let now = chrono::Utc::now().timestamp();
        let summary = ledger::summary(conn, ledger::path_for_conn(conn).as_deref(), now)?;
        // ~21k stats on the author's index; this runs on the blocking pool.
        let threads = match indexer::claude::projects_root() {
            Some(root) => ledger::preserved_claude_threads(conn, &root)?,
            None => 0,
        };
        // An unreadable settings.json reads as unset here; changing it reports the problem.
        let days = integration::claude_cleanup_period_days().unwrap_or(None);
        let previous = retention_previous(conn)?;
        Ok(ValueLedger {
            week: summary.week,
            all_time: summary.all_time,
            preserved: PreservedSessions {
                threads,
                source: "claude_code",
                cleanup_period_days: days,
                effective_days: days.unwrap_or(integration::CLAUDE_DEFAULT_CLEANUP_DAYS),
                previous_cleanup_days: previous.flatten(),
                can_restore: previous.is_some(),
            },
        })
    })
    .await
}

/// Set Claude Code's `cleanupPeriodDays` (null removes it, back to the 30-day default).
/// Remembers the value it replaced so the Stats card can offer it back; setting that
/// remembered value again is the restore, which forgets it.
#[tauri::command]
async fn set_claude_cleanup_days(app: AppHandle, days: Option<i64>) -> AppResult<()> {
    write_blocking(&app, move |conn| {
        let current = integration::claude_cleanup_period_days()?;
        if current == days {
            return Ok(());
        }
        let previous = retention_previous(conn)?;
        integration::set_claude_cleanup_period_days(days)?;
        if previous == Some(days) {
            knowledge::app_config::delete(conn, RETENTION_PREVIOUS_KEY)?;
        } else if previous.is_none() {
            // An invalid value is remembered as unset, so the restore can always run.
            knowledge::app_config::put(
                conn,
                RETENTION_PREVIOUS_KEY,
                &serde_json::json!({ "days": valid_cleanup_days(current) }).to_string(),
            )?;
        }
        Ok(())
    })
    .await
}

// ---- telemetry (opt-in analytics; see telemetry.rs) ----

/// Telemetry consent + whether capture is live (for the consent card and Settings).
#[tauri::command]
async fn telemetry_state(
    app: AppHandle,
    pool: tauri::State<'_, db::ReadPool>,
) -> AppResult<telemetry::TelemetryStatus> {
    read_blocking(&pool, move |conn| {
        Ok(telemetry::telemetry_state(
            &app.state::<telemetry::TelemetryState>(),
            conn,
        ))
    })
    .await
}

/// Persist the user's choice and start/stop capture immediately.
#[tauri::command]
async fn set_telemetry(app: AppHandle, granted: bool) -> AppResult<telemetry::TelemetryStatus> {
    let state_app = app.clone();
    write_blocking(&app, move |conn| {
        telemetry::set_telemetry(
            &state_app.state::<telemetry::TelemetryState>(),
            conn,
            granted,
        )
    })
    .await
}

/// Fire-and-forget event from the webview; validated against the closed schema in Rust.
#[tauri::command]
fn track_event(
    state: tauri::State<'_, telemetry::TelemetryState>,
    name: String,
    props: Option<serde_json::Map<String, serde_json::Value>>,
) {
    telemetry::track_event(&state, &name, &props.unwrap_or_default());
}

/// Send buffered events now (UpdateBanner awaits this before relaunch()).
#[tauri::command]
async fn telemetry_flush(state: tauri::State<'_, telemetry::TelemetryState>) -> AppResult<()> {
    state.flush(telemetry::EXIT_FLUSH_TIMEOUT).await;
    Ok(())
}

/// Stable id for the tray icon, so it can be added on enable and removed on disable.
const TRAY_ID: &str = "close-to-tray";

/// Bumped on every tray (re)build so each menu gets unique item ids (see `show_tray`).
static TRAY_GEN: AtomicUsize = AtomicUsize::new(0);

/// Show + focus the main window (used to restore it from the tray).
fn reveal_main_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

/// A Dock click with no window showing. Main is also hidden on purpose during startup
/// (until both halves are ready, with the splash itself held back by [`SPLASH_DELAY`]), and
/// revealing it there showed an unloaded main window next to the splash. So before the
/// reveal the click brings up the splash, and after it main comes back from the tray.
#[cfg(target_os = "macos")]
fn reopen_from_dock(app: &AppHandle) {
    let revealed = app
        .try_state::<SetupState>()
        .is_none_or(|s| s.0.lock().map_or(true, |f| f.revealed));
    if revealed {
        reveal_main_window(app);
    } else {
        show_splash(app);
    }
}

/// Put the tray icon up (idempotent): a Show / Quit menu, with a left-click restoring the
/// window. Present only while close-to-tray is enabled.
fn show_tray(app: &AppHandle) -> tauri::Result<()> {
    if app.tray_by_id(TRAY_ID).is_some() {
        return Ok(()); // already showing
    }
    // Fresh menu-item ids on every (re)build: a disable/re-enable cycle drops the old menu
    // and builds a new one, so a generation suffix guarantees no collision with a prior
    // menu's ids regardless of when muda clears them. Matched by stable prefix below.
    let generation = TRAY_GEN.fetch_add(1, Ordering::Relaxed);
    let show = MenuItem::with_id(
        app,
        format!("tray-show-{generation}"),
        "Show Callimachus",
        true,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(
        app,
        format!("tray-quit-{generation}"),
        "Quit",
        true,
        None::<&str>,
    )?;
    let menu = Menu::with_items(app, &[&show, &quit])?;
    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip("Callimachus")
        .menu(&menu)
        // Left-click restores the window; the menu opens on right-click.
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| {
            // Ids carry a generation suffix (see above); match on the stable prefix.
            let id = event.id.as_ref();
            if id.starts_with("tray-show") {
                reveal_main_window(app);
            } else if id.starts_with("tray-quit") {
                app.exit(0);
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                reveal_main_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon().cloned() {
        builder = builder.icon(icon);
    }
    builder.build(app)?;
    Ok(())
}

/// Remove the tray icon (no-op if it isn't showing).
fn hide_tray(app: &AppHandle) {
    app.remove_tray_by_id(TRAY_ID);
}

/// Whether the main window hides to the system tray on close (vs. quitting).
#[tauri::command]
async fn get_close_to_tray(pool: tauri::State<'_, db::ReadPool>) -> AppResult<bool> {
    read_blocking(&pool, knowledge::get_close_to_tray).await
}

/// Toggle close-to-tray: persist the pref, update the live flag the close handler reads,
/// and add/remove the tray icon (shown only while enabled). Off the main thread; the tray
/// calls dispatch to it internally.
#[tauri::command(async)]
fn set_close_to_tray(app: AppHandle, on: bool) -> AppResult<()> {
    // Create/remove the tray FIRST: enabling only takes effect if the tray icon actually
    // comes up, so we never persist a state that hides the window with no way to restore it.
    if on {
        show_tray(&app).map_err(anyhow::Error::from)?;
    } else {
        hide_tray(&app);
    }
    // Persist. If the DB write fails, roll the tray change back so the icon, the saved
    // pref, and the in-memory flag never diverge.
    let persisted: AppResult<()> = (|| {
        let db = app.state::<db::Db>();
        let conn = lock(&db)?;
        knowledge::set_close_to_tray(&conn, on)?;
        Ok(())
    })();
    if let Err(e) = persisted {
        if on {
            hide_tray(&app);
        } else {
            let _ = show_tray(&app);
        }
        return Err(e);
    }
    app.state::<CloseToTray>().0.store(on, Ordering::Relaxed);
    Ok(())
}

// ---- startup ----
//
// Tauri creates the config windows (the splash, hidden until SPLASH_DELAY) and then runs
// `setup` ON THE MAIN THREAD, before the event loop paints anything. Opening the index there
// (migrations, which on an upgrade can rebuild indexes over a multi-GB messages table, plus the
// backfills) froze the splash into a beachball / "Not Responding". So `setup` only manages
// cheap state and hands the rest to a background thread. The main window is created by that
// thread once the database state is managed (it is `"create": false` in tauri.conf.json), so no
// command can run before the state it needs exists. A failure to open the index shows an error
// screen in the splash window instead of a panic that made the app vanish.

/// Why the backend could not start, for the error screen.
#[derive(Debug, PartialEq)]
enum StartupFailure {
    /// The index was migrated by a newer build (`DatabaseTooFarAhead`).
    TooNew,
    /// SQLite reports the file is not a database or is corrupt.
    Corrupt,
    /// The index opened, but the main window couldn't be built.
    Window,
    Other,
}

fn is_corrupt_sqlite(e: &rusqlite::Error) -> bool {
    matches!(
        e.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase)
    )
}

fn classify_startup_error(e: &anyhow::Error) -> StartupFailure {
    for cause in e.chain() {
        if let Some(m) = cause.downcast_ref::<rusqlite_migration::Error>() {
            match m {
                rusqlite_migration::Error::MigrationDefinition(
                    rusqlite_migration::MigrationDefinitionError::DatabaseTooFarAhead,
                ) => return StartupFailure::TooNew,
                rusqlite_migration::Error::RusqliteError { err, .. } if is_corrupt_sqlite(err) => {
                    return StartupFailure::Corrupt
                }
                _ => {}
            }
        }
        if let Some(r) = cause.downcast_ref::<rusqlite::Error>() {
            if is_corrupt_sqlite(r) {
                return StartupFailure::Corrupt;
            }
        }
    }
    StartupFailure::Other
}

/// (title, paragraphs) for the startup error screen.
fn startup_error_text(kind: &StartupFailure, db_path: &std::path::Path) -> (String, Vec<String>) {
    let path = db_path.display();
    match kind {
        StartupFailure::TooNew => (
            "This index is from a newer Callimachus".into(),
            vec![
                format!(
                    "The index at {path} was upgraded by a newer version of Callimachus \
                     (or of its cal / MCP tools). This version can't open it safely."
                ),
                "Update Callimachus to the latest release, then open it again.".into(),
            ],
        ),
        StartupFailure::Corrupt => {
            // SQLite runs this index in WAL mode, so recent commits can live only in the
            // `-wal` file. Moving the three files together keeps the saved copy complete, and
            // keeps a leftover WAL from being replayed into the fresh index (which can corrupt
            // it). A folder keeps their names paired, so the copy still opens as one database.
            let name = db_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "index.db".into());
            let dir = db_path
                .parent()
                .map(|d| d.display().to_string())
                .unwrap_or_default();
            (
                "The index file is damaged".into(),
                vec![
                    format!("Callimachus couldn't read the index at {path}."),
                    format!(
                        "To start fresh: quit, then in {dir} move {name} together with \
                         {name}-wal and {name}-shm (if they exist) into a new folder, for \
                         example one named broken-index. Open Callimachus again and it \
                         rebuilds the index from your agents' history."
                    ),
                    "Move all three files: the -wal file can hold your most recent changes, \
                     and one left behind can damage the new index. Stars, tags, recorded \
                     decisions and snapshots live only in these files, so keep that folder."
                        .into(),
                ],
            )
        }
        StartupFailure::Window => (
            "Callimachus couldn't open its window".into(),
            vec![
                "Your index opened fine, but the main window couldn't be created.".into(),
                "Quit and open Callimachus again. If it keeps happening, repair the Microsoft \
                 Edge WebView2 Runtime on Windows (Settings, Apps), or update WebKitGTK on \
                 Linux."
                    .into(),
            ],
        ),
        StartupFailure::Other => (
            "Callimachus couldn't start".into(),
            vec![format!("Opening the index at {path} failed.")],
        ),
    }
}

/// Run `script` in a webview a few times over the next few seconds. A window created
/// moments ago may still be loading its page, and a script evaluated then is lost; every
/// script passed here is idempotent (the error screen checks a marker on `<body>`), so
/// repeating it is harmless.
fn eval_soon(app: &AppHandle, label: &'static str, script: String) {
    let app = app.clone();
    std::thread::spawn(move || {
        for delay_ms in [0u64, 300, 1_000, 2_500] {
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            match app.get_webview_window(label) {
                Some(w) => {
                    let _ = w.eval(&script);
                }
                None => return,
            }
        }
    });
}

/// Replace the splash's tagline ("Cataloguing your coding history…") with a status line.
fn set_splash_status(app: &AppHandle, text: &str) {
    let text = js_literal(text);
    eval_soon(
        app,
        "splashscreen",
        format!("(function(){{var p=document.querySelector('.wordmark p');if(p)p.textContent={text};}})();"),
    );
}

/// Turn the splash window into a readable, closable error screen. No dialog plugin is
/// bundled, so the message is rendered into the splash webview itself.
fn show_startup_error(app: &AppHandle, kind: StartupFailure, e: &anyhow::Error) {
    let db_path = db::default_index_path();
    let (title, paragraphs) = startup_error_text(&kind, &db_path);
    let mut details = format!("{e:#}");
    if let Some(log) = applog::path() {
        details.push_str(&format!("\n\nLog: {}", log.display()));
    }
    app_log!("[startup] {title}: {e:#}");

    let window = match app.get_webview_window("splashscreen") {
        Some(w) => Some(w),
        None => tauri::WebviewWindowBuilder::new(
            app,
            "splashscreen",
            tauri::WebviewUrl::App("splashscreen.html".into()),
        )
        .title("Callimachus")
        .build()
        .ok(),
    };
    let Some(window) = window else {
        return; // nothing to render into; the log has the error
    };
    // The splash is created hidden (SPLASH_DELAY), and a failure can come sooner.
    let _ = window.show();
    let _ = window.set_decorations(true);
    let _ = window.set_resizable(true);
    let _ = window.set_size(tauri::LogicalSize::new(600.0, 440.0));
    let _ = window.center();
    let _ = window.set_title("Callimachus couldn't start");
    let _ = window.set_focus();

    eval_soon(
        app,
        "splashscreen",
        startup_error_script(&title, &paragraphs, &details),
    );
}

/// The script that renders the error screen into the splash page. Built from DOM calls with
/// `textContent` (no HTML parsing), so nothing in an error message can inject markup.
fn startup_error_script(title: &str, paragraphs: &[String], details: &str) -> String {
    format!(
        r#"(function(){{
  var d=document,b=d.body;
  if(!b||b.getAttribute("data-startup-error"))return;
  b.setAttribute("data-startup-error","1");
  b.style.cssText="display:block;padding:28px 32px;overflow:auto;user-select:text;cursor:auto;text-align:left;font-size:14px;line-height:1.5";
  b.innerHTML="";
  var h=d.createElement("h1");h.style.cssText="font-size:18px;font-weight:600;margin:0 0 12px";h.textContent={title};b.appendChild(h);
  {paras}.forEach(function(t){{var p=d.createElement("p");p.style.cssText="margin:0 0 10px;color:#e5e5e5";p.textContent=t;b.appendChild(p);}});
  var pre=d.createElement("pre");pre.style.cssText="margin:14px 0 10px;padding:10px 12px;border:1px solid rgba(255,255,255,.12);border-radius:6px;white-space:pre-wrap;word-break:break-word;font-size:12px;color:#a1a1a1";pre.textContent={details};b.appendChild(pre);
  var q=d.createElement("p");q.style.cssText="margin:0;color:#a1a1a1;font-size:12px";q.textContent="Close this window to quit.";b.appendChild(q);
}})();"#,
        title = js_literal(title),
        paras = js_literal(paragraphs),
        details = js_literal(details),
    )
}

/// A value as a JavaScript literal (JSON is valid JS), for scripts evaluated in a webview.
fn js_literal<T: Serialize + ?Sized>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".into())
}

/// Open + migrate the index and manage every piece of state the commands need. Runs on the
/// startup thread; the caller shows the error screen if it fails.
fn init_backend(app: &AppHandle) -> anyhow::Result<()> {
    // Resolve the DB the SAME way as the indexer, read pool, watcher, and sidecars
    // (default_index_path: honors CALLIMACHUS_DB, else the app data dir) so every
    // component opens one consistent file. Pointing CALLIMACHUS_DB at a throwaway
    // path is how you exercise a clean first-run / onboarding without touching the
    // real index.
    let db_path = db::default_index_path();
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // An upgrade with pending migrations can take a while on a big index: say so.
    if db_path.exists() {
        let pending = rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .ok()
        .and_then(|c| db::migrations::MIGRATIONS.pending_migrations(&c).ok());
        if pending.is_some_and(|n| n > 0) {
            set_splash_status(
                app,
                "Upgrading your index (one time, this can take a minute)…",
            );
            show_splash(app); // no waiting for SPLASH_DELAY: this launch is a slow one
        }
    }
    let conn = db::open(&db_path)?; // single WRITER; also runs migrations

    // Only what the window needs runs here. The post-migration housekeeping (project-key
    // backfill / re-key, legacy purges) runs in spawn_background_jobs: cold, it cost about
    // 0.35 s of splash on every launch and about 1.1 s on the first launch of a re-key.

    // Settings overrides for relocated Claude / Codex data roots, for every code path that
    // isn't a scan (scans re-read them on their own).
    indexer::claude::load_config_dir_setting(&conn);
    indexer::codex::load_codex_home_setting(&conn);

    // Read the close-to-tray pref before `conn` is moved into the managed writer.
    let close_to_tray_on = knowledge::get_close_to_tray(&conn).unwrap_or(false);

    // Opt-in analytics. The token is compiled in only by the official release workflow
    // (build.yml, tauri-action step env). Source builds, forks and `tauri dev` get None, and
    // debug builds never send. Read here and nowhere else, so the `cal` and `--mcp` paths in
    // main.rs never reach it. An unset CI variable expands to "", which counts as unset.
    app.manage(telemetry::TelemetryState::new(
        option_env!("CALLIMACHUS_POSTHOG_HOST").filter(|h| !h.trim().is_empty()),
        option_env!("CALLIMACHUS_POSTHOG_TOKEN").filter(|t| !t.trim().is_empty()),
        &conn,
    ));

    app.manage(db::Db(Mutex::new(conn)));
    // Read pool (after the writer migrated): UI read commands run concurrently
    // here instead of serializing behind the writer mutex.
    let pool_size = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(4);
    app.manage(db::read_pool(&db_path, pool_size.clamp(2, 6))?);
    app.manage(search::SearchGen::default());

    // Close-to-tray: bring the tray up if the pref is on. The cached flag reflects
    // whether it ACTUALLY came up, so a failed tray never leaves the window
    // hideable-but-unrecoverable. The saved pref is left intact so it retries next
    // launch.
    let tray_up = close_to_tray_on
        && match show_tray(app) {
            Ok(()) => true,
            Err(e) => {
                app_log!("[tray] startup: {e}");
                false
            }
        };
    app.state::<CloseToTray>()
        .0
        .store(tray_up, Ordering::Relaxed);
    Ok(())
}

/// Create the main window from its tauri.conf.json entry (`"create": false`, hidden until
/// `complete_setup` shows it).
fn create_main_window(app: &AppHandle) -> anyhow::Result<()> {
    if app.get_webview_window("main").is_some() {
        return Ok(()); // the config still auto-creates it
    }
    let config = app
        .config()
        .app
        .windows
        .iter()
        .find(|w| w.label == "main")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no \"main\" window in tauri.conf.json"))?;
    tauri::WebviewWindowBuilder::from_config(app, &config)?
        // WebKitGTK (Linux) and WebView2 refuse script clipboard writes unless asked, which
        // broke "Copy context" there; macOS always allows them.
        .enable_clipboard_access()
        .build()?;
    Ok(())
}

/// The startup thread: init, then the main window, then the background jobs that need the
/// database. Any failure before the window exists ends on the error screen.
fn start_backend(app: AppHandle) {
    let started = std::time::Instant::now();
    {
        let app = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(SPLASH_DELAY);
            show_splash(&app);
        });
    }
    if let Err(e) = init_backend(&app) {
        show_startup_error(&app, classify_startup_error(&e), &e);
        return;
    }
    // Reported on its own: the index is healthy here, so the database screen would send the
    // user after the wrong problem.
    if let Err(e) = create_main_window(&app) {
        // init_backend may have brought up the tray; its "Show" item has no window to reveal.
        hide_tray(&app);
        app.state::<CloseToTray>().0.store(false, Ordering::Relaxed);
        show_startup_error(&app, StartupFailure::Window, &e);
        return;
    }
    app_log!(
        "[startup] backend ready in {} ms (v{})",
        started.elapsed().as_millis(),
        env!("CARGO_PKG_VERSION")
    );
    complete_setup(&app, "backend");
    // Backstop: reveal main even if the frontend never reports ready (its bundle failed to
    // load, or a render threw before AppShell mounted), so the user isn't left on a splash.
    {
        let app = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(REVEAL_BACKSTOP);
            complete_setup(&app, "frontend");
        });
    }
    spawn_background_jobs(&app);
}

/// Jobs that need the managed database and must not delay the window.
fn spawn_background_jobs(app: &AppHandle) {
    // Background watcher keeps the index fresh as agents write new threads.
    indexer::watcher::spawn(app.clone());

    // Bring already-installed integrations up to date (hook shape, timeouts, missing-binary
    // guard, retired SubagentStop, dangling MCP / `cal` paths). Only touches pieces the user
    // installed, with one exception: it first puts back whatever the Windows uninstaller's
    // `--uninstall-integration` removed during a manual reinstall (the marker it leaves in
    // the data dir). A failed restore piece is logged here and not retried; Settings still
    // offers Install for it.
    std::thread::spawn(|| {
        match current_exe().and_then(|exe| integration::refresh_installed(&exe)) {
            Ok(true) => app_log!("[integration] restored / refreshed installed integrations"),
            Ok(false) => {}
            Err(e) => app_log!("[integration] refresh: {e:#}"),
        }
    });

    // One-time removal of distill / Ask sessions the app's own CLI calls left in the index
    // (guarded by an app_config flag, so later launches return at once). Own connection.
    std::thread::spawn(|| {
        if let Ok(mut c) = db::open(&db::default_index_path()) {
            match cleanup::purge_app_cli_sessions_once(&mut c) {
                Ok(n) if n > 0 => app_log!("[cleanup] removed {n} app-generated CLI sessions"),
                Err(e) => app_log!("[cleanup] purge: {e}"),
                _ => {}
            }
        }
    });

    spawn_tool_output_compaction(app);

    let app = app.clone();
    std::thread::spawn(move || {
        // Housekeeping first, so the distill catch-up below sees re-keyed projects. Own
        // connection (the watcher's first write waits out a short overlap via BUSY_TIMEOUT).
        match db::open(&db::default_index_path()) {
            Ok(conn) => {
                let clock = jobclock::JobClock::start();
                let changed = startup_housekeeping(&conn);
                app_log!(
                    "[startup] housekeeping: {changed} rows changed in {}",
                    clock.thread_summary()
                );
                if changed > 0 {
                    // Open views may be grouping by the old project keys. The null payload
                    // (no source kind: no scan wrote these rows) is the contract main.tsx
                    // reads as "housekeeping": it refetches what groups by project at once,
                    // Project Memory included, instead of only marking it stale the way a
                    // watcher batch does.
                    let _ = app.emit("index:updated", ());
                }
            }
            Err(e) => app_log!("[startup] housekeeping: {e:#}"),
        }

        let db = app.state::<db::Db>();
        let embedder = app.state::<embed::Embedder>();
        let (has_vectors, facts_pending) =
            db.0.lock()
                .map(|c| startup_embed_state(&c))
                .unwrap_or((false, false));
        // Load the model at launch only when this session needs it right away. Hybrid search,
        // recall and Ask load it from the cache on first use (about 150 ms), so a prefetch
        // for everyone with a semantic index pinned about 400 MB and 45 ORT threads for the
        // whole session of a close-to-tray app that may never run a semantic query.
        if should_prefetch_model(has_vectors, facts_pending, embed::model_is_cached()) {
            if let Err(e) = embedder.ensure_loaded() {
                app_log!("[embed] prefetch: {e}");
            }
        }
        if facts_pending {
            // Distilled facts not embedded yet (distilled before the recall index shipped,
            // or while offline): embed them so cross-thread recall finds them.
            embed_facts_if_model_ready(&app, "knowledge");
        }
        // Then catch up on any auto-distillation owed (no-op unless enabled). Usually the
        // watcher's catch-up still holds CatchUpJob here; it kicks again when it finishes.
        auto_distill_kick(&app);
    });
}

/// One-time TODO backfill for indexes from before TODOs were always on (guarded by
/// `knowledge.todos_backfilled`, so later launches return at once). The watcher calls this
/// once its launch catch-up is done, so the two never compete for the writer; the backfill
/// then pauses between batches while a reindex, catch-up, semantic build or rewrite runs, as
/// a distill drain does.
pub(crate) fn todo_backfill_kick(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        let db = app.state::<db::Db>();
        let clock = jobclock::JobClock::start();
        let pause = || writer_job_running(&app);
        match knowledge::backfill_todos_once(&db, chrono::Utc::now().timestamp(), &pause) {
            Ok(true) => {
                app_log!("[knowledge] todo backfill in {}", clock.thread_summary());
                let _ = app.emit("knowledge:todos-ready", ());
            }
            Ok(false) => {}
            Err(e) => app_log!("[knowledge] todo backfill: {e}"),
        }
    });
}

/// Rows per background tool-output compaction pass ([`spawn_tool_output_compaction`]).
const BACKGROUND_COMPACT_ROWS: usize = 2_000;
/// How long after launch the compaction starts. The watcher's catch-up claims the writer
/// within moments of launch and takes 0.1-2 s (measured), so by then it is done and the
/// window has its first data; a catch-up deferred by a reindex is waited out below anyway.
const BACKGROUND_COMPACT_DELAY: std::time::Duration = std::time::Duration::from_secs(20);
/// Pause between passes, so the watcher and distill get the writer in between.
const BACKGROUND_COMPACT_GAP: std::time::Duration = std::time::Duration::from_secs(1);

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
fn spawn_tool_output_compaction(app: &AppHandle) {
    let app = app.clone();
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
        let other_writer = |app: &AppHandle| {
            app.state::<IndexJob>().0.is_set()
                || app.state::<CatchUpJob>().0.is_set()
                || app.state::<EmbedJob>().is_running()
        };
        let mut after = 0i64;
        for pass in 1.. {
            // Claim first, then check, as claim_rewrite does: a reindex or build that
            // started in between holds its own flag, and the claim is given back for it.
            let job = loop {
                if let Some(job) = JobGuard::try_acquire(&app.state::<CompactJob>().0) {
                    if !other_writer(&app) {
                        break job;
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(2));
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
                let _ = app.emit("index:updated", ());
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

/// One idempotent post-migration cleanup step; returns the rows it changed.
type HousekeepingStep = fn(&rusqlite::Connection) -> anyhow::Result<usize>;

/// Post-migration housekeeping, run once per launch off the splash path. Each step is
/// idempotent; a failure is logged and skipped. Returns how many rows changed in total.
fn startup_housekeeping(conn: &rusqlite::Connection) -> usize {
    let steps: [(&str, HousekeepingStep); 5] = [
        (
            "[index] project-key backfill",
            indexer::backfill_project_keys,
        ),
        // Threads whose project key went stale (home-dir keys, worktree keys).
        ("[knowledge] re-key projects", knowledge::rekey_project_keys),
        // Heuristic TODOs extracted from subagent threads by older builds.
        (
            "[knowledge] purge subagent todos",
            knowledge::purge_subagent_todos,
        ),
        // Legacy per-SubagentStop auto snapshots.
        (
            "[snapshot] purge legacy auto snapshots",
            snapshot::purge_legacy_auto_snapshots,
        ),
        // Owner rows for vectors an older build wrote without one (an old app that kept
        // embedding after a newer sidecar migrated the index). Until they exist, scoped
        // semantic search post-filters. Two b-tree counts when nothing is missing.
        (
            "[embed] reconcile vector owners",
            cleanup::reconcile_vec_owners,
        ),
    ];
    steps
        .into_iter()
        .map(|(what, step)| {
            step(conn).unwrap_or_else(|e| {
                app_log!("{what}: {e}");
                0
            })
        })
        .sum()
}

/// Recall facts still waiting for vectors. An index search, never a scan of `facts`: the
/// planner takes idx_facts_kind (or the partial idx_facts_pending_embed) and `EXISTS` stops
/// at the first hit, so a launch with nothing pending reads a few thousand index entries.
const FACTS_PENDING_EMBED_SQL: &str = "SELECT EXISTS(SELECT 1 FROM facts \
     WHERE embedded = 0 AND kind IN ('decision', 'gotcha'))";

/// (the semantic index has vectors, recall facts are waiting to be embedded).
fn startup_embed_state(conn: &rusqlite::Connection) -> (bool, bool) {
    let exists = |sql: &str| {
        conn.query_row(sql, [], |r| r.get::<_, bool>(0))
            .unwrap_or(false)
    };
    (
        exists("SELECT EXISTS(SELECT 1 FROM vec_chunk_owner)"),
        exists(FACTS_PENDING_EMBED_SQL),
    )
}

/// Whether launch should load the embedding model now, off the UI thread:
/// - an existing semantic index whose old CWD-relative model cache is gone: the one-time
///   re-download into the shared models dir (users who never built the semantic index
///   download nothing until they click Build);
/// - recall facts waiting for vectors, when the model is already on disk (a background fact
///   embed never starts the download; see `embed_facts_if_model_ready`).
fn should_prefetch_model(has_vectors: bool, facts_pending: bool, model_cached: bool) -> bool {
    (has_vectors && !model_cached) || (facts_pending && model_cached)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Persistent log + panic hook first (GUI only), so even a failed startup leaves a
    // record next to the index.
    if let Some(dir) = db::default_index_path().parent() {
        let _ = std::fs::create_dir_all(dir);
        applog::init(dir.join("callimachus.log"));
    }
    applog::install_panic_hook();

    let mut builder = tauri::Builder::default().plugin(tauri_plugin_opener::init());

    // Auto-update is desktop-only; the updater + process plugins back the
    // in-app "check for updates / restart to install" flow.
    #[cfg(desktop)]
    {
        builder = builder
            .plugin(tauri_plugin_updater::Builder::new().build())
            .plugin(tauri_plugin_process::init());
    }

    let app = builder
        .setup(|app| {
            // Cheap state that commands, window events and the tray read. The database
            // state (Db, ReadPool, TelemetryState) is managed by the startup thread.
            app.manage(embed::Embedder::default());
            app.manage(EmbedJob::default());
            app.manage(IndexJob::default());
            app.manage(CatchUpJob::default());
            app.manage(DistillJob::default());
            app.manage(CompactJob::default());
            app.manage(SetupState::default());
            app.manage(ChatGeneration::default());
            app.manage(PendingApprovals::default());
            app.manage(CloseToTray::default());
            app.manage(EditOrder::default());
            let handle = app.handle().clone();
            std::thread::Builder::new()
                .name("startup".into())
                .spawn(move || start_backend(handle))?;
            Ok(())
        })
        .on_window_event(|window, event| {
            // Close-to-tray: intercept the MAIN window's close and hide it instead of
            // quitting — but ONLY when a tray icon actually exists to restore it, so an
            // absent/failed tray can never trap the user with a hidden window. Other
            // windows (splash) close normally.
            if let WindowEvent::CloseRequested { api, .. } = event {
                let app = window.app_handle();
                if window.label() == "main"
                    && app.state::<CloseToTray>().0.load(Ordering::Relaxed)
                    && app.tray_by_id(TRAY_ID).is_some()
                {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            db_stats,
            index_stats,
            cleanup_candidates,
            delete_threads,
            vacuum_db,
            compact_index,
            jobs_status,
            get_data_roots,
            set_data_roots,
            index_all,
            indexing_status,
            index_source,
            search_threads,
            warm_semantic,
            recent_threads,
            get_thread,
            message_text,
            embedding_status,
            build_embeddings,
            send_chat,
            cancel_chat,
            approve_tool,
            set_api_key,
            delete_api_key,
            provider_has_key,
            list_models,
            resume_thread,
            cli_launch_supported,
            thread_context,
            open_thread_in_cli,
            export_thread,
            obsidian_vaults,
            can_synthesize,
            synthesize_export,
            recall_integration_status,
            install_recall_integration,
            uninstall_recall_integration,
            set_proactive_recall,
            cli_engines,
            memory_setup,
            enable_memory,
            agent_integrations_status,
            install_agent_integrations,
            uninstall_agent_integrations,
            agent_hooks_status,
            install_agent_hooks,
            uninstall_agent_hooks,
            hook_settings,
            set_hook_digest,
            onboarding_state,
            set_onboarding_step,
            value_ledger,
            set_claude_cleanup_days,
            set_star,
            set_thread_tags,
            list_tags,
            list_open_todos,
            knowledge_config,
            provider_base_url,
            set_provider_base_url,
            set_complete,
            coach_overview,
            set_knowledge_config,
            set_auto_distill,
            auto_distill_budget,
            set_auto_distill_daily_cap,
            harvest_settings,
            set_harvest_claude_memory,
            thread_knowledge,
            thread_commits,
            link_thread_commits,
            recurring_issues,
            spend,
            distill_thread,
            recall_decisions,
            recall_gotchas,
            find_prior_work,
            ask_history,
            search_by_file,
            list_projects,
            project_memory,
            project_brief,
            write_project_memory_file,
            distilling_status,
            cancel_distill,
            distill_project,
            write_agent_memory_file,
            set_fact_pinned,
            hide_fact,
            set_todo_done,
            remember,
            edit_fact,
            detect_conflicts,
            get_close_to_tray,
            set_close_to_tray,
            telemetry_state,
            set_telemetry,
            track_event,
            telemetry_flush
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|app, event| match event {
        // Last chance to send buffered analytics on a normal quit (window close, tray
        // Quit / app.exit). The updater's restart path skips this, which is why
        // UpdateBanner awaits `telemetry_flush` before relaunch().
        tauri::RunEvent::Exit => {
            if let Some(t) = app.try_state::<telemetry::TelemetryState>() {
                t.flush_blocking(telemetry::EXIT_FLUSH_TIMEOUT);
            }
        }
        // macOS: clicking the Dock icon while the window is hidden to the tray brings it
        // back (the app is still running, so there is no second launch to do it).
        #[cfg(target_os = "macos")]
        tauri::RunEvent::Reopen {
            has_visible_windows: false,
            ..
        } => reopen_from_dock(app),
        _ => {}
    });
}

#[cfg(test)]
mod startup_tests {
    use super::*;

    fn temp_db(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("calli_startup_{tag}_{}.db", std::process::id()));
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        p
    }

    /// A splash that appeared stays up for SPLASH_MIN_VISIBLE in all; one that never
    /// appeared holds nothing back.
    #[test]
    fn a_shown_splash_is_held_for_its_minimum_time() {
        use std::time::{Duration, Instant};
        let now = Instant::now();
        assert_eq!(splash_hold(None, now), Duration::ZERO);
        assert_eq!(splash_hold(Some(now), now), SPLASH_MIN_VISIBLE);
        let shown = now - Duration::from_millis(50); // a reveal just after SPLASH_DELAY
        assert_eq!(splash_hold(Some(shown), now), Duration::from_millis(550));
        let shown = now - Duration::from_secs(2); // a slow launch: no extra wait
        assert_eq!(splash_hold(Some(shown), now), Duration::ZERO);
    }

    /// A remembered `cleanupPeriodDays` Claude Code would reject reads as unset, so the
    /// Stats card's restore removes the key instead of failing on every click.
    #[test]
    fn an_invalid_remembered_retention_restores_as_unset() {
        let p = temp_db("retention");
        let conn = db::open(&p).unwrap();
        assert_eq!(retention_previous(&conn).unwrap(), None);
        for (stored, read) in [
            (r#"{"days":0}"#, None),
            (r#"{"days":-3}"#, None),
            (r#"{"days":"365"}"#, None),
            (r#"{"days":null}"#, None),
            (r#"{"days":90}"#, Some(90)),
        ] {
            knowledge::app_config::put(&conn, RETENTION_PREVIOUS_KEY, stored).unwrap();
            assert_eq!(retention_previous(&conn).unwrap(), Some(read), "{stored}");
        }
        assert_eq!(valid_cleanup_days(Some(0)), None);
        assert_eq!(valid_cleanup_days(Some(1)), Some(1));
    }

    #[test]
    fn a_newer_schema_is_reported_as_too_new() {
        let p = temp_db("ahead");
        drop(db::open(&p).unwrap());
        {
            let c = rusqlite::Connection::open(&p).unwrap();
            c.pragma_update(None, "user_version", 9_999).unwrap();
        }
        let err = db::open(&p).unwrap_err();
        assert_eq!(
            classify_startup_error(&err),
            StartupFailure::TooNew,
            "{err:#}"
        );
    }

    #[test]
    fn a_garbage_file_is_reported_as_corrupt() {
        let p = temp_db("garbage");
        std::fs::write(&p, vec![0x5a_u8; 8192]).unwrap();
        let err = db::open(&p).unwrap_err();
        assert_eq!(
            classify_startup_error(&err),
            StartupFailure::Corrupt,
            "{err:#}"
        );
    }

    #[test]
    fn error_script_encodes_every_string_as_a_literal() {
        let script = startup_error_script(
            "Title \"quoted\" </script>",
            &["one".to_string(), "two\nlines".to_string()],
            "C:\\path\\index.db",
        );
        assert!(script.contains(r#"h.textContent="Title \"quoted\" </script>""#));
        assert!(script.contains(r#"["one","two\nlines"].forEach"#));
        assert!(script.contains(r#"pre.textContent="C:\\path\\index.db""#));
        if let Ok(out) = std::env::var("CALLIMACHUS_DUMP_STARTUP_SCRIPT") {
            std::fs::write(out, &script).unwrap();
        }
    }

    #[test]
    fn other_failures_fall_back_to_the_generic_screen() {
        let err = anyhow::anyhow!("disk on fire");
        assert_eq!(classify_startup_error(&err), StartupFailure::Other);
        let (title, paras) = startup_error_text(
            &StartupFailure::Corrupt,
            std::path::Path::new("/x/index.db"),
        );
        assert!(title.contains("damaged"));
        assert!(paras.iter().any(|p| p.contains("/x/index.db")));
        // The WAL and shared-memory files must move with the database (see the text).
        assert!(paras
            .iter()
            .any(|p| p.contains("index.db-wal") && p.contains("index.db-shm")));
    }

    #[test]
    fn a_window_failure_does_not_blame_the_index() {
        let (title, paras) =
            startup_error_text(&StartupFailure::Window, std::path::Path::new("/x/index.db"));
        assert!(title.contains("window"), "{title}");
        assert!(
            !paras.iter().any(|p| p.contains("/x/index.db")),
            "{paras:?}"
        );
    }

    fn thread(ext: &str, project: Option<&str>, msgs: usize) -> indexer::ParsedThread {
        indexer::ParsedThread {
            external_id: ext.into(),
            title: Some(ext.into()),
            project_path: project.map(Into::into),
            updated_at: Some(1_000),
            created_at: Some(1_000),
            messages: (0..msgs)
                .map(|i| indexer::ParsedMessage {
                    role: if i % 2 == 0 { "user" } else { "assistant" }.into(),
                    text: format!("message {i}"),
                    tool_name: None,
                    ts: Some(1_000),
                })
                .collect(),
            ..Default::default()
        }
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

    #[test]
    fn launch_loads_the_model_only_when_the_session_needs_it() {
        // (has_vectors, facts_pending, model_cached) -> prefetch
        let cases = [
            ((true, false, true), false), // semantic index, model on disk: load on first use
            ((true, false, false), true), // semantic index, cache gone: one-time re-download
            ((false, true, true), true),  // recall facts waiting, model on disk
            ((false, true, false), false), // never downloads just for facts
            ((true, true, true), true),
            ((false, false, true), false),
            ((false, false, false), false),
        ];
        for ((vectors, pending, cached), want) in cases {
            assert_eq!(
                should_prefetch_model(vectors, pending, cached),
                want,
                "has_vectors={vectors} facts_pending={pending} cached={cached}"
            );
        }
    }

    #[test]
    fn pending_facts_check_searches_an_index() {
        let p = temp_db("facts_plan");
        let conn = db::open(&p).unwrap();
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {FACTS_PENDING_EMBED_SQL}"))
            .unwrap()
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            plan.iter().any(|d| d.starts_with("SEARCH facts USING")),
            "{plan:?}"
        );
        assert!(
            !plan.iter().any(|d| d.starts_with("SCAN facts")),
            "{plan:?}"
        );
        assert_eq!(startup_embed_state(&conn), (false, false));
    }

    /// The watcher's auto-embed probes this after every quiet minute of agent activity, so
    /// it must stay an index probe on a 1.4M-message table.
    #[test]
    fn pending_messages_check_searches_an_index() {
        let p = temp_db("msgs_pending_plan");
        let conn = db::open(&p).unwrap();
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
        let pending =
            |c: &rusqlite::Connection| c.query_row(&sql, [], |r| r.get::<_, bool>(0)).unwrap();
        assert!(!pending(&conn));
        let mut conn = conn;
        let sid = indexer::source_id(&conn, "codex").unwrap();
        indexer::upsert_thread(&mut conn, sid, &thread("a", None, 2)).unwrap();
        assert!(pending(&conn));
    }

    #[test]
    fn db_counts_match_the_live_row_counts() {
        let p = temp_db("db_counts");
        let mut conn = db::open(&p).unwrap();
        let sid = indexer::source_id(&conn, "codex").unwrap();
        indexer::upsert_thread(&mut conn, sid, &thread("a", None, 3)).unwrap();
        indexer::upsert_thread(&mut conn, sid, &thread("b", None, 5)).unwrap();
        // A re-index that rewrites a thread with more messages keeps the sum exact.
        indexer::upsert_thread(&mut conn, sid, &thread("a", None, 4)).unwrap();
        let live = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
        let stats = db_counts(&conn).unwrap();
        assert_eq!(stats.threads, 2);
        assert_eq!(stats.messages, live("SELECT COUNT(*) FROM messages"));
        assert_eq!(stats.messages, 9);
        assert_eq!(stats.sources, live("SELECT COUNT(*) FROM sources"));
    }

    #[test]
    fn housekeeping_reports_changes_once_then_nothing() {
        let p = temp_db("housekeeping");
        let mut conn = db::open(&p).unwrap();
        let project = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let sid = indexer::source_id(&conn, "codex").unwrap();
        indexer::upsert_thread(&mut conn, sid, &thread("t", project.to_str(), 2)).unwrap();
        // A thread from before project keys existed (what the backfill is for).
        conn.execute("UPDATE threads SET project_key = NULL", [])
            .unwrap();
        assert!(startup_housekeeping(&conn) > 0);
        let key: Option<String> = conn
            .query_row("SELECT project_key FROM threads", [], |r| r.get(0))
            .unwrap();
        assert!(key.is_some());
        // Steady state: nothing to change, so launch sends no `index:updated`.
        assert_eq!(startup_housekeeping(&conn), 0);
    }
}

/// Text built from index rows for a model provider is redacted on the way out, while the
/// index itself keeps the raw text.
#[cfg(test)]
mod redaction_tests {
    use super::*;

    const KEY: &str = "sk-ant-api03-AbCdEfGhIjKlMnOpQrStUv0123456789";

    fn assert_scrubbed(out: &str) {
        assert!(out.contains("[REDACTED]"), "{out}");
        assert!(!out.contains("sk-ant-api03"), "{out}");
    }

    fn memory_db() -> rusqlite::Connection {
        db::open(std::path::Path::new(":memory:")).unwrap()
    }

    #[test]
    fn chat_search_and_ask_context_redact_a_planted_key() {
        let mut conn = memory_db();
        let sid = indexer::source_id(&conn, "codex").unwrap();
        let t = indexer::ParsedThread {
            external_id: "leaky".into(),
            title: Some(format!("wire the gateway with {KEY}")),
            project_path: Some("/proj/app".into()),
            messages: vec![indexer::ParsedMessage {
                role: "user".into(),
                text: format!("the gateway rejects {KEY}"),
                tool_name: None,
                ts: Some(1_000),
            }],
            ..Default::default()
        };
        indexer::upsert_thread(&mut conn, sid, &t).unwrap();

        // The chat agent's search_history tool: title and snippet.
        let json = chat_search_json(&conn, "gateway", 10).unwrap();
        assert_scrubbed(&json);
        assert!(json.contains("wire the gateway with [REDACTED]"), "{json}");
        assert!(
            !json.contains("\\u0001"),
            "match sentinels are stripped: {json}"
        );

        // Ask: the title heading each excerpt. A keyless engine, so no keychain read.
        knowledge::set_config(&conn, true, Some("ollama"), Some("llama3")).unwrap();
        let prep = prepare_ask(&conn, "gateway", None)
            .unwrap()
            .expect("the seeded thread is relevant");
        assert_scrubbed(&prep.context);
        assert!(
            prep.context
                .contains("] wire the gateway with [REDACTED]\n"),
            "{}",
            prep.context
        );

        // Redaction happens on read: the index still holds the raw title.
        let stored: String = conn
            .query_row("SELECT title FROM threads", [], |r| r.get(0))
            .unwrap();
        assert!(stored.contains(KEY));
    }

    #[test]
    fn memory_notes_and_conflict_review_redact_a_planted_key() {
        let conn = memory_db();
        let project = "/proj/app";
        let leaky = format!("call the billing API with {KEY}");
        knowledge::record_fact(&conn, project, "decision", &leaky, None, 1_000).unwrap();
        knowledge::record_fact(&conn, project, "decision", "keep sqlite", None, 1_001).unwrap();

        // Project brief prompt (project_brief, write_project_memory_file).
        let mem = knowledge::get_project_memory_facts(&conn, project, 10).unwrap();
        let notes = format_memory_notes(&mem);
        assert_scrubbed(&notes);
        assert!(
            notes.contains("- call the billing API with [REDACTED]\n"),
            "{notes}"
        );

        // Conflict review prompt.
        let decisions = knowledge::project_decisions(&conn, project).unwrap();
        assert_eq!(decisions.len(), 2);
        let texts = conflict_review_texts(&decisions);
        assert_scrubbed(&texts.join("\n"));
        assert!(texts
            .iter()
            .any(|t| t == "call the billing API with [REDACTED]"));
        assert!(texts.iter().any(|t| t == "keep sqlite"));
    }
}

#[cfg(test)]
mod memory_setup_tests {
    use super::*;

    fn cli(id: &'static str, path: Option<&str>) -> agent::CliEngine {
        agent::CliEngine {
            id,
            label: id,
            bin: id,
            installed: path.is_some(),
            path: path.map(str::to_string),
        }
    }

    /// Both CLIs are always listed (with "not found" when missing), then Ollama, then only
    /// the keyed providers that have a key, in the legacy first-key order.
    #[test]
    fn engine_list_shape_and_details() {
        let models = vec!["llama3.1".to_string(), "qwen3".to_string()];
        let e = memory_engines(
            &[
                cli("claude-cli", Some("/usr/local/bin/claude")),
                cli("codex-cli", None),
            ],
            Some(&models),
            None,
            &["openrouter", "anthropic"],
        );
        let ids: Vec<&str> = e.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "claude-cli",
                "codex-cli",
                "ollama",
                "anthropic",
                "openrouter"
            ]
        );
        assert_eq!(e[0].detail, "/usr/local/bin/claude");
        assert!(e[0].detected && !e[1].detected);
        assert_eq!(e[1].detail, "not found");
        assert_eq!(e[2].kind, "ollama");
        assert_eq!(e[2].detail, "Running at http://localhost:11434, 2 models");
        assert!(e[3..]
            .iter()
            .all(|k| k.kind == "key" && k.detected && k.detail == "API key saved"));
        assert_eq!(
            knowledge::recommended_engine(&e).as_deref(),
            Some("claude-cli")
        );

        let e = memory_engines(
            &[cli("claude-cli", None), cli("codex-cli", None)],
            None,
            Some("http://gpu-box:11434 "),
            &[],
        );
        assert_eq!(e.len(), 3);
        assert_eq!(e[2].detail, "Not running at http://gpu-box:11434");
        assert_eq!(knowledge::recommended_engine(&e), None);
    }

    /// enable_memory accepts the CLIs and local Ollama even before they are detected (the
    /// card lists them), and refuses names it never lists. No keychain read for these.
    #[test]
    fn enable_memory_accepts_only_listed_engines() {
        assert!(memory_engine_listed("claude-cli"));
        assert!(memory_engine_listed("codex-cli"));
        assert!(memory_engine_listed("ollama"));
        assert!(!memory_engine_listed("auto"));
        assert!(!memory_engine_listed(""));
        assert!(!memory_engine_listed("bogus"));
    }

    /// A repeat of the current choice changes nothing (a stale card must not turn
    /// auto-distill back on); a different engine or model, or a legacy enabled config with
    /// no provider, is applied as the user's explicit pick.
    #[test]
    fn enable_memory_repeat_is_a_noop() {
        let cfg = |enabled: bool, provider: Option<&str>, model: Option<&str>| {
            knowledge::KnowledgeConfig {
                enabled,
                provider: provider.map(str::to_string),
                model: model.map(str::to_string),
                auto_distill: false,
            }
        };
        assert!(enable_memory_is_noop(
            &cfg(true, Some("claude-cli"), None),
            "claude-cli",
            None
        ));
        assert!(enable_memory_is_noop(
            &cfg(true, Some("ollama"), Some("qwen3")),
            "ollama",
            Some("qwen3")
        ));
        assert!(!enable_memory_is_noop(
            &cfg(true, Some("ollama"), Some("qwen3")),
            "ollama",
            Some("llama3.1")
        ));
        assert!(!enable_memory_is_noop(
            &cfg(true, None, None),
            "claude-cli",
            None
        ));
        assert!(!enable_memory_is_noop(
            &cfg(false, Some("claude-cli"), None),
            "claude-cli",
            None
        ));
    }
}

/// The Tauri command surface and the typed frontend API must list the same commands.
#[cfg(test)]
mod command_surface_tests {
    use std::collections::BTreeSet;

    /// The names in `run()`'s handler list. The needle is assembled so this test's own
    /// source never matches it.
    fn registered_commands() -> BTreeSet<&'static str> {
        let src = include_str!("lib.rs");
        let needle = concat!("tauri::generate_", "handler![");
        assert_eq!(src.matches(needle).count(), 1, "one handler list in lib.rs");
        let start = src.find(needle).unwrap() + needle.len();
        let end = start + src[start..].find(']').expect("the handler list closes");
        src[start..end]
            .lines()
            .map(|line| line.split("//").next().unwrap_or_default())
            .flat_map(|line| line.split(','))
            .map(str::trim)
            .filter(|name| !name.is_empty())
            // `module::command` registers under its last segment, the name `invoke` uses.
            .map(|path| path.rsplit("::").next().unwrap_or(path))
            .collect()
    }

    /// Command names passed as a string literal to `invoke(...)` or `invoke<T>(...)`.
    fn invoked_commands(src: &str) -> Vec<&str> {
        let mut names = Vec::new();
        for (at, word) in src.match_indices("invoke") {
            let prev = src[..at].chars().next_back();
            if prev.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$') {
                continue;
            }
            let mut rest = &src[at + word.len()..];
            if rest.starts_with('<') {
                // Skip the type argument: `invoke<Record<string, T>>(` ends at the first `>(`.
                let Some(close) = rest.find(">(") else {
                    continue;
                };
                rest = &rest[close + 1..];
            }
            let Some(args) = rest.strip_prefix('(') else {
                continue; // `import { invoke }`, or `invoke` in a comment
            };
            let args = args.trim_start();
            let Some(quote) = args
                .chars()
                .next()
                .filter(|c| matches!(c, '"' | '\'' | '`'))
            else {
                continue;
            };
            if let Some(end) = args[1..].find(quote) {
                names.push(&args[1..1 + end]);
            }
        }
        names
    }

    #[test]
    fn invoke_parser_reads_every_call_shape() {
        let src = r#"
            import { invoke, Channel } from "@tauri-apps/api/core";
            // Keep all `invoke` calls here.
            a: () => invoke<DbStats>("db_stats"),
            b: () => invoke<[string, number][]>("list_tags"),
            c: () => invoke<Record<string, Foo<Bar>>>('nested'),
            d: (x) =>
              invoke<ThreadDetail | null>(
                "get_thread",
                { x },
              ),
            e: () => invoke("track_event", { name }),
            f: () => reinvoke("not_a_command"),
        "#;
        assert_eq!(
            invoked_commands(src),
            [
                "db_stats",
                "list_tags",
                "nested",
                "get_thread",
                "track_event"
            ]
        );
    }

    /// Every command in generate_handler! has a typed wrapper (api.ts, or telemetry.ts, which
    /// api.ts re-exports), and every wrapper names a registered command. bce5702 registered six
    /// commands with no wrapper, so the Settings overrides its changeset announced had no UI.
    #[test]
    fn every_command_has_a_frontend_wrapper_and_every_wrapper_a_command() {
        // Invoked directly, not through a wrapper: the splash handshake in main.tsx and
        // ErrorBoundary.tsx, which must work even when the app's own modules fail to load.
        const INVOKED_OUTSIDE_WRAPPERS: &[&str] = &["set_complete"];
        let wrapper_sources = [
            include_str!("../../src/lib/api.ts"),
            include_str!("../../src/lib/telemetry.ts"),
        ];
        let wrapped: BTreeSet<&str> = wrapper_sources
            .iter()
            .flat_map(|src| invoked_commands(src))
            .chain(INVOKED_OUTSIDE_WRAPPERS.iter().copied())
            .collect();
        let registered = registered_commands();
        assert!(
            registered.contains("db_stats") && registered.len() > 50,
            "{registered:?}"
        );

        let missing: Vec<_> = registered.difference(&wrapped).collect();
        assert!(
            missing.is_empty(),
            "registered in generate_handler! but no wrapper in apps/desktop/src/lib/api.ts \
             invokes them: {missing:?}. Add a typed wrapper (and a caller), or unregister them."
        );
        let unknown: Vec<_> = wrapped.difference(&registered).collect();
        assert!(
            unknown.is_empty(),
            "the frontend invokes commands that generate_handler! doesn't register \
             (they fail at runtime): {unknown:?}"
        );
    }
}
