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
pub mod integration;
pub mod knowledge;
pub mod mcp_server;
pub mod search;

use error::AppResult;
use search::{SearchFilters, SearchHit, ThreadDetail, ThreadSummary};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};

/// Guards against launching more than one background embedding job at a time.
#[derive(Default)]
struct EmbedJob(AtomicBool);

/// Cancellation token for the in-flight chat stream (one generation at a time).
#[derive(Default)]
struct ChatGeneration(Mutex<Option<tokio_util::sync::CancellationToken>>);

/// Pending shell-command approvals, keyed by tool call id. `approve_tool` resolves
/// the matching one-shot, unblocking the awaiting tool execution.
#[derive(Default)]
struct PendingApprovals(Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<bool>>>);

/// Execute one tool call requested by the in-app agent. Read-only index tools run
/// immediately; `run_shell` emits an approval request and waits for the user.
async fn run_tool(
    app: AppHandle,
    ch: tauri::ipc::Channel<agent::ChatChunk>,
    call: genai::chat::ToolCall,
) -> anyhow::Result<String> {
    let name = call.fn_name.clone();
    let args = call.fn_arguments.clone();
    let arg_str = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or_default().to_string();

    // Announce the call.
    let announce = match name.as_str() {
        "search_history" => format!("search: {}", arg_str("query")),
        "get_thread" => format!("thread #{}", args.get("thread_id").and_then(|v| v.as_i64()).unwrap_or(0)),
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
                let db = app.state::<db::Db>();
                let conn = db.0.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
                let hits = search::search(
                    &conn,
                    &query,
                    &SearchFilters { limit: Some(limit), ..Default::default() },
                )?;
                serde_json::to_string(&hits)?
            };
            let _ = ch.send(result(format!("{} results", json.matches("\"threadId\"").count())));
            Ok(json)
        }
        "get_thread" => {
            let tid = args.get("thread_id").and_then(|v| v.as_i64()).unwrap_or(0);
            let packed = {
                let db = app.state::<db::Db>();
                let conn = db.0.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
                context::pack_thread(&conn, tid, context::DEFAULT_BUDGET_CHARS)?
                    .unwrap_or_else(|| "thread not found".to_string())
            };
            let _ = ch.send(result(format!("loaded thread #{tid} ({} chars)", packed.len())));
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
                out = format!("{}\n…(truncated)", out.chars().take(12_000).collect::<String>());
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

#[tauri::command]
fn db_stats(db: tauri::State<'_, db::Db>) -> AppResult<DbStats> {
    let conn = lock(&db)?;
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
fn index_stats(db: tauri::State<'_, db::Db>) -> AppResult<search::Stats> {
    let conn = lock(&db)?;
    Ok(search::stats(&conn)?)
}

/// Oldest-first list of threads with their size, for the storage-cleanup UI.
#[tauri::command]
fn cleanup_candidates(
    db: tauri::State<'_, db::Db>,
    before: Option<i64>,
    sources: Option<Vec<String>>,
    limit: Option<i64>,
) -> AppResult<Vec<cleanup::CleanupRow>> {
    let conn = lock(&db)?;
    Ok(cleanup::candidates(&conn, before, &sources.unwrap_or_default(), limit.unwrap_or(200))?)
}

/// Permanently delete the given threads (cascades to messages, FTS, vectors).
#[tauri::command]
fn delete_threads(db: tauri::State<'_, db::Db>, ids: Vec<i64>) -> AppResult<usize> {
    let mut conn = lock(&db)?;
    Ok(cleanup::delete_threads(&mut conn, &ids)?)
}

/// Reclaim disk space freed by deletes (VACUUM).
#[tauri::command]
fn vacuum_db(db: tauri::State<'_, db::Db>) -> AppResult<()> {
    let conn = lock(&db)?;
    cleanup::vacuum(&conn)?;
    Ok(())
}

/// Index (or re-index changed files from) every source.
#[tauri::command]
fn index_all(db: tauri::State<'_, db::Db>) -> AppResult<indexer::IndexReport> {
    let mut conn = lock(&db)?;
    Ok(indexer::scan_all(&mut conn)?)
}

/// Index a single source by kind ("claude_code" | "codex" | "cursor").
#[tauri::command]
fn index_source(db: tauri::State<'_, db::Db>, kind: String) -> AppResult<indexer::IndexReport> {
    let mut conn = lock(&db)?;
    let report = match kind.as_str() {
        "claude_code" => indexer::claude::scan(&mut conn)?,
        "codex" => indexer::codex::scan(&mut conn)?,
        "cursor" => indexer::cursor::scan(&mut conn)?,
        "gemini" => indexer::gemini::scan(&mut conn)?,
        "qwen" => indexer::qwen::scan(&mut conn)?,
        "goose" => indexer::goose::scan(&mut conn)?,
        "opencode" => indexer::opencode::scan(&mut conn)?,
        "continue" => indexer::continue_cli::scan(&mut conn)?,
        "cline" => indexer::cline::scan(&mut conn)?,
        "roo" => indexer::roo::scan(&mut conn)?,
        "kilo" => indexer::kilo::scan(&mut conn)?,
        other => return Err(anyhow::anyhow!("unknown source kind: {other}").into()),
    };
    Ok(report)
}

#[tauri::command]
fn search_threads(
    db: tauri::State<'_, db::Db>,
    embedder: tauri::State<'_, embed::Embedder>,
    query: String,
    filters: Option<SearchFilters>,
) -> AppResult<Vec<SearchHit>> {
    let filters = filters.unwrap_or_default();
    let hits = if filters.hybrid {
        // Embed the query BEFORE locking the DB so the (multi-second on first call)
        // inference never holds the single connection and freezes other UI commands —
        // especially while a background embedding build is running.
        let qv = embed::embed_query(&embedder, &query)?;
        let conn = lock(&db)?;
        search::hybrid_vec(&conn, &query, qv.as_deref(), &filters)?
    } else {
        let conn = lock(&db)?;
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

#[tauri::command]
fn embedding_status(
    db: tauri::State<'_, db::Db>,
    job: tauri::State<'_, EmbedJob>,
) -> AppResult<EmbedStatus> {
    let conn = lock(&db)?;
    let (done, total) = embed::embed_progress(&conn)?;
    Ok(EmbedStatus {
        done,
        total,
        running: job.0.load(Ordering::Relaxed),
    })
}

/// Kick off (or no-op if already running) a background job that embeds all pending
/// messages batch-by-batch, releasing the DB lock between batches.
#[tauri::command]
fn build_embeddings(app: AppHandle) -> AppResult<()> {
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
                    eprintln!("[embed] {e}");
                    break;
                }
            };
            // 3. Locked, fast: persist the vectors + mark the messages embedded.
            let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
            {
                let Ok(mut conn) = db.0.lock() else { break };
                if let Err(e) = embed::store_batch(&mut conn, &ids, &owners, &vecs) {
                    eprintln!("[embed] {e}");
                    break;
                }
            }
            done += rows.len() as i64;
            let _ = app.emit("embed:progress", EmbedProgressEvent { done, total });
        }
        app.state::<EmbedJob>().0.store(false, Ordering::SeqCst);
        let _ = app.emit("embed:done", ());
    });
    Ok(())
}

