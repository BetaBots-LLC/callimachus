pub mod agent;
mod error;
mod indexer;
pub mod secrets;

// Public so the sidecar binaries (MCP server, `cal` CLI) can reuse the core.
pub mod cleanup;
pub mod cli_core;
pub mod context;
pub mod db;
pub mod embed;
pub mod export;
pub mod gitlink;
pub mod integration;
pub mod knowledge;
pub mod mcp_server;
pub mod search;
pub mod snapshot;

use error::AppResult;
use search::{SearchFilters, SearchHit, ThreadDetail, ThreadSummary};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};

/// Guards against launching more than one background embedding job at a time.
#[derive(Default)]
struct EmbedJob(AtomicBool);

/// Guards against more than one background re-index running at a time.
#[derive(Default)]
struct IndexJob(AtomicBool);

/// Guards against more than one project-distill running at a time; clearing it mid-run
/// (via `cancel_distill`) also signals the running job to stop.
#[derive(Default)]
struct DistillJob(AtomicBool);

/// Tracks frontend + backend startup readiness so the splash window is dismissed only
/// once BOTH are ready (the Tauri splashscreen pattern).
#[derive(Default)]
struct SetupState(Mutex<SetupFlags>);
#[derive(Default)]
struct SetupFlags {
    frontend: bool,
    backend: bool,
}

/// Mark a startup task done; when BOTH frontend and backend are ready, close the splash
/// window and reveal the main window.
fn complete_setup(app: &AppHandle, task: &str) {
    let reveal = {
        let state = app.state::<SetupState>();
        let mut flags = match state.0.lock() {
            Ok(f) => f,
            Err(_) => return,
        };
        match task {
            "frontend" => flags.frontend = true,
            "backend" => flags.backend = true,
            _ => {}
        }
        flags.frontend && flags.backend
    };
    if reveal {
        if let Some(splash) = app.get_webview_window("splashscreen") {
            let _ = splash.close();
        }
        if let Some(main) = app.get_webview_window("main") {
            let _ = main.show();
            let _ = main.set_focus();
        }
    }
}

/// Frontend signals it has loaded its initial data (the other half of `complete_setup`).
#[tauri::command]
fn set_complete(app: AppHandle, task: String) {
    complete_setup(&app, &task);
}

/// Cancellation token for the in-flight chat stream (one generation at a time).
#[derive(Default)]
struct ChatGeneration(Mutex<Option<tokio_util::sync::CancellationToken>>);

/// Pending shell-command approvals, keyed by tool call id. `approve_tool` resolves
/// the matching one-shot, unblocking the awaiting tool execution.
#[derive(Default)]
struct PendingApprovals(
    Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<bool>>>,
);

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
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as u32;
            let json = {
                let pool = app.state::<db::ReadPool>();
                let conn = pool
                    .0
                    .get()
                    .map_err(|e| anyhow::anyhow!("read pool: {e}"))?;
                let hits = search::search(
                    &conn,
                    &query,
                    &SearchFilters {
                        limit: Some(limit),
                        ..Default::default()
                    },
                )?;
                serde_json::to_string(&hits)?
            };
            let _ = ch.send(result(format!(
                "{} results",
                json.matches("\"threadId\"").count()
            )));
            Ok(json)
        }
        "get_thread" => {
            let tid = args.get("thread_id").and_then(|v| v.as_i64()).unwrap_or(0);
            let packed = {
                let pool = app.state::<db::ReadPool>();
                let conn = pool
                    .0
                    .get()
                    .map_err(|e| anyhow::anyhow!("read pool: {e}"))?;
                context::pack_thread(&conn, tid, context::DEFAULT_BUDGET_CHARS)?
                    .unwrap_or_else(|| "thread not found".to_string())
            };
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
            let output = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&command)
                .output()
                .await?;
            let mut out = String::from_utf8_lossy(&output.stdout).into_owned();
            let err = String::from_utf8_lossy(&output.stderr);
            if !err.trim().is_empty() {
                out.push_str("\n[stderr]\n");
                out.push_str(&err);
            }
            if out.chars().count() > 12_000 {
                out = format!(
                    "{}\n…(truncated)",
                    out.chars().take(12_000).collect::<String>()
                );
            }
            let _ = ch.send(result(out.clone()));
            Ok(out)
        }
        other => Ok(format!("unknown tool: {other}")),
    }
}

/// Lightweight counts for the dashboard / verification.
#[derive(Debug, Serialize)]
pub struct DbStats {
    pub threads: i64,
    pub messages: i64,
    pub sources: i64,
}

fn lock<'a>(db: &'a db::Db) -> AppResult<std::sync::MutexGuard<'a, rusqlite::Connection>> {
    db.0.lock()
        .map_err(|e| anyhow::anyhow!("db lock poisoned: {e}").into())
}

/// Same as [`lock`] but anyhow-typed, for the background `run_project_distill` helper.
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
fn db_stats(pool: tauri::State<'_, db::ReadPool>) -> AppResult<DbStats> {
    let conn = read(&pool)?;
    let count = |sql: &str| -> rusqlite::Result<i64> { conn.query_row(sql, [], |r| r.get(0)) };
    Ok(DbStats {
        threads: count("SELECT COUNT(*) FROM threads").map_err(anyhow::Error::from)?,
        messages: count("SELECT COUNT(*) FROM messages").map_err(anyhow::Error::from)?,
        sources: count("SELECT COUNT(*) FROM sources").map_err(anyhow::Error::from)?,
    })
}

/// Rich index analytics for the dashboard: per-source / per-role breakdowns,
/// date range, embedding coverage, and top projects.
#[tauri::command]
fn index_stats(pool: tauri::State<'_, db::ReadPool>) -> AppResult<search::Stats> {
    let conn = read(&pool)?;
    Ok(search::stats(&conn)?)
}

/// Proactive Coach dashboard: a daily-activity heatmap + the last week's distilled
/// decisions and gotchas.
#[tauri::command]
fn coach_overview(pool: tauri::State<'_, db::ReadPool>) -> AppResult<search::CoachOverview> {
    let conn = read(&pool)?;
    Ok(search::coach_overview(
        &conn,
        chrono::Utc::now().timestamp(),
    )?)
}

/// The git commits a thread likely produced (inferred file-overlap links). Read-only.
#[tauri::command]
fn thread_commits(
    pool: tauri::State<'_, db::ReadPool>,
    thread_id: i64,
) -> AppResult<Vec<gitlink::CommitLink>> {
    let conn = read(&pool)?;
    Ok(gitlink::linked_commits(&conn, thread_id)?)
}

/// (Re)compute git linkage for a thread's project by reading its `git log`, then return the
/// thread's commits. Writes (shells out to git + stores links), so it takes the writer lock.
#[tauri::command]
fn link_thread_commits(
    db: tauri::State<'_, db::Db>,
    thread_id: i64,
) -> AppResult<Vec<gitlink::CommitLink>> {
    let conn = lock(&db)?;
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
    Ok(gitlink::linked_commits(&conn, thread_id)?)
}

