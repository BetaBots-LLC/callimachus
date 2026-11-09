//! The Tauri-free core of Callimachus: the index (db, indexer, search, embed), project memory
//! (knowledge, export, context), the agent integrations, the `cal` CLI, the MCP server and the
//! background jobs. The desktop app (`callimachus_lib`) re-exports these modules, and the
//! standalone `cal` and `callimachus-mcp` binaries link this crate alone, so neither needs
//! GTK or WebKit to build.
//!
//! # Identity and the graph tables (migration 0034)
//!
//! What the Batch H phase 2 features (repo memory, `cal why`, cost per commit, failure
//! playbooks) build on. Shapes, as created by `migrations/0034_identity_and_graph.sql`:
//!
//! - `facts.uid TEXT`: a random v4 UUID, unique (`idx_facts_uid`, full, not partial) and
//!   non-null and non-empty for every row after the migration (a backfill; the `facts_uid_ai`
//!   trigger fills any insert that names none or ''; `facts_uid_bu` aborts an update that
//!   would clear it). Insert paths pass [`knowledge::new_fact_uid`]. The merge key across
//!   machines and files. Re-derived heuristic TODOs, re-distilled facts with the same kind and
//!   text, re-imported Claude Code memory and compaction summaries keep theirs. An importer
//!   that already has a uid never plain-INSERTs it unchecked: a uid already present (one this
//!   machine published, coming back from the repo) fails the unique index and aborts the
//!   import. Repo memory ([`knowledge::repo_memory::import_repo`]) looks each uid up inside
//!   one IMMEDIATE transaction: an unknown uid is inserted with `extractor = 'repo'`; a uid
//!   this machine wrote keeps its local copy; an imported one follows the file unless the user
//!   edited or hid it; a uid another project already imported is skipped.
//!   [`knowledge::IMPORT_FACT_SQL`] is the plain upsert (`ON CONFLICT(uid) DO UPDATE ...
//!   WHERE facts.edited = 0 AND facts.hidden = 0`) for an importer that needs no per-project
//!   check.
//! - `facts.source_seq INTEGER`: the source message's `seq` in its thread (heuristic TODOs set
//!   it). After a full re-index replaces a thread's messages, the indexer calls
//!   [`knowledge::rebind_fact_sources`]`(conn, thread_id)`, which binds a fact to the message
//!   at that seq only while it still contains the fact's text, else to the nearest message
//!   that does (updating `source_seq`), else leaves `source_message_id` NULL.
//! - `projects (key TEXT PRIMARY KEY, remote TEXT, subpath TEXT, portable_id TEXT,
//!   refreshed_at INTEGER)`, index on `portable_id`. [`knowledge::portable_project_id`]`(conn,
//!   key) -> Option<String>` computes and caches `host/owner/repo[#subpath]` from `git remote
//!   get-url origin` (24 h TTL; [`knowledge::refresh_portable_project_id`] skips the cache; a
//!   failed `git` probe is never cached); None means no remote, so fall back to the local
//!   key. On github.com, gitlab.com and bitbucket.org the path is lowercased too.
//!   [`knowledge::local_keys_for_portable`]`(conn, id) -> Vec<String>` maps back to the local
//!   keys resolved so far (the watcher resolves every checkout it links). `threads.project_key`
//!   stays the canonical absolute path.
//! - `file_edits (thread_id, message_id, path, op, ts)`, `op` one of `read | edit | write |
//!   mention`, indexes `(path, ts)` and `(thread_id)`. One row per tool call and path (see
//!   [`indexer::file_edits`]); `path` is relative to the checkout the call ran in when inside
//!   it (`src/a.rs`, `../` resolved), else as written. At most 2000 rows per thread, edits and
//!   writes kept over reads. `message_id` carries no foreign key (rows are rewritten with the
//!   thread's messages; a thread delete cascades) and is NULL for a Codex `apply_patch` the
//!   backfill read from a rollout file. Prose mentions stay in `file_mentions`; sources
//!   without tool calls add no rows. Threads indexed before 0034 are filled once, in the
//!   background, by [`indexer::file_edits::backfill_file_edits_once`] (flagged by the
//!   migration in `app_config` `file_edits.backfill_pending`; the watcher and `cal index` run
//!   it), never by resetting cursors. A Claude Code subagent transcript keeps its rows under
//!   its own thread (`is_subagent = 1`): readers that join commits or sessions to files
//!   expand a session with [`indexer::session_thread_ids`] (and map back with
//!   [`indexer::parent_session_id`]).
//! - `issue_clusters (signature TEXT PRIMARY KEY, example, count, threads, first_seen,
//!   last_seen, last_thread_id, resolved_thread_id, resolved_sha, refreshed_at)`, keyed by
//!   [`issues::normalize`] of the example line. [`issues::refresh_clusters`]`(conn)` rebuilds
//!   the top 500 over 180 days (the watcher's minute tick, at most every 5 minutes after new
//!   messages, and at least daily), upserting the signatures it found and keeping their
//!   `resolved_*`. `recurring_issues` serves an unscoped 180-day call (`cal issues`, MCP with
//!   `project: "*"`) from it while the refresh is under 10 minutes old
//!   ([`issues::CLUSTERS_FRESH_SECS`]), or up to a day old when nothing was indexed since
//!   ([`issues::CLUSTERS_IDLE_SECS`]); a scoped call mines live. Readers of `issue_clusters`
//!   filter on `refreshed_at = app_config issues.clusters_refreshed_at`. Rows the latest
//!   refresh did not see are kept with their `resolved_*` columns: an unresolved one is pruned
//!   once its `last_seen` falls out of the 180-day window, a resolved one is kept
//!   indefinitely (`PRUNE_CLUSTERS_SQL`). `resolved_thread_id` / `resolved_sha` hold the fix
//!   an unscoped read shows; each project's own fix lives in `app_config`
//!   `issues.cluster_fixes` (JSON, signature then project key) and the stored clusters'
//!   thread ids in `issues.cluster_threads`, because phase 2 adds no migration. The refresh is split into [`issues::clusters_due`],
//!   [`issues::mine_for_refresh`] (read-only connection) and [`issues::store_clusters`] (the
//!   writer). The commit linker and the cluster refresh also run from `cal index`, not only
//!   from the watcher.
//! - `thread_commits` stays current without a Today load: after each index batch the watcher
//!   links the batch's threads in a git checkout ([`jobs::CommitLinker`], at most once a minute
//!   per checkout, 2 s budget per pass).