#[tauri::command]
fn recent_threads(
    db: tauri::State<'_, db::Db>,
    filters: Option<SearchFilters>,
) -> AppResult<Vec<ThreadSummary>> {
    let conn = lock(&db)?;
    Ok(search::recent_threads(&conn, &filters.unwrap_or_default())?)
}

#[tauri::command]
fn get_thread(db: tauri::State<'_, db::Db>, thread_id: i64) -> AppResult<Option<ThreadDetail>> {
    let conn = lock(&db)?;
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
fn list_tags(db: tauri::State<'_, db::Db>) -> AppResult<Vec<(String, i64)>> {
    let conn = lock(&db)?;
    Ok(search::list_tags(&conn)?)
}

/// Open TODOs across the corpus (free heuristic knowledge tier), newest first,
/// optionally scoped to a project-path substring and/or a source kind.
#[tauri::command]
fn list_open_todos(
    db: tauri::State<'_, db::Db>,
    project: Option<String>,
    source: Option<String>,
) -> AppResult<Vec<knowledge::TodoFact>> {
    let conn = lock(&db)?;
    Ok(knowledge::list_open_todos(&conn, project.as_deref(), source.as_deref(), 500)?)
}

/// Current distillation engine config (enabled + provider/model).
#[tauri::command]
fn knowledge_config(db: tauri::State<'_, db::Db>) -> AppResult<knowledge::KnowledgeConfig> {
    let conn = lock(&db)?;
    Ok(knowledge::get_config(&conn)?)
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
    db: tauri::State<'_, db::Db>,
    thread_id: i64,
) -> AppResult<knowledge::ThreadKnowledge> {
    let conn = lock(&db)?;
    Ok(knowledge::get_thread_knowledge(&conn, thread_id)?)
}

/// Distill one thread now (decisions/gotchas/summary) using the configured engine, and
/// return the fresh knowledge. The LLM call runs WITHOUT the DB lock held.
#[tauri::command]
async fn distill_thread(
    db: tauri::State<'_, db::Db>,
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
            let mut conn = lock(&db)?;
            let now = chrono::Utc::now().timestamp();
            knowledge::store_distilled(&mut conn, thread_id, &distilled, now)?;
            Ok(knowledge::get_thread_knowledge(&conn, thread_id)?)
        }
        Err(e) => {
            let conn = lock(&db)?;
            // Store a short, sanitized summary — not the raw provider error, which can
            // echo HTTP status / URLs / response bodies.
            let msg: String =
                e.to_string().lines().next().unwrap_or("distillation failed").chars().take(160).collect();
            knowledge::set_error(&conn, thread_id, &msg)?;
            Err(e.into())
        }
    }
}