/// Oldest-first list of threads with their size, for the storage-cleanup UI.
#[tauri::command]
fn cleanup_candidates(
    pool: tauri::State<'_, db::ReadPool>,
    before: Option<i64>,
    sources: Option<Vec<String>>,
    limit: Option<i64>,
) -> AppResult<Vec<cleanup::CleanupRow>> {
    let conn = read(&pool)?;
    Ok(cleanup::candidates(
        &conn,
        before,
        &sources.unwrap_or_default(),
        limit.unwrap_or(200),
    )?)
}

/// Permanently delete the given threads (cascades to messages, FTS, vectors).
#[tauri::command]
fn delete_threads(db: tauri::State<'_, db::Db>, ids: Vec<i64>) -> AppResult<usize> {
    let mut conn = lock(&db)?;
    Ok(cleanup::delete_threads(&mut conn, &ids)?)
}

/// Reclaim disk space freed by deletes (VACUUM). Runs on a DEDICATED connection: VACUUM
/// rewrites the whole file and would otherwise hold the shared mutex for the entire
/// (multi-second) rewrite, freezing every other command. On its own connection it still
/// takes SQLite's write lock but never the Rust mutex, so the rest of the UI stays live.
#[tauri::command]
async fn vacuum_db() -> AppResult<()> {
    tauri::async_runtime::spawn_blocking(|| -> anyhow::Result<()> {
        let conn = db::open(&db::default_index_path())?;
        cleanup::vacuum(&conn)?;
        Ok(())
    })
    .await
    .map_err(|e| anyhow::anyhow!("vacuum task: {e}"))??;
    Ok(())
}

/// Kick off a re-index of every source in the BACKGROUND on a dedicated connection, so
/// the button returns instantly and the UI stays responsive. Emits `index:done` (with
/// the report) when finished; no-op if one is already running.
#[tauri::command]
fn index_all(app: AppHandle) -> AppResult<()> {
    if app.state::<EmbedJob>().0.load(Ordering::Relaxed) {
        return Ok(()); // a semantic build is writing — don't fight for the lock
    }
    // Note: a running distill does NOT block reindex — the distill loop yields to us.
    if app.state::<IndexJob>().0.swap(true, Ordering::SeqCst) {
        return Ok(()); // already running
    }
    std::thread::spawn(move || {
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
            Ok(r)
        });
        app.state::<IndexJob>().0.store(false, Ordering::SeqCst);
        let report = report.unwrap_or_else(|e| {
            eprintln!("[index] {e}");
            indexer::IndexReport::default()
        });
        let _ = app.emit("index:done", report);
        // Newly indexed threads may need distilling — drain them if auto-distill is on.
        auto_distill_kick(&app);
    });
    Ok(())
}

/// Whether a background re-index is in progress (for the Reindex button state).
#[tauri::command]
fn indexing_status(job: tauri::State<'_, IndexJob>) -> bool {
    job.0.load(Ordering::Relaxed)
}

/// Index a single source by kind ("claude_code" | "codex" | "cursor").
#[tauri::command]
fn index_source(kind: String) -> AppResult<indexer::IndexReport> {
    let mut conn = db::open(&db::default_index_path())?;
    let noop = &mut || {};
    let report = match kind.as_str() {
        "claude_code" => indexer::claude::scan(&mut conn, noop)?,
        "codex" => indexer::codex::scan(&mut conn, noop)?,
        "cursor" => indexer::cursor::scan(&mut conn, noop)?,
        "gemini" => indexer::gemini::scan(&mut conn, noop)?,
        "qwen" => indexer::qwen::scan(&mut conn, noop)?,
        "goose" => indexer::goose::scan(&mut conn, noop)?,
        "opencode" => indexer::opencode::scan(&mut conn, noop)?,
        "continue" => indexer::continue_cli::scan(&mut conn, noop)?,
        "cline" => indexer::cline::scan(&mut conn, noop)?,
        "roo" => indexer::roo::scan(&mut conn, noop)?,
        "kilo" => indexer::kilo::scan(&mut conn, noop)?,
        other => return Err(anyhow::anyhow!("unknown source kind: {other}").into()),
    };
    Ok(report)
}

#[tauri::command]
fn search_threads(
    pool: tauri::State<'_, db::ReadPool>,
    embedder: tauri::State<'_, embed::Embedder>,
    query: String,
    filters: Option<SearchFilters>,
) -> AppResult<Vec<SearchHit>> {
    let filters = filters.unwrap_or_default();
    let hits = if filters.hybrid {
        // Embed the query BEFORE touching the DB so the (multi-second on first call)
        // inference never pins a connection. Then read on the pool — concurrent with
        // other reads and the writer.
        let qv = embed::embed_query(&embedder, &query)?;
        let conn = read(&pool)?;
        search::hybrid_vec(&conn, &query, qv.as_deref(), &filters)?
    } else {
        let conn = read(&pool)?;
        search::search(&conn, &query, &filters)?
    };
    Ok(hits)
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
fn embedding_status(
    pool: tauri::State<'_, db::ReadPool>,
    job: tauri::State<'_, EmbedJob>,
) -> AppResult<EmbedStatus> {
    let conn = read(&pool)?;
    let (done, total) = embed::embed_progress(&conn)?;
    Ok(EmbedStatus {
        done,
        total,
        running: job.0.load(Ordering::Relaxed),
    })
}

/// True for transient SQLite contention ("database is locked"/"busy") so the
/// embed job retries rather than aborting when another process holds a write lock.
fn is_db_locked(e: &anyhow::Error) -> bool {
    let s = e.to_string().to_lowercase();
    s.contains("database is locked") || s.contains("database is busy")
}

/// Run a short DB write, retrying briefly on transient "database is locked" (another
/// process — the MCP server, a second app instance — momentarily holds the write
/// lock) instead of failing the user's click. The mutex is released between tries.
fn write_retry<T>(
    db: &db::Db,
    mut op: impl FnMut(&rusqlite::Connection) -> anyhow::Result<T>,
) -> AppResult<T> {
    let mut last: Option<anyhow::Error> = None;
    for attempt in 0..8 {
        let res = {
            let conn = lock(db)?;
            op(&conn)
        };
        match res {
            Ok(v) => return Ok(v),
            Err(e) if is_db_locked(&e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(120 * (attempt + 1)));
            }
            Err(e) => return Err(e.into()),
        }
    }
    Err(last
        .unwrap_or_else(|| anyhow::anyhow!("database stayed locked"))
        .into())
}