// First, so `app_log!` is in scope for every module declared after it.
#[macro_use]
pub mod applog;
pub mod jobclock;
pub mod memtrim;

pub mod agent;
pub mod error;
pub mod indexer;
pub mod secrets;

pub mod cleanup;
// The versioned `cal --json` shapes and their JSON Schemas (`schemas/cal/`).
pub mod cli_contract;
pub mod cli_core;
pub mod context;
pub mod cost;
pub mod db;
pub mod embed;
pub mod export;
pub mod gitlink;
// `cal index` / `watch` / `embed` / `distill --auto`: the jobs without the app.
pub mod headless;
pub mod integration;
pub mod issues;
pub mod jobs;
pub mod knowledge;
// Local-only counts of what the hooks and MCP server did; never telemetry (module docs).
pub mod ledger;
// gui.lock / watch.lock: which process keeps the index fresh.
pub mod locks;
pub mod mcp_server;
// The first-run checklist's saved answers; GUI only.
pub mod onboarding;
// "Your agents, so far": `cal report` and the app's Report view.
pub mod report;
#[cfg(test)]
pub(crate) mod schema_check;
pub mod search;
pub mod snapshot;
pub mod today;
// Agent Trace (agent-trace.dev) records: read by `cal why`, written by `cal trace export`.
pub mod trace;
// `cal why`: git blame to commits to the sessions behind them, and their decisions.
pub mod why;

use error::AppResult;
use search::SearchFilters;
use serde::Serialize;

/// A thread cited as a source in an "ask your history" answer.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct AskSource {
    #[serde(rename = "threadId")]
    pub thread_id: i64,
    pub title: Option<String>,
    pub source: String,
    #[serde(rename = "projectPath")]
    pub project_path: Option<String>,
    /// The first matching message, so a citation chip can open the thread there.
    #[serde(rename = "messageId")]
    pub message_id: Option<i64>,
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
            message_id: focus.get(&h.thread_id).and_then(|m| m.first().copied()),
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

/// Synthesis model per provider, the cheap/fast tier (this is summarization, not
/// hard reasoning). Default when a provider is pinned without a model, and the
/// auto-pick order for keyed providers.
pub const SYNTH_MODELS: &[(&str, &str)] = &[
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

pub fn synth_default_model(provider: &str) -> Option<&'static str> {
    SYNTH_MODELS
        .iter()
        .find(|(p, _)| *p == provider)
        .map(|(_, m)| *m)
}

/// Keyless engines need no stored API key: local Ollama, and the CLI backends (which use the
/// user's logged-in `claude` / `codex` subscription).
pub fn is_keyless(provider: &str) -> bool {
    provider == "ollama" || agent::is_cli_provider(provider)
}

/// Resolve (provider, model) from an optional pinned choice, else auto-pick. A pinned KEYED
/// provider must have a stored key; keyless engines (Ollama, CLIs) don't. A CLI may run with an
/// empty model (the CLI picks its own default).
pub fn resolve_synth(provider: Option<&str>, model: Option<&str>) -> AppResult<(String, String)> {
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
                    "the engine is the first saved API key, and none is saved: add one in Settings > AI engine or pick an engine"
                )
            })?;
            Ok((p.to_string(), m.to_string()))
        }
    }
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
            "LLM distillation is off: turn on memory in Settings > AI engine (a CLI subscription, local Ollama or an API key)"
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