// ---- agent chat ----

/// Stream a chat completion. Tokens are pushed over `on_token`; the full reply is
/// returned and the conversation persisted as a searchable in_app thread.
#[tauri::command]
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
fn thread_context(db: tauri::State<'_, db::Db>, thread_id: i64) -> AppResult<String> {
    let conn = lock(&db)?;
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
        let Ok(text) = std::fs::read_to_string(&cfg) else { continue };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
        let Some(vaults) = json.get("vaults").and_then(|v| v.as_object()) else { continue };
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
fn write_note(detail: &ThreadDetail, synthesis: Option<&str>, vault_dir: &str) -> AppResult<String> {
    let md = export::to_obsidian(detail, synthesis);
    std::fs::create_dir_all(vault_dir).map_err(anyhow::Error::from)?;
    let path = std::path::Path::new(vault_dir).join(format!("{}.md", export::note_filename(detail)));
    std::fs::write(&path, md).map_err(anyhow::Error::from)?;
    Ok(path.to_string_lossy().into_owned())
}

/// Quick export: a thread → Obsidian note (transcript + `[[project]]` link), no LLM.
#[tauri::command]
fn export_thread(
    db: tauri::State<'_, db::Db>,
    thread_id: i64,
    vault_dir: String,
) -> AppResult<String> {
    let detail = {
        let conn = lock(&db)?;
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
    SYNTH_MODELS.iter().copied().find(|(p, _)| secrets::has_key(p))
}

fn synth_default_model(provider: &str) -> Option<&'static str> {
    SYNTH_MODELS.iter().find(|(p, _)| *p == provider).map(|(_, m)| *m)
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
            let (p, m) = pick_synth_provider()
                .ok_or_else(|| anyhow::anyhow!("no API key set — add one in Settings to synthesize"))?;
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
    let key = if provider == "ollama" { None } else { secrets::get_key(&provider)? };
    Ok((provider, model, key))
}

/// Synthesized export: pack the thread, run the chosen (or first available) LLM to
/// extract a summary + decisions / gotchas / TODOs, and write a knowledge-grade
/// Obsidian note (synthesis above the transcript) into `vault_dir`.
#[tauri::command]
async fn synthesize_export(
    db: tauri::State<'_, db::Db>,
    thread_id: i64,
    vault_dir: String,
    provider: Option<String>,
    model: Option<String>,
) -> AppResult<String> {
    let (provider, model) = resolve_synth(provider.as_deref(), model.as_deref())?;
    // Pull detail + packed transcript, then drop the DB lock before the network call.
    let (detail, packed) = {
        let conn = lock(&db)?;
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
    db: tauri::State<'_, db::Db>,
    thread_id: i64,
    program: Option<String>,
) -> AppResult<String> {
    let (packed, project): (String, Option<String>) = {
        let conn = lock(&db)?;
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
    let program = program.unwrap_or_else(|| "claude".into());
    let path = agent::cli_resume::launch_with_context(&program, &packed, project.as_deref())?;
    Ok(path)
}

/// Relaunch the original CLI agent on an indexed thread (Claude Code / Codex).
#[tauri::command]
fn resume_thread(db: tauri::State<'_, db::Db>, thread_id: i64) -> AppResult<()> {
    let (source, external_id, is_subagent, project): (String, String, bool, Option<String>) = {
        let conn = lock(&db)?;
        conn.query_row(
            "SELECT s.kind, t.external_id, t.is_subagent, t.project_path
             FROM threads t JOIN sources s ON s.id = t.source_id WHERE t.id = ?1",
            [thread_id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get::<_, i64>(2)? != 0,
                    r.get(3)?,
                ))
            },
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
            let dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&dir)?;
            let conn = db::open(&dir.join("index.db"))?;
            app.manage(db::Db(Mutex::new(conn)));
            app.manage(embed::Embedder::default());
            app.manage(EmbedJob::default());
            app.manage(ChatGeneration::default());
            app.manage(PendingApprovals::default());
            // Background watcher keeps the index fresh as agents write new threads.
            indexer::watcher::spawn(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            db_stats,
            index_stats,
            cleanup_candidates,
            delete_threads,
            vacuum_db,
            index_all,
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
            set_star,
            set_thread_tags,
            list_tags,
            list_open_todos,
            knowledge_config,
            set_knowledge_config,
            thread_knowledge,
            distill_thread
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