/// Kick off (or no-op if already running) a background job that embeds all pending
/// messages batch-by-batch, releasing the DB lock between batches.
#[tauri::command]
fn build_embeddings(app: AppHandle) -> AppResult<()> {
    if app.state::<IndexJob>().0.load(Ordering::Relaxed) {
        return Ok(()); // a reindex is writing — don't fight it for the write lock
    }
    // Note: a running distill does NOT block the embed build — the distill loop yields.
    let job = app.state::<EmbedJob>();
    if job.0.swap(true, Ordering::SeqCst) {
        return Ok(()); // already running
    }
    std::thread::spawn(move || {
        // Smaller batches keep each locked write + each inference short, so the UI
        // (and ambient-recall) stay snappy while the job runs in the background.
        const BATCH: usize = 64;
        let db = app.state::<db::Db>();
        let embedder = app.state::<embed::Embedder>();
        // Snapshot totals ONCE; then increment a running counter per batch so progress
        // events carry counts without re-running COUNT(*) scans under the lock.
        let (mut done, total) = match db.0.lock() {
            Ok(conn) => embed::embed_progress(&conn).unwrap_or((0, 0)),
            Err(_) => (0, 0),
        };
        // Consecutive batches we couldn't persist because another writer held the lock.
        // We re-queue instead of aborting (the manual Reindex is now mutually exclusive,
        // but the file watcher still indexes incrementally), bailing only after a long
        // standoff so a truly stuck writer can't spin us forever.
        let mut deferrals = 0u32;
        loop {
            // 1. Locked, fast: claim the next batch of pending messages.
            let rows = {
                let Ok(conn) = db.0.lock() else { break };
                match embed::pending_batch(&conn, BATCH) {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("[embed] {e}");
                        break;
                    }
                }
            };
            if rows.is_empty() {
                break;
            }
            // 2. UNLOCKED: the heavy model inference runs with the DB lock released,
            //    so search/recent/stats stay responsive while embeddings build.
            let (owners, texts) = embed::chunk_messages(&rows);
            let vecs = match embedder.embed(texts) {
                Ok(v) => v,
                Err(e) => {
                    // Don't let one bad batch stall the whole job: mark these messages
                    // embedded (they just won't have vectors — FTS still finds them) and
                    // move on, instead of aborting at this point forever.
                    eprintln!("[embed] batch failed, skipping: {e}");
                    let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
                    if let Ok(conn) = db.0.lock() {
                        let _ = embed::mark_embedded(&conn, &ids);
                    }
                    done += rows.len() as i64;
                    let _ = app.emit("embed:progress", EmbedProgressEvent { done, total });
                    continue;
                }
            };
            // 3. Locked, fast: persist the vectors + mark the messages embedded.
            //    Another writer (the MCP server, `cal`, a second app instance) can
            //    hold the write lock; retry "database is locked" instead of aborting
            //    the whole job, releasing the mutex between tries so the UI breathes.
            let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
            let mut stored = false;
            for _ in 0..10 {
                let res = {
                    let Ok(mut conn) = db.0.lock() else { break };
                    embed::store_batch(&mut conn, &ids, &owners, &vecs)
                };
                match res {
                    Ok(()) => {
                        stored = true;
                        break;
                    }
                    Err(e) if is_db_locked(&e) => {
                        std::thread::sleep(std::time::Duration::from_millis(300));
                    }
                    Err(e) => {
                        eprintln!("[embed] {e}");
                        break;
                    }
                }
            }
            if !stored {
                deferrals += 1;
                if deferrals > 40 {
                    eprintln!("[embed] giving up — database stayed locked too long");
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
                continue; // re-queue this batch; don't kill the whole job
            }
            deferrals = 0;
            done += rows.len() as i64;
            let _ = app.emit("embed:progress", EmbedProgressEvent { done, total });
        }
        app.state::<EmbedJob>().0.store(false, Ordering::SeqCst);
        // Fold the WAL back after the build's many small writes (PASSIVE never blocks).
        if let Ok(conn) = db.0.lock() {
            let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");
        }
        let _ = app.emit("embed:done", ());
    });
    Ok(())
}

#[tauri::command]
fn recent_threads(
    pool: tauri::State<'_, db::ReadPool>,
    filters: Option<SearchFilters>,
) -> AppResult<Vec<ThreadSummary>> {
    let conn = read(&pool)?;
    Ok(search::recent_threads(&conn, &filters.unwrap_or_default())?)
}

#[tauri::command]
fn get_thread(
    pool: tauri::State<'_, db::ReadPool>,
    thread_id: i64,
) -> AppResult<Option<ThreadDetail>> {
    let conn = read(&pool)?;
    Ok(search::thread_detail(&conn, thread_id)?)
}

/// Star or unstar a thread ("collections").
#[tauri::command]
fn set_star(db: tauri::State<'_, db::Db>, thread_id: i64, starred: bool) -> AppResult<()> {
    let conn = lock(&db)?;
    search::set_star(&conn, thread_id, starred)?;
    Ok(())
}

/// Replace a thread's tags with the given set.
#[tauri::command]
fn set_thread_tags(
    db: tauri::State<'_, db::Db>,
    thread_id: i64,
    tags: Vec<String>,
) -> AppResult<()> {
    let mut conn = lock(&db)?;
    let now = chrono::Utc::now().timestamp();
    search::set_thread_tags(&mut conn, thread_id, &tags, now)?;
    Ok(())
}

/// All tags in the index with their thread counts, for the filter chips.
#[tauri::command]
fn list_tags(pool: tauri::State<'_, db::ReadPool>) -> AppResult<Vec<(String, i64)>> {
    let conn = read(&pool)?;
    Ok(search::list_tags(&conn)?)
}

/// Open TODOs across the corpus (free heuristic knowledge tier), newest first,
/// optionally scoped to a project-path substring and/or a source kind.
#[tauri::command]
fn list_open_todos(
    pool: tauri::State<'_, db::ReadPool>,
    query: Option<String>,
    project: Option<String>,
    source: Option<String>,
) -> AppResult<Vec<knowledge::TodoFact>> {
    let conn = read(&pool)?;
    Ok(knowledge::list_open_todos(
        &conn,
        query.as_deref(),
        project.as_deref(),
        source.as_deref(),
        500,
    )?)
}

/// Current distillation engine config (enabled + provider/model).
#[tauri::command]
fn knowledge_config(pool: tauri::State<'_, db::ReadPool>) -> AppResult<knowledge::KnowledgeConfig> {
    let conn = read(&pool)?;
    Ok(knowledge::get_config(&conn)?)
}

/// Toggle background auto-distillation. Turning it ON kicks an immediate drain.
#[tauri::command]
fn set_auto_distill(app: AppHandle, on: bool) -> AppResult<()> {
    {
        let db = app.state::<db::Db>();
        let conn = lock(&db)?;
        knowledge::set_auto_distill(&conn, on)?;
    }
    if on {
        auto_distill_kick(&app);
    }
    Ok(())
}

