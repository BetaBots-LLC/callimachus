//! The Tauri-free core of Callimachus: the index (db, indexer, search, embed), project memory
//! (knowledge, export, context), the agent integrations, the `cal` CLI, the MCP server and the
//! background jobs. The desktop app (`callimachus_lib`) re-exports these modules, and the
//! standalone `cal` and `callimachus-mcp` binaries link this crate alone, so neither needs
//! GTK or WebKit to build.

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
pub mod search;
pub mod snapshot;
pub mod today;

use error::AppResult;
use search::SearchFilters;
use serde::Serialize;

/// A thread cited as a source in an "ask your history" answer.
#[derive(Debug, Clone, Serialize)]
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