/// Enable/disable distillation and pick the engine. Enabling is the user's consent.
#[tauri::command]
fn set_knowledge_config(
    app: AppHandle,
    enabled: bool,
    provider: Option<String>,
    model: Option<String>,
) -> AppResult<()> {
    let db = app.state::<db::Db>();
    // Quick: write the config; clearing on OFF is one fast statement.
    let was_enabled = {
        let conn = lock(&db)?;
        let prev = knowledge::get_config(&conn)?.enabled;
        knowledge::set_config(&conn, enabled, provider.as_deref(), model.as_deref())?;
        if !enabled {
            knowledge::clear_heuristic(&conn)?;
        }
        prev
    };
    // Turning ON backfills todos from already-indexed text — in the BACKGROUND, in
    // short batches, so the toggle returns instantly and the UI stays snappy.
    if enabled && !was_enabled {
        let app = app.clone();
        std::thread::spawn(move || {
            let db = app.state::<db::Db>();
            let now = chrono::Utc::now().timestamp();
            if let Err(e) = knowledge::backfill_todos(&db, now) {
                eprintln!("[knowledge] backfill: {e}");
            }
            let _ = app.emit("knowledge:todos-ready", ());
        });
    }
    Ok(())
}

/// Cached distilled knowledge (summary/decisions/gotchas/todos) for one thread.
#[tauri::command]
fn thread_knowledge(
    pool: tauri::State<'_, db::ReadPool>,
    thread_id: i64,
) -> AppResult<knowledge::ThreadKnowledge> {
    let conn = read(&pool)?;
    Ok(knowledge::get_thread_knowledge(&conn, thread_id)?)
}

/// Distill one thread now (decisions/gotchas/summary) using the configured engine, and
/// return the fresh knowledge. The LLM call runs WITHOUT the DB lock held.
#[tauri::command]
async fn distill_thread(
    db: tauri::State<'_, db::Db>,
    embedder: tauri::State<'_, embed::Embedder>,
    thread_id: i64,
) -> AppResult<knowledge::ThreadKnowledge> {
    // Resolve engine + pack the transcript under the lock, then release it.
    let (provider, model, key, packed) = {
        let conn = lock(&db)?;
        let (provider, model, key) = resolve_distill_engine(&conn)?;
        let packed = context::pack_thread(&conn, thread_id, context::DEFAULT_BUDGET_CHARS)?
            .ok_or_else(|| anyhow::anyhow!("thread {thread_id} not found"))?;
        (provider, model, key, packed)
    };

    match agent::distill(&provider, &model, key.as_deref(), &packed).await {
        Ok(distilled) => {
            {
                let mut conn = lock(&db)?;
                let now = chrono::Utc::now().timestamp();
                knowledge::store_distilled(&mut conn, thread_id, &distilled, now)?;
            }
            // Embed the new facts so they're recallable across threads (lock released
            // during inference). Best-effort — recall just lags if it fails.
            if let Err(e) = embed::embed_pending_facts(&db, &embedder) {
                eprintln!("[knowledge] embed facts: {e}");
            }
            let conn = lock(&db)?;
            Ok(knowledge::get_thread_knowledge(&conn, thread_id)?)
        }
        Err(e) => {
            let conn = lock(&db)?;
            // Store a short, sanitized summary — not the raw provider error, which can
            // echo HTTP status / URLs / response bodies.
            let msg: String = e
                .to_string()
                .lines()
                .next()
                .unwrap_or("distillation failed")
                .chars()
                .take(160)
                .collect();
            knowledge::set_error(&conn, thread_id, &msg)?;
            Err(e.into())
        }
    }
}

// ---- fact curation (pin / edit / hide) ----

/// Pin or unpin a distilled fact (pinned ranks first + survives re-distill).
#[tauri::command]
fn set_fact_pinned(db: tauri::State<'_, db::Db>, fact_id: i64, pinned: bool) -> AppResult<()> {
    write_retry(&db, |conn| {
        knowledge::set_fact_pinned(conn, fact_id, pinned)
    })
}

/// Hide (soft-delete) or restore a fact. Hidden facts are kept as a tombstone so
/// re-distillation won't bring them back.
#[tauri::command]
fn hide_fact(db: tauri::State<'_, db::Db>, fact_id: i64, hidden: bool) -> AppResult<()> {
    write_retry(&db, |conn| {
        knowledge::set_fact_hidden(conn, fact_id, hidden)
    })
}

/// Mark a TODO done (drops out of open lists) or reopen it.
#[tauri::command]
fn set_todo_done(db: tauri::State<'_, db::Db>, fact_id: i64, done: bool) -> AppResult<()> {
    write_retry(&db, |conn| knowledge::set_todo_done(conn, fact_id, done))
}

/// Write-back: record a decision or gotcha for a project, then embed it for recall.
#[tauri::command]
fn remember(
    db: tauri::State<'_, db::Db>,
    embedder: tauri::State<'_, embed::Embedder>,
    project: String,
    kind: String,
    text: String,
) -> AppResult<()> {
    write_retry(&db, |conn| {
        knowledge::record_fact(
            conn,
            &project,
            &kind,
            &text,
            None,
            chrono::Utc::now().timestamp(),
        )
        .map(|_| ())
    })?;
    if let Err(e) = embed::embed_pending_facts(&db, &embedder) {
        eprintln!("[remember] embed: {e}");
    }
    Ok(())
}

/// Edit a fact's text. Re-embeds it so cross-thread recall matches the new wording.
#[tauri::command]
fn edit_fact(
    db: tauri::State<'_, db::Db>,
    embedder: tauri::State<'_, embed::Embedder>,
    fact_id: i64,
    text: String,
) -> AppResult<()> {
    write_retry(&db, |conn| knowledge::edit_fact(conn, fact_id, &text))?;
    if let Err(e) = embed::embed_pending_facts(&db, &embedder) {
        eprintln!("[knowledge] re-embed edited fact: {e}");
    }
    Ok(())
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
    let (provider, model, key, decisions) = {
        let conn = read(&pool)?;
        let (provider, model, key) = resolve_distill_engine(&conn)?;
        let decisions = knowledge::project_decisions(&conn, &project)?;
        (provider, model, key, decisions)
    };
    if decisions.len() < 2 {
        return Ok(Vec::new());
    }
    let texts: Vec<String> = decisions.iter().map(|(_, t)| t.clone()).collect();
    let pairs = agent::find_conflicts(&provider, &model, key.as_deref(), &texts).await?;
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

// ---- project memory ----

/// Distinct projects with distillation coverage, for the Project Memory picker.
#[tauri::command]
fn list_projects(pool: tauri::State<'_, db::ReadPool>) -> AppResult<Vec<knowledge::ProjectInfo>> {
    let conn = read(&pool)?;
    Ok(knowledge::list_projects(&conn)?)
}

/// Aggregated, durable memory (decisions/gotchas/open todos + coverage) for one project.
#[tauri::command]
fn project_memory(
    pool: tauri::State<'_, db::ReadPool>,
    project: String,
) -> AppResult<knowledge::ProjectMemory> {
    let conn = read(&pool)?;
    Ok(knowledge::get_project_memory(&conn, &project, 60)?)
}

/// Flatten a project's facts into markdown notes for the brief / memory file.
fn format_memory_notes(m: &knowledge::ProjectMemory) -> String {
    fn section(s: &mut String, title: &str, facts: &[knowledge::MemoryFact]) {
        if facts.is_empty() {
            return;
        }
        s.push_str(&format!("## {title}\n"));
        for f in facts {
            s.push_str(&format!("- {}\n", f.text.trim()));
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
    let (provider, model, key, notes) = {
        let conn = read(&pool)?;
        let (provider, model, key) = resolve_distill_engine(&conn)?;
        let mem = knowledge::get_project_memory(&conn, &project, 80)?;
        (provider, model, key, format_memory_notes(&mem))
    };
    if notes.trim().is_empty() {
        return Ok(String::new());
    }
    Ok(agent::project_brief(&provider, &model, key.as_deref(), &project, &notes).await?)
}

/// Write a project's memory (optionally with an LLM brief) to `<project>/.callimachus/
/// memory.md` so agents can be pointed at it. Returns the written path.
#[tauri::command]
async fn write_project_memory_file(
    pool: tauri::State<'_, db::ReadPool>,
    project: String,
    with_brief: bool,
) -> AppResult<String> {
    let (mem, engine) = {
        let conn = read(&pool)?;
        let mem = knowledge::get_project_memory(&conn, &project, 200)?;
        let engine = if with_brief {
            resolve_distill_engine(&conn).ok()
        } else {
            None
        };
        (mem, engine)
    };
    let brief = match engine {
        Some((provider, model, key)) => {
            let notes = format_memory_notes(&mem);
            if notes.trim().is_empty() {
                None
            } else {
                agent::project_brief(&provider, &model, key.as_deref(), &project, &notes)
                    .await
                    .ok()
            }
        }
        None => None,
    };
    let md = export::project_memory_md(&project, &mem, brief.as_deref());
    let dir = std::path::Path::new(&project).join(".callimachus");
    std::fs::create_dir_all(&dir).map_err(anyhow::Error::from)?;
    let path = dir.join("memory.md");
    std::fs::write(&path, md).map_err(anyhow::Error::from)?;
    Ok(path.to_string_lossy().into_owned())
}

/// Write/refresh the Callimachus memory block in a project's agent context file (AGENTS.md
/// / CLAUDE.md), preserving the user's own content. So any agent that reads the file opens
/// with the project's distilled memory. `project` is the canonical key (a repo path).
#[tauri::command]
fn write_agent_memory_file(
    pool: tauri::State<'_, db::ReadPool>,
    project: String,
    filename: String,
) -> AppResult<String> {
    let mem = {
        let conn = read(&pool)?;
        knowledge::get_project_memory(&conn, &project, 100)?
    };
    let body = export::agent_memory_md(&project, &mem, None);
    let path = std::path::Path::new(&project).join(&filename);
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    std::fs::write(&path, export::upsert_managed_block(&existing, &body))
        .map_err(anyhow::Error::from)?;
    Ok(path.to_string_lossy().into_owned())
}

/// Whether a project distill is in progress (for the Build-memory button state).
#[tauri::command]
fn distilling_status(job: tauri::State<'_, DistillJob>) -> bool {
    job.0.load(Ordering::Relaxed)
}

/// Stop a running project distill at the next thread boundary.
#[tauri::command]
fn cancel_distill(job: tauri::State<'_, DistillJob>) {
    job.0.store(false, Ordering::SeqCst);
}

/// Threads per auto-distill round before re-checking the gates (so a reindex/embed can
/// preempt promptly, and the first run on a big history doesn't commit to it all at once).
const AUTO_DISTILL_BATCH: i64 = 25;

/// Distill every not-yet-distilled thread in a project, in the BACKGROUND, so the project's
/// memory fills in. Paced + cancellable; yields to a reindex / embed build if one starts.
/// Emits distill:progress / distill:done.
#[tauri::command]
fn distill_project(app: AppHandle, project: String) -> AppResult<()> {
    if app.state::<IndexJob>().0.load(Ordering::Relaxed)
        || app.state::<EmbedJob>().0.load(Ordering::Relaxed)
    {
        return Ok(()); // a reindex / embed build is writing — start later
    }
    if app.state::<DistillJob>().0.swap(true, Ordering::SeqCst) {
        return Ok(()); // already running
    }
    tauri::async_runtime::spawn(async move {
        let ids = {
            let db = app.state::<db::Db>();
            db.0.lock()
                .ok()
                .and_then(|c| knowledge::project_pending_threads(&c, &project).ok())
        };
        if let Some(ids) = ids {
            if let Err(e) = run_distill(&app, ids).await {
                eprintln!("[distill] {e}");
            }
        }
        app.state::<DistillJob>().0.store(false, Ordering::SeqCst);
        let _ = app.emit("distill:done", ());
    });
    Ok(())
}

/// Distill a worklist of thread ids with the resolved engine. Paced (so a cloud provider
/// isn't hammered), cancellable (DistillJob cleared), and yields to a user-initiated
/// reindex / embed build. Shared by the project distill and the background auto-distill.
async fn run_distill(app: &AppHandle, ids: Vec<i64>) -> anyhow::Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let db = app.state::<db::Db>();
    let embedder = app.state::<embed::Embedder>();
    let (provider, model, key) = {
        let conn = lock_anyhow(&db)?;
        resolve_distill_engine(&conn)?
    };
    let total = ids.len() as i64;
    let _ = app.emit("distill:progress", DistillProgressEvent { done: 0, total });
    for (i, tid) in ids.into_iter().enumerate() {
        // Stop if canceled, or yield to a user-initiated reindex / embed build.
        if !app.state::<DistillJob>().0.load(Ordering::Relaxed)
            || app.state::<IndexJob>().0.load(Ordering::Relaxed)
            || app.state::<EmbedJob>().0.load(Ordering::Relaxed)
        {
            break;
        }
        let packed = {
            let conn = lock_anyhow(&db)?;
            context::pack_thread(&conn, tid, context::DEFAULT_BUDGET_CHARS)?
        };
        let Some(packed) = packed else { continue };
        match agent::distill(&provider, &model, key.as_deref(), &packed).await {
            Ok(distilled) => {
                {
                    let mut conn = lock_anyhow(&db)?;
                    knowledge::store_distilled(
                        &mut conn,
                        tid,
                        &distilled,
                        chrono::Utc::now().timestamp(),
                    )?;
                }
                if let Err(e) = embed::embed_pending_facts(&db, &embedder) {
                    eprintln!("[distill] embed facts: {e}");
                }
            }
            Err(e) => {
                let conn = lock_anyhow(&db)?;
                let msg: String = e
                    .to_string()
                    .lines()
                    .next()
                    .unwrap_or("distillation failed")
                    .chars()
                    .take(160)
                    .collect();
                let _ = knowledge::set_error(&conn, tid, &msg);
            }
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
    Ok(())
}

/// Kick a background auto-distill drain: distill pending threads corpus-wide so the
/// knowledge surfaces (Ask / recall / Project Memory) self-populate. No-op unless
/// distillation AND auto-distill are enabled and nothing else is running. Drains in
/// batches, re-checking the gates each round, and yields to reindex / embed.
fn auto_distill_kick(app: &AppHandle) {
    if app.state::<IndexJob>().0.load(Ordering::Relaxed)
        || app.state::<EmbedJob>().0.load(Ordering::Relaxed)
    {
        return;
    }
    let ids = {
        let db = app.state::<db::Db>();
        let Ok(conn) = db.0.lock() else { return };
        let Ok(cfg) = knowledge::get_config(&conn) else {
            return;
        };
        if !cfg.enabled || !cfg.auto_distill {
            return;
        }
        knowledge::pending_threads(&conn, AUTO_DISTILL_BATCH).unwrap_or_default()
    };
    if ids.is_empty() {
        return;
    }
    if app.state::<DistillJob>().0.swap(true, Ordering::SeqCst) {
        return; // a distill is already running
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = run_distill(&app, ids).await {
            eprintln!("[auto-distill] {e}");
        }
        app.state::<DistillJob>().0.store(false, Ordering::SeqCst);
        let _ = app.emit("distill:done", ());
        // More may remain (and nothing preempted us) — drain the next batch.
        auto_distill_kick(&app);
    });
}

/// Cross-thread semantic recall of distilled DECISIONS, matched to a query.
#[tauri::command]
fn recall_decisions(
    pool: tauri::State<'_, db::ReadPool>,
    embedder: tauri::State<'_, embed::Embedder>,
    query: String,
    project: Option<String>,
    limit: Option<u32>,
) -> AppResult<Vec<knowledge::RecallHit>> {
    recall_facts(
        &pool,
        &embedder,
        "decision",
        &query,
        project.as_deref(),
        limit,
    )
}

/// Cross-thread semantic recall of distilled GOTCHAS, matched to a query.
#[tauri::command]
fn recall_gotchas(
    pool: tauri::State<'_, db::ReadPool>,
    embedder: tauri::State<'_, embed::Embedder>,
    query: String,
    project: Option<String>,
    limit: Option<u32>,
) -> AppResult<Vec<knowledge::RecallHit>> {
    recall_facts(
        &pool,
        &embedder,
        "gotcha",
        &query,
        project.as_deref(),
        limit,
    )
}

/// "Have I done this before?" — prior SESSIONS similar to a task description, each rolled up
/// from its matching decisions/gotchas. Embed OUTSIDE the DB, then KNN on the read pool.
#[tauri::command]
fn find_prior_work(
    pool: tauri::State<'_, db::ReadPool>,
    embedder: tauri::State<'_, embed::Embedder>,
    query: String,
    project: Option<String>,
    limit: Option<u32>,
) -> AppResult<Vec<knowledge::PriorWork>> {
    let Some(qv) = embed::embed_query(&embedder, &query)? else {
        return Ok(Vec::new());
    };
    let conn = read(&pool)?;
    Ok(knowledge::find_prior_work(
        &conn,
        &qv,
        project.as_deref(),
        limit.unwrap_or(8) as usize,
    )?)
}

/// Shared recall path: embed the query OUTSIDE the DB, then run the SQL KNN on the pool.
fn recall_facts(
    pool: &db::ReadPool,
    embedder: &embed::Embedder,
    kind: &str,
    query: &str,
    project: Option<&str>,
    limit: Option<u32>,
) -> AppResult<Vec<knowledge::RecallHit>> {
    let Some(qv) = embed::embed_query(embedder, query)? else {
        return Ok(Vec::new());
    };
    let conn = read(pool)?;
    Ok(knowledge::recall(
        &conn,
        &qv,
        kind,
        project,
        limit.unwrap_or(20) as usize,
    )?)
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
    pub context: String,
    pub sources: Vec<AskSource>,
}

/// Retrieve + pack the top threads for a question and resolve the distill engine. SYNC (no
/// LLM call) so callers run it under a lock, drop the conn, then await `agent::answer`.
/// `qv` is the PRE-EMBEDDED query vector (embed before locking). None = no engine or no
/// relevant threads. Shared by the desktop command, `cal ask`, and the MCP ask tool.
pub fn prepare_ask(
    conn: &rusqlite::Connection,
    question: &str,
    qv: Option<&[f32]>,
) -> anyhow::Result<Option<AskPrep>> {
    let (provider, model, key) = resolve_distill_engine(conn)?;
    let filters = SearchFilters {
        limit: Some(30),
        ..Default::default()
    };
    let hits = search::hybrid_vec(conn, question, qv, &filters)?;
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
        let excerpt = context::pack_thread(conn, h.thread_id, 2500)?.unwrap_or_default();
        context.push_str(&format!(
            "[thread {}] {}\n{excerpt}\n\n",
            h.thread_id,
            h.title.as_deref().unwrap_or("untitled")
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
        context,
        sources,
    }))
}

/// "Ask your history" (RAG): retrieve the most relevant threads for a question, pack
/// excerpts, and have the configured LLM answer with [thread N] citations.
#[tauri::command]
async fn ask_history(
    pool: tauri::State<'_, db::ReadPool>,
    embedder: tauri::State<'_, embed::Embedder>,
    question: String,
) -> AppResult<AskAnswer> {
    let q = question.trim().to_string();
    if q.is_empty() {
        return Err(anyhow::anyhow!("ask needs a question").into());
    }
    // Embed OUTSIDE the DB; retrieve + pack on a pooled read conn; LLM after the conn drops.
    let qv = embed::embed_query(&embedder, &q)?;
    let prep = {
        let conn = read(&pool)?;
        prepare_ask(&conn, &q, qv.as_deref())?
    };
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
fn search_by_file(
    pool: tauri::State<'_, db::ReadPool>,
    path: String,
) -> AppResult<Vec<ThreadSummary>> {
    let conn = read(&pool)?;
    Ok(search::threads_with_file(&conn, &path, 200)?)
}

// ---- agent chat ----

/// Stream a chat completion. Tokens are pushed over `on_token`; the full reply is
/// returned and the conversation persisted as a searchable in_app thread.
#[tauri::command]
#[allow(clippy::too_many_arguments)] // Tauri injects app/db/generation/channel; the rest are the chat params
async fn send_chat(
    app: AppHandle,
    db: tauri::State<'_, db::Db>,
    generation: tauri::State<'_, ChatGeneration>,
    on_token: tauri::ipc::Channel<agent::ChatChunk>,
    thread_id: String,
    provider: String,
    model: String,
    base_url: Option<String>,
    messages: Vec<agent::ChatMessage>,
) -> AppResult<String> {
    let key = secrets::get_key(&provider)?;
    // Publish a fresh cancellation token so cancel_chat can stop this stream.
    let token = tokio_util::sync::CancellationToken::new();
    *generation
        .0
        .lock()
        .map_err(|e| anyhow::anyhow!("generation lock: {e}"))? = Some(token.clone());

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
    {
        let mut conn = lock(&db)?;
        agent::persist_chat(&mut conn, &thread_id, &messages, &full)?;
    }
    if let Ok(mut g) = generation.0.lock() {
        *g = None;
    }
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

/// Abort the in-flight chat stream (if any). The partial reply is still persisted.
#[tauri::command]
fn cancel_chat(generation: tauri::State<'_, ChatGeneration>) -> AppResult<()> {
    if let Some(token) = generation
        .0
        .lock()
        .map_err(|e| anyhow::anyhow!("generation lock: {e}"))?
        .take()
    {
        token.cancel();
    }
    Ok(())
}

#[tauri::command]
fn set_api_key(provider: String, key: String) -> AppResult<()> {
    secrets::set_key(&provider, &key)?;
    Ok(())
}

#[tauri::command]
fn delete_api_key(provider: String) -> AppResult<()> {
    secrets::delete_key(&provider)?;
    Ok(())
}

#[tauri::command]
fn provider_has_key(provider: String) -> AppResult<bool> {
    Ok(secrets::has_key(&provider))
}

/// List the models a provider currently offers (from its API). Free-text entry in
/// the UI still works; this populates the suggestions with real, current options.
#[tauri::command]
async fn list_models(provider: String, base_url: Option<String>) -> AppResult<Vec<String>> {
    let key = secrets::get_key(&provider)?;
    Ok(agent::list_models(&provider, base_url.as_deref(), key.as_deref()).await?)
}

/// Pack a thread into an LLM-ready context blob (markdown + XML envelope, budgeted).
#[tauri::command]
fn thread_context(pool: tauri::State<'_, db::ReadPool>, thread_id: i64) -> AppResult<String> {
    let conn = read(&pool)?;
    let packed = context::pack_thread(&conn, thread_id, context::DEFAULT_BUDGET_CHARS)?
        .ok_or_else(|| anyhow::anyhow!("thread not found"))?;
    Ok(packed)
}

/// Obsidian-known vault folders, read from Obsidian's own config (no filesystem
/// scan). Checks the macOS / Linux / Windows config locations; only existing dirs
/// are returned. Empty when Obsidian isn't installed or registers no vaults.
#[tauri::command]
fn obsidian_vaults() -> Vec<String> {
    let mut configs: Vec<std::path::PathBuf> = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let home = std::path::PathBuf::from(home);
        configs.push(home.join("Library/Application Support/obsidian/obsidian.json"));
        configs.push(home.join(".config/obsidian/obsidian.json"));
    }
    if let Some(appdata) = std::env::var_os("APPDATA") {
        configs.push(std::path::PathBuf::from(appdata).join("obsidian/obsidian.json"));
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
) -> AppResult<String> {
    let md = export::to_obsidian(detail, synthesis);
    std::fs::create_dir_all(vault_dir).map_err(anyhow::Error::from)?;
    let path =
        std::path::Path::new(vault_dir).join(format!("{}.md", export::note_filename(detail)));
    std::fs::write(&path, md).map_err(anyhow::Error::from)?;
    Ok(path.to_string_lossy().into_owned())
}

/// Quick export: a thread → Obsidian note (transcript + `[[project]]` link), no LLM.
#[tauri::command]
fn export_thread(
    pool: tauri::State<'_, db::ReadPool>,
    thread_id: i64,
    vault_dir: String,
) -> AppResult<String> {
    let detail = {
        let conn = read(&pool)?;
        search::thread_detail(&conn, thread_id)?
            .ok_or_else(|| anyhow::anyhow!("thread not found"))?
    };
    write_note(&detail, None, &vault_dir)
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
];

/// First provider with a stored key (Ollama is keyless, so never auto-picked).
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

/// Resolve (provider, model) from an optional pinned choice, else auto-pick. A
/// pinned provider must have a stored key (except keyless Ollama).
fn resolve_synth(provider: Option<&str>, model: Option<&str>) -> AppResult<(String, String)> {
    match provider.filter(|p| !p.is_empty()) {
        Some(p) => {
            if p != "ollama" && !secrets::has_key(p) {
                return Err(anyhow::anyhow!("no API key stored for {p}").into());
            }
            let model = model
                .filter(|m| !m.is_empty())
                .map(str::to_string)
                .or_else(|| synth_default_model(p).map(str::to_string))
                .ok_or_else(|| anyhow::anyhow!("pick a model for {p}"))?;
            Ok((p.to_string(), model))
        }
        None => {
            let (p, m) = pick_synth_provider().ok_or_else(|| {
                anyhow::anyhow!("no API key set — add one in Settings to synthesize")
            })?;
            Ok((p.to_string(), m.to_string()))
        }
    }
}

/// Whether any cloud provider key is stored — gates the "Synthesize" action.
#[tauri::command]
fn can_synthesize() -> bool {
    pick_synth_provider().is_some()
}

/// Resolve (provider, model, api_key) for distillation from the saved engine config,
/// gated on distillation being enabled. Shared by the Tauri command and `cal distill`.
/// Ollama is keyless; cloud providers must have a stored key.
pub fn resolve_distill_engine(
    conn: &rusqlite::Connection,
) -> anyhow::Result<(String, String, Option<String>)> {
    let cfg = knowledge::get_config(conn)?;
    if !cfg.enabled {
        anyhow::bail!("distillation is off — enable it in Settings (local Ollama or an API key)");
    }
    let (provider, model) = resolve_synth(cfg.provider.as_deref(), cfg.model.as_deref())
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let key = if provider == "ollama" {
        None
    } else {
        secrets::get_key(&provider)?
    };
    Ok((provider, model, key))
}

/// Synthesized export: pack the thread, run the chosen (or first available) LLM to
/// extract a summary + decisions / gotchas / TODOs, and write a knowledge-grade
/// Obsidian note (synthesis above the transcript) into `vault_dir`.
#[tauri::command]
async fn synthesize_export(
    pool: tauri::State<'_, db::ReadPool>,
    thread_id: i64,
    vault_dir: String,
    provider: Option<String>,
    model: Option<String>,
) -> AppResult<String> {
    let (provider, model) = resolve_synth(provider.as_deref(), model.as_deref())?;
    // Pull detail + packed transcript on a pooled read conn, dropped before the network call.
    let (detail, packed) = {
        let conn = read(&pool)?;
        let detail = search::thread_detail(&conn, thread_id)?
            .ok_or_else(|| anyhow::anyhow!("thread not found"))?;
        let packed = context::pack_thread(&conn, thread_id, context::DEFAULT_BUDGET_CHARS)?
            .ok_or_else(|| anyhow::anyhow!("thread not found"))?;
        (detail, packed)
    };
    let key = secrets::get_key(&provider)?;
    let synthesis = agent::synthesize(&provider, &model, key.as_deref(), &packed).await?;
    write_note(&detail, Some(&synthesis), &vault_dir)
}

/// Pack a thread and open it as context in a fresh CLI agent session (default: claude).
#[tauri::command]
fn open_thread_in_cli(
    pool: tauri::State<'_, db::ReadPool>,
    thread_id: i64,
    program: Option<String>,
) -> AppResult<String> {
    let (packed, project): (String, Option<String>) = {
        let conn = read(&pool)?;
        let packed = context::pack_thread(&conn, thread_id, context::DEFAULT_BUDGET_CHARS)?
            .ok_or_else(|| anyhow::anyhow!("thread not found"))?;
        let project = conn
            .query_row(
                "SELECT project_path FROM threads WHERE id = ?1",
                [thread_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .map_err(anyhow::Error::from)?;
        (packed, project)
    };
    // Prepend the project's distilled memory so the agent opens with what was already
    // decided + the gotchas to avoid, not just this one thread's transcript.
    let packed = match project.as_deref().filter(|p| !p.is_empty()) {
        Some(proj) => {
            let key = indexer::canonical_project(proj).unwrap_or_else(|| proj.to_string());
            let mem = read(&pool)
                .ok()
                .and_then(|c| knowledge::get_project_memory(&c, &key, 25).ok());
            match mem {
                Some(m)
                    if !(m.decisions.is_empty()
                        && m.gotchas.is_empty()
                        && m.open_todos.is_empty()) =>
                {
                    format!(
                        "<project_memory project=\"{proj}\">\n{}</project_memory>\n\n{packed}",
                        format_memory_notes(&m)
                    )
                }
                _ => packed,
            }
        }
        None => packed,
    };
    let program = program.unwrap_or_else(|| "claude".into());
    let path = agent::cli_resume::launch_with_context(&program, &packed, project.as_deref())?;
    Ok(path)
}

/// Relaunch the original CLI agent on an indexed thread (Claude Code / Codex).
#[tauri::command]
fn resume_thread(pool: tauri::State<'_, db::ReadPool>, thread_id: i64) -> AppResult<()> {
    let (source, external_id, is_subagent, project): (String, String, bool, Option<String>) = {
        let conn = read(&pool)?;
        conn.query_row(
            "SELECT s.kind, t.external_id, t.is_subagent, t.project_path
             FROM threads t JOIN sources s ON s.id = t.source_id WHERE t.id = ?1",
            [thread_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0, r.get(3)?)),
        )
        .map_err(anyhow::Error::from)?
    };
    agent::cli_resume::launch(&source, &external_id, is_subagent, project.as_deref())?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
// ---- Claude Code integration (one-click /recall skill + MCP server) ----

/// Whether the `/recall` skill + `callimachus` MCP server are installed for Claude Code.
#[tauri::command]
fn recall_integration_status() -> AppResult<integration::IntegrationStatus> {
    let exe = std::env::current_exe().map_err(anyhow::Error::from)?;
    Ok(integration::status(&exe))
}

/// Install (or refresh) the `/recall` skill and register this app as Claude Code's
/// `callimachus` MCP server — no terminal, cargo, or extra binary needed.
#[tauri::command]
fn install_recall_integration() -> AppResult<integration::IntegrationStatus> {
    let exe = std::env::current_exe().map_err(anyhow::Error::from)?;
    Ok(integration::install(&exe)?)
}

/// Remove the skill file and the MCP registration.
#[tauri::command]
fn uninstall_recall_integration() -> AppResult<()> {
    integration::uninstall()?;
    Ok(())
}

/// MCP-registration status for the other detected agents (Codex / Cursor / Gemini).
#[tauri::command]
fn agent_integrations_status() -> AppResult<Vec<integration::AgentIntegration>> {
    let exe = std::env::current_exe().map_err(anyhow::Error::from)?;
    Ok(integration::agent_status(&exe))
}

/// Register the `callimachus` MCP server with every detected non-Claude agent.
#[tauri::command]
fn install_agent_integrations() -> AppResult<Vec<integration::AgentIntegration>> {
    let exe = std::env::current_exe().map_err(anyhow::Error::from)?;
    Ok(integration::install_agents(&exe)?)
}

/// Remove the `callimachus` MCP registration from the other agents.
#[tauri::command]
fn uninstall_agent_integrations() -> AppResult<()> {
    integration::uninstall_agents()?;
    Ok(())
}

pub fn run() {
    let mut builder = tauri::Builder::default().plugin(tauri_plugin_opener::init());

    // Auto-update is desktop-only; the updater + process plugins back the
    // in-app "check for updates / restart to install" flow.
    #[cfg(desktop)]
    {
        builder = builder
            .plugin(tauri_plugin_updater::Builder::new().build())
            .plugin(tauri_plugin_process::init());
    }

    builder
        .setup(|app| {
            // Resolve the DB the SAME way as the indexer, read pool, watcher, and sidecars
            // (default_index_path: honors CALLIMACHUS_DB, else the app data dir) so every
            // component opens one consistent file. Pointing CALLIMACHUS_DB at a throwaway
            // path is how you exercise a clean first-run / onboarding without touching the
            // real index.
            let db_path = db::default_index_path();
            if let Some(parent) = db_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let conn = db::open(&db_path)?; // single WRITER; also runs migrations
                                            // Post-migration: fill canonical project keys for existing threads (fast).
            if let Err(e) = indexer::backfill_project_keys(&conn) {
                eprintln!("[index] project-key backfill: {e}");
            }
            app.manage(db::Db(Mutex::new(conn)));
            // Read pool (after the writer migrated): UI read commands run concurrently
            // here instead of serializing behind the writer mutex.
            let pool_size = std::thread::available_parallelism()
                .map(|n| n.get() as u32)
                .unwrap_or(4);
            app.manage(db::read_pool(&db_path, pool_size.clamp(2, 6))?);
            app.manage(embed::Embedder::default());
            app.manage(EmbedJob::default());
            app.manage(IndexJob::default());
            app.manage(DistillJob::default());
            app.manage(SetupState::default());
            app.manage(ChatGeneration::default());
            app.manage(PendingApprovals::default());
            // Background watcher keeps the index fresh as agents write new threads.
            indexer::watcher::spawn(app.handle().clone());
            // Drain any distilled facts that aren't embedded yet (e.g. distilled before
            // the recall index shipped) so cross-thread recall works. No-op if none.
            {
                let app = app.handle().clone();
                std::thread::spawn(move || {
                    let db = app.state::<db::Db>();
                    let embedder = app.state::<embed::Embedder>();
                    if let Err(e) = embed::embed_pending_facts(&db, &embedder) {
                        eprintln!("[knowledge] startup fact embed: {e}");
                    }
                    // Then catch up on any auto-distillation owed (no-op unless enabled).
                    auto_distill_kick(&app);
                });
            }
            // The blocking init (open + migrate + backfill + read pool) is done — the
            // backend is ready. The splash stays up until the frontend also signals ready.
            complete_setup(app.handle(), "backend");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            db_stats,
            index_stats,
            cleanup_candidates,
            delete_threads,
            vacuum_db,
            index_all,
            indexing_status,
            index_source,
            search_threads,
            recent_threads,
            get_thread,
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
            thread_context,
            open_thread_in_cli,
            export_thread,
            obsidian_vaults,
            can_synthesize,
            synthesize_export,
            recall_integration_status,
            install_recall_integration,
            uninstall_recall_integration,
            agent_integrations_status,
            install_agent_integrations,
            uninstall_agent_integrations,
            set_star,
            set_thread_tags,
            list_tags,
            list_open_todos,
            knowledge_config,
            set_complete,
            coach_overview,
            set_knowledge_config,
            set_auto_distill,
            thread_knowledge,
            thread_commits,
            link_thread_commits,
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
            detect_conflicts
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
