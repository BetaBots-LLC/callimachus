//! Provider-agnostic LLM chat with token streaming. Talks directly to provider
//! HTTP APIs (Anthropic, OpenAI, Ollama) over reqwest — no SDK lock-in. Tokens are
//! delivered to the caller via a callback (wired to a Tauri Channel); the finished
//! conversation is persisted as an `in_app` thread so it is searchable like any other.

pub mod cli_resume;

use crate::indexer::{self, ParsedMessage, ParsedThread};
use anyhow::{bail, Result};
use futures_util::StreamExt;
use genai::adapter::AdapterKind;
use genai::chat::{
    ChatMessage as GMessage, ChatOptions, ChatRequest, ChatStreamEvent, Tool, ToolCall,
    ToolResponse,
};
use genai::resolver::AuthData;
use genai::Client;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// One chat turn from the frontend.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatMessage {
    pub role: String, // system | user | assistant
    pub content: String,
}

/// Which stream a chunk belongs to: the model's reasoning, or the answer text.
#[derive(Debug, Clone, Copy)]
pub enum ChunkKind {
    Reasoning,
    Text,
}

/// A streamed chunk sent to the frontend over the Tauri channel. `kind` is one of
/// "reasoning" | "text" | "tool_call" | "tool_request" | "tool_result". Tool chunks
/// carry the tool name and (for shell approval) the call id.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatChunk {
    pub kind: &'static str,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

impl ChatChunk {
    pub fn text(kind: &'static str, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
            tool_id: None,
            tool_name: None,
        }
    }
}

/// The tools the in-app agent can call. Read-only ones run automatically;
/// `run_shell` is gated behind explicit user approval in the caller.
pub fn default_tools() -> Vec<Tool> {
    vec![
        Tool::new("search_history")
            .with_description(
                "Search the user's indexed AI coding-agent conversation history (Claude Code, \
                 Codex, Cursor, and past in-app chats). Returns matching threads with snippets \
                 and a threadId.",
            )
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search query" },
                    "limit": { "type": "integer", "description": "Max results (default 10)" }
                },
                "required": ["query"]
            })),
        Tool::new("get_thread")
            .with_description(
                "Fetch one indexed thread as a packed markdown transcript by threadId.",
            )
            .with_schema(json!({
                "type": "object",
                "properties": { "thread_id": { "type": "integer" } },
                "required": ["thread_id"]
            })),
        Tool::new("run_shell")
            .with_description(
                "Run a shell command on the user's machine and return its output. Requires the \
                 user to approve each command before it executes.",
            )
            .with_schema(json!({
                "type": "object",
                "properties": { "command": { "type": "string" } },
                "required": ["command"]
            })),
    ]
}

fn adapter_for(provider: &str) -> Result<AdapterKind> {
    Ok(match provider {
        "anthropic" => AdapterKind::Anthropic,
        "openai" => AdapterKind::OpenAI,
        "openrouter" => AdapterKind::OpenRouter,
        "gemini" => AdapterKind::Gemini,
        "ollama" => AdapterKind::Ollama,
        other => bail!("unknown provider: {other}"),
    })
}

/// A non-interactive CLI LLM backend: runs the user's logged-in agent CLI (Claude Code, Codex)
/// in print mode and reads its answer from stdout. Lets users distill/ask with their existing
/// CLI subscription instead of a raw API key. Tools/agentic actions are disabled and the call
/// runs in a neutral cwd, so it's a plain text completion, not a coding session.
struct CliBackend {
    bin: &'static str,
    /// Fixed args (print mode, text output, tools off).
    base_args: Vec<&'static str>,
    /// Flag used to pin a model; appended with the model when one is set.
    model_flag: &'static str,
}

/// Map a CLI provider id to its invocation. `None` for the keyed genai providers.
fn cli_backend_for(provider: &str) -> Option<CliBackend> {
    match provider {
        "claude-cli" => Some(CliBackend {
            bin: "claude",
            base_args: vec!["-p", "--output-format", "text", "--allowedTools", ""],
            model_flag: "--model",
        }),
        "codex-cli" => Some(CliBackend {
            bin: "codex",
            base_args: vec!["exec", "--skip-git-repo-check"],
            model_flag: "--model",
        }),
        _ => None,
    }
}

/// True when `provider` is a CLI backend (keyless: it uses the CLI's own logged-in auth).
pub fn is_cli_provider(provider: &str) -> bool {
    cli_backend_for(provider).is_some()
}

/// A selectable CLI LLM backend and whether its binary is reachable.
#[derive(Debug, Serialize)]
pub struct CliEngine {
    /// Provider id used as the engine (e.g. `claude-cli`).
    pub id: &'static str,
    pub label: &'static str,
    pub bin: &'static str,
    /// The binary resolves on the user's PATH (via their login shell).
    pub installed: bool,
}

/// The supported CLI backends, each marked installed/not based on a login-shell PATH lookup.
pub async fn cli_engines() -> Vec<CliEngine> {
    let mut out = Vec::new();
    for (id, label, bin) in [
        ("claude-cli", "Claude Code CLI", "claude"),
        ("codex-cli", "Codex CLI", "codex"),
    ] {
        out.push(CliEngine {
            id,
            label,
            bin,
            installed: resolve_cli_bin(bin).await.is_some(),
        });
    }
    out
}

/// Suggested model names for a CLI backend (free-text still works). Empty = let the CLI pick its
/// own default model.
pub fn cli_models(provider: &str) -> Vec<String> {
    match provider {
        "claude-cli" => vec!["sonnet".into(), "opus".into(), "haiku".into()],
        _ => Vec::new(),
    }
}

/// Resolve a CLI's absolute path. A GUI app launched from Finder/Dock does NOT inherit the user's
/// shell PATH (nvm / homebrew / npm-global dirs), so we ask the login shell to resolve it, then
/// fall back to scanning whatever PATH we do have. Returns `None` if the binary isn't found.
async fn resolve_cli_bin(bin: &str) -> Option<std::path::PathBuf> {
    if let Some(shell) = std::env::var_os("SHELL") {
        if let Ok(out) = tokio::process::Command::new(&shell)
            .arg("-lc")
            .arg(format!("command -v {bin}"))
            .output()
            .await
        {
            if out.status.success() {
                let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let p = std::path::PathBuf::from(&line);
                if !line.is_empty() && p.exists() {
                    return Some(p);
                }
            }
        }
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(bin))
            .find(|p| p.is_file())
    })
}

/// Run a CLI backend once: spawn the binary, pipe `prompt` to stdin, return trimmed stdout.
/// Runs in a neutral temp cwd so the CLI never acts on the user's repos.
async fn cli_complete(provider: &str, model: &str, prompt: &str) -> Result<String> {
    use tokio::io::AsyncWriteExt;
    let backend = cli_backend_for(provider)
        .ok_or_else(|| anyhow::anyhow!("not a CLI provider: {provider}"))?;
    let bin = resolve_cli_bin(backend.bin).await.ok_or_else(|| {
        anyhow::anyhow!(
            "`{}` not found. Install it and make sure it's logged in, then pick it again.",
            backend.bin
        )
    })?;
    let mut cmd = tokio::process::Command::new(&bin);
    cmd.args(&backend.base_args);
    if !model.is_empty() {
        cmd.arg(backend.model_flag).arg(model);
    }
    let mut child = cmd
        .current_dir(std::env::temp_dir())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            anyhow::anyhow!(
                "could not launch `{}` ({e}). Is it installed and on PATH?",
                backend.bin
            )
        })?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(prompt.as_bytes()).await.ok();
        // Drop stdin to send EOF so the CLI stops waiting for more input.
    }
    let out = child
        .wait_with_output()
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    if !out.status.success() {
        let err: String = String::from_utf8_lossy(&out.stderr)
            .trim()
            .chars()
            .take(300)
            .collect();
        bail!("`{}` failed ({}): {}", backend.bin, out.status, err);
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        bail!("`{}` returned no output", backend.bin);
    }
    Ok(text)
}

/// Stream a CLI backend's reply token-by-token. The Claude CLI supports realtime streaming via
/// `--output-format stream-json --include-partial-messages`: it prints JSONL where each
/// `content_block_delta` carries a text (or thinking) token, which we forward through `on_token`
/// as it arrives. Other CLI backends have no token stream we parse, so they fall back to one
/// completion emitted at once. Returns the full assistant text; honors `cancel` (kills the child).
async fn cli_stream<F>(
    provider: &str,
    model: &str,
    prompt: &str,
    cancel: &tokio_util::sync::CancellationToken,
    mut on_token: F,
) -> Result<String>
where
    F: FnMut(ChunkKind, &str),
{
    // Only the Claude CLI exposes a token stream we can parse; others emit one chunk at the end.
    if provider != "claude-cli" {
        let text = tokio::select! {
            _ = cancel.cancelled() => return Ok(String::new()),
            r = cli_complete(provider, model, prompt) => r?,
        };
        on_token(ChunkKind::Text, &text);
        return Ok(text);
    }

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let bin = resolve_cli_bin("claude").await.ok_or_else(|| {
        anyhow::anyhow!(
            "`claude` not found. Install it and make sure it's logged in, then pick it again."
        )
    })?;
    let mut cmd = tokio::process::Command::new(&bin);
    // stream-json in print mode requires --verbose; --include-partial-messages adds token deltas.
    cmd.args([
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--allowedTools",
        "",
    ]);
    if !model.is_empty() {
        cmd.arg("--model").arg(model);
    }
    let mut child = cmd
        .current_dir(std::env::temp_dir())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            anyhow::anyhow!("could not launch `claude` ({e}). Is it installed and on PATH?")
        })?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(prompt.as_bytes()).await.ok();
        // Drop stdin to send EOF so the CLI stops waiting for more input.
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("claude: no stdout pipe"))?;
    let mut reader = BufReader::new(stdout).lines();
    let mut full = String::new();

    loop {
        let line = tokio::select! {
            _ = cancel.cancelled() => {
                let _ = child.start_kill();
                return Ok(full);
            }
            next = reader.next_line() => match next.map_err(|e| anyhow::anyhow!(e.to_string()))? {
                Some(l) => l,
                None => break,
            },
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue; // non-JSON noise — skip
        };
        if v.get("type").and_then(serde_json::Value::as_str) != Some("stream_event") {
            continue;
        }
        let Some(delta) = v
            .get("event")
            .filter(|e| {
                e.get("type").and_then(serde_json::Value::as_str) == Some("content_block_delta")
            })
            .and_then(|e| e.get("delta"))
        else {
            continue;
        };
        match delta.get("type").and_then(serde_json::Value::as_str) {
            Some("text_delta") => {
                if let Some(t) = delta.get("text").and_then(serde_json::Value::as_str) {
                    full.push_str(t);
                    on_token(ChunkKind::Text, t);
                }
            }
            Some("thinking_delta") => {
                if let Some(t) = delta.get("thinking").and_then(serde_json::Value::as_str) {
                    on_token(ChunkKind::Reasoning, t);
                }
            }
            _ => {}
        }
    }

    let status = child
        .wait()
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    if full.is_empty() {
        if status.success() {
            bail!("`claude` returned no output");
        }
        bail!("`claude` failed ({status}). Is it installed and logged in?");
    }
    Ok(full)
}

/// One non-streaming completion, routed to the user's CLI (keyless) or to genai (keyed). The CLI
/// path has no separate system slot, so `system` and `user` are flattened into one prompt.
async fn complete(
    provider: &str,
    model: &str,
    api_key: Option<&str>,
    system: &str,
    user: &str,
    temperature: f64,
    max_tokens: u32,
) -> Result<String> {
    if is_cli_provider(provider) {
        return cli_complete(provider, model, &format!("{system}\n\n{user}")).await;
    }
    let adapter = adapter_for(provider)?;
    let key = api_key.map(str::to_string);
    let client = Client::builder()
        .with_adapter_kind(adapter)
        .with_auth_resolver_fn(move |_iden: genai::ModelIden| {
            Ok(key.clone().map(AuthData::from_single))
        })
        .build();
    let req = ChatRequest::new(Vec::new())
        .with_system(system)
        .append_message(GMessage::user(user.to_string()));
    let options = ChatOptions::default()
        .with_temperature(temperature)
        .with_max_tokens(max_tokens);
    let resp = client
        .exec_chat(model, req, Some(&options))
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    resp.into_first_text()
        .map(|t| t.trim().to_string())
        .ok_or_else(|| anyhow::anyhow!("completion returned no text"))
}

/// System prompt for one-shot thread synthesis (the Obsidian knowledge layer).
const SYNTH_SYSTEM: &str = "You are summarizing one AI coding-agent session for the developer's \
Obsidian knowledge base. Read the transcript and output concise Markdown with ONLY these \
sections, each a `##` heading, omitting any section that would be empty:\n\
## Summary — 2-3 sentences: what the session set out to do and the outcome.\n\
## Decisions — bullets: concrete technical decisions made, and why.\n\
## Gotchas — bullets: pitfalls, surprises, or non-obvious constraints discovered.\n\
## TODOs — bullets: unfinished follow-ups or next steps.\n\
## Files — bullets: notable files/paths created or changed.\n\
Be terse and specific. No top-level title, no frontmatter, do not repeat the transcript, and do \
not invent anything the transcript does not support.";

/// One-shot, non-streaming synthesis of a packed transcript into a Markdown
/// knowledge block for the Obsidian export. Same provider plumbing as
/// `chat_stream`, but a single completion (no streaming, no tools).
pub async fn synthesize(
    provider: &str,
    model: &str,
    api_key: Option<&str>,
    transcript: &str,
) -> Result<String> {
    let user = format!("Transcript:\n\n{transcript}");
    complete(provider, model, api_key, SYNTH_SYSTEM, &user, 0.2, 1500).await
}

/// System prompt for "ask your history" — RAG over the user's own past sessions.
const ANSWER_SYSTEM: &str = "You answer the user's question using ONLY the provided excerpts \
from their own past AI-coding sessions. Cite sources inline as [thread N], using the thread \
number shown next to each excerpt. Be concise, specific, and technical. If the excerpts do not \
contain the answer, say you couldn't find it in their history — never invent.";

/// Answer a question from packed excerpts of the user's history, with inline [thread N]
/// citations. Same provider plumbing as `synthesize`. The caller does retrieval + packing.
pub async fn answer(
    provider: &str,
    model: &str,
    api_key: Option<&str>,
    question: &str,
    context: &str,
) -> Result<String> {
    let user = format!("Question: {question}\n\nExcerpts from past sessions:\n\n{context}");
    complete(provider, model, api_key, ANSWER_SYSTEM, &user, 0.2, 900).await
}

/// System prompt for the project-memory brief — a tight orientation built from facts
/// already distilled across many sessions on ONE project.
const BRIEF_SYSTEM: &str = "You write a concise project-memory brief for a developer, from \
notes (decisions, gotchas, open TODOs) distilled across many past AI coding sessions on a \
SINGLE project. Output GitHub-flavored markdown: a 1-2 sentence orientation, then short \
sections — Key decisions, Watch out for (gotchas), and Open threads — as tight bullets. \
Merge duplicates, keep the most load-bearing points, stay specific and technical. Use ONLY \
the provided notes; never invent. If a section has nothing, omit it.";

/// Synthesize a project-memory brief from a project's aggregated facts. Same provider
/// plumbing as `synthesize`. The caller formats the facts into `notes`.
pub async fn project_brief(
    provider: &str,
    model: &str,
    api_key: Option<&str>,
    project: &str,
    notes: &str,
) -> Result<String> {
    let user = format!("Project: {project}\n\nDistilled notes:\n\n{notes}");
    complete(provider, model, api_key, BRIEF_SYSTEM, &user, 0.2, 900).await
}

/// System prompt for conflict review over a project's distilled decisions.
const CONFLICT_SYSTEM: &str = "You review a numbered list of technical decisions distilled \
from one project's history. Find pairs that CONFLICT, or where one SUPERSEDES the other (the \
project changed its mind). Output ONLY a JSON array; each element is \
{\"a\": <number>, \"b\": <number>, \"reason\": <one terse sentence>} where a and b are the \
1-based numbers of the two decisions. Include ONLY genuine conflicts or supersessions, not \
merely related or similar decisions. Output [] if there are none. No prose, no code fences.";

/// One conflicting / superseding decision pair (1-based indices into the input list).
#[derive(Debug, Deserialize)]
pub struct ConflictPair {
    pub a: usize,
    pub b: usize,
    #[serde(default)]
    pub reason: String,
}

/// Ask the LLM which of a project's decisions conflict or supersede each other. Returns
/// 1-based index pairs; the caller maps them back to fact ids. Same provider plumbing.
pub async fn find_conflicts(
    provider: &str,
    model: &str,
    api_key: Option<&str>,
    decisions: &[String],
) -> Result<Vec<ConflictPair>> {
    let list = decisions
        .iter()
        .enumerate()
        .map(|(i, d)| format!("{}. {}", i + 1, d.trim()))
        .collect::<Vec<_>>()
        .join("\n");
    let user = format!("Decisions:\n{list}");
    let text = complete(provider, model, api_key, CONFLICT_SYSTEM, &user, 0.1, 800).await?;
    Ok(parse_conflicts(&text))
}

/// Lenient parse of the model's JSON array of conflict pairs (tolerates fences/prose).
fn parse_conflicts(raw: &str) -> Vec<ConflictPair> {
    let trimmed = raw
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    if let (Some(s), Some(e)) = (trimmed.find('['), trimmed.rfind(']')) {
        if e > s {
            if let Ok(v) = serde_json::from_str::<Vec<ConflictPair>>(&trimmed[s..=e]) {
                return v;
            }
        }
    }
    Vec::new()
}

/// System prompt for STRUCTURED distillation into the knowledge `facts` table. We ask
/// for plain JSON and parse leniently (works identically across cloud and local Ollama
/// — no adapter-specific response-format API). TODOs are intentionally NOT requested:
/// those come from the free heuristic tier.
const DISTILL_SYSTEM: &str = "You distill one AI coding-agent session into structured \
knowledge for the developer. Output ONLY a single JSON object — no prose, no markdown \
code fences — of exactly this shape:\n\
{\"summary\": string, \"decisions\": string[], \"gotchas\": string[]}\n\
- summary: 1-2 sentences on what the session did and the outcome.\n\
- decisions: concrete technical decisions made, and why — one terse sentence each.\n\
- gotchas: pitfalls, surprises, or non-obvious constraints discovered — one each.\n\
Ground every item in the transcript; omit anything you cannot support; never invent. \
Use an empty string / empty array when a field has nothing.";

/// The distilled knowledge for one thread. Deserialized leniently from the model's
/// JSON; `summary` carries the raw completion as a fallback if parsing fails.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Distilled {
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub decisions: Vec<String>,
    #[serde(default)]
    pub gotchas: Vec<String>,
}

/// One-shot structured distillation of a packed transcript. Same provider plumbing as
/// `synthesize`; returns parsed facts (tolerant of fences / surrounding prose).
pub async fn distill(
    provider: &str,
    model: &str,
    api_key: Option<&str>,
    transcript: &str,
) -> Result<Distilled> {
    let user = format!("Transcript:\n\n{transcript}");
    let text = complete(provider, model, api_key, DISTILL_SYSTEM, &user, 0.1, 1200).await?;
    Ok(parse_distilled(&text))
}

/// Lenient JSON extraction: strip code fences, take the outer `{…}`, parse. On failure,
/// keep the raw completion as a single summary fact so a thread always yields something.
pub fn parse_distilled(raw: &str) -> Distilled {
    let trimmed = raw
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    if let (Some(s), Some(e)) = (trimmed.find('{'), trimmed.rfind('}')) {
        if e > s {
            if let Ok(d) = serde_json::from_str::<Distilled>(&trimmed[s..=e]) {
                return d;
            }
        }
    }
    let summary: String = trimmed.chars().take(2000).collect();
    Distilled {
        summary,
        decisions: Vec::new(),
        gotchas: Vec::new(),
    }
}

/// System prompt for the interactive in-app agent. Unlike the one-shot consts above, this
/// drives the streaming, tool-calling chat: it tells the model its real edge is the user's own
/// indexed coding history and when to reach for each tool. `chat_stream` injects it for both the
/// genai and CLI backends; it is NOT persisted into the saved transcript (it's seeded at send
/// time, not stored as a message), so saved chats stay clean.
const CHAT_SYSTEM: &str = "You are the assistant inside Callimachus, a local app that indexes the \
developer's own AI coding-agent history (Claude Code, Codex, Cursor, and past in-app chats) on \
their machine. Your advantage over a plain chatbot is that you can search and read that history \
with your tools — use them.\n\
- When the user asks about anything they may have done, decided, built, or hit before — prior \
work, past decisions, recurring errors, how something was solved — call search_history FIRST, \
then get_thread on the most relevant result (using its threadId) to read details before \
answering. Don't answer from memory when their own history could hold the answer.\n\
- Cite the threads you drew on inline as [thread N], using the threadId.\n\
- Use run_shell only when the user explicitly asks you to run or inspect something locally; keep \
commands minimal and read-only where possible. Each command needs the user's approval.\n\
- Be concise, specific, and technical. If a search finds nothing relevant, say so — never invent.";

/// Stream a chat completion via the genai crate (multi-provider, native protocols,
/// retries). The adapter is forced per `provider`; the API key (from the keychain)
/// is injected through an auth resolver so it never leaves Rust. `on_token` fires
/// for each text chunk; the full assistant text is returned.
#[allow(clippy::too_many_arguments)] // provider/model/key/messages/tools/cancel/callbacks — all distinct inputs
pub async fn chat_stream<F, E, Fut>(
    provider: &str,
    model: &str,
    _base_url: Option<&str>,
    api_key: Option<&str>,
    messages: &[ChatMessage],
    tools: Vec<Tool>,
    cancel: tokio_util::sync::CancellationToken,
    mut on_token: F,
    execute_tool: E,
) -> Result<String>
where
    F: FnMut(ChunkKind, &str),
    E: Fn(ToolCall) -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    // CLI backends have no genai tool-calling, so we flatten the conversation into one prompt and
    // stream the reply (the Claude CLI streams token-by-token; see `cli_stream`). The history-search
    // `tools` / `execute_tool` loop is genai-only, so it's intentionally skipped here.
    if is_cli_provider(provider) {
        let _ = (&tools, &execute_tool);
        let mut system = String::from(CHAT_SYSTEM);
        let mut convo = String::new();
        for m in messages {
            match m.role.as_str() {
                "system" => {
                    if !system.is_empty() {
                        system.push_str("\n\n");
                    }
                    system.push_str(&m.content);
                }
                "assistant" => {
                    convo.push_str("\n\nAssistant: ");
                    convo.push_str(&m.content);
                }
                _ => {
                    convo.push_str("\n\nUser: ");
                    convo.push_str(&m.content);
                }
            }
        }
        let prompt = format!("{system}\n{convo}\n\nAssistant:");
        return cli_stream(provider, model, &prompt, &cancel, on_token).await;
    }

    let adapter = adapter_for(provider)?;
    let key = api_key.map(str::to_string);
    let client = Client::builder()
        .with_adapter_kind(adapter)
        .with_auth_resolver_fn(move |_iden: genai::ModelIden| {
            Ok(key.clone().map(AuthData::from_single))
        })
        .build();

    // Split system messages out (genai takes system separately); map the rest.
    let mut system = String::from(CHAT_SYSTEM);
    let mut req = ChatRequest::new(Vec::new());
    for m in messages {
        match m.role.as_str() {
            "system" => {
                if !system.is_empty() {
                    system.push_str("\n\n");
                }
                system.push_str(&m.content);
            }
            "assistant" => req = req.append_message(GMessage::assistant(m.content.clone())),
            _ => req = req.append_message(GMessage::user(m.content.clone())),
        }
    }
    if !system.is_empty() {
        req = req.with_system(system);
    }
    if !tools.is_empty() {
        req = req.with_tools(tools);
    }

    // Capture content + tool calls so we can drive the multi-turn tool loop.
    let options = ChatOptions::default()
        .with_capture_content(true)
        .with_capture_tool_calls(true);

    let mut full = String::new();
    loop {
        let resp = client
            .exec_chat_stream(model, req.clone(), Some(&options))
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let mut stream = resp.stream;
        let mut end = None;
        // Coalesce text deltas: flush every ~25ms (or on reasoning/end/cancel) to
        // collapse thousands of tiny JSON+IPC sends. The frontend rAF-batches too.
        let mut buf = String::new();
        let mut last_flush = std::time::Instant::now();

        'turn: loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    if !buf.is_empty() { on_token(ChunkKind::Text, &buf); }
                    return Ok(full);
                }
                event = stream.next() => {
                    let Some(event) = event else { break 'turn };
                    match event.map_err(|e| anyhow::anyhow!(e.to_string()))? {
                        ChatStreamEvent::Chunk(chunk) => {
                            full.push_str(&chunk.content);
                            buf.push_str(&chunk.content);
                            // ~per-frame flush: small increments keep the print smooth
                            // while still collapsing many tiny deltas into one send.
                            if last_flush.elapsed() >= std::time::Duration::from_millis(12) {
                                on_token(ChunkKind::Text, &buf);
                                buf.clear();
                                last_flush = std::time::Instant::now();
                            }
                        }
                        ChatStreamEvent::ReasoningChunk(chunk) => {
                            if !buf.is_empty() {
                                on_token(ChunkKind::Text, &buf);
                                buf.clear();
                            }
                            on_token(ChunkKind::Reasoning, &chunk.content);
                        }
                        ChatStreamEvent::End(e) => {
                            if !buf.is_empty() {
                                on_token(ChunkKind::Text, &buf);
                                buf.clear();
                            }
                            end = Some(e);
                            break 'turn;
                        }
                        _ => {}
                    }
                }
            }
        }

        // No End (stream closed) or no tool calls -> the answer is complete.
        let Some(end) = end else { break };
        let calls: Vec<ToolCall> = end
            .captured_tool_calls()
            .map(|v| v.into_iter().cloned().collect())
            .unwrap_or_default();
        if calls.is_empty() {
            break;
        }

        // Append the assistant's tool-call turn, run each tool, append responses, loop.
        if let Some(content) = &end.captured_content {
            req = req.append_message(GMessage::assistant(content.clone()));
        }
        let mut responses = Vec::new();
        for call in calls {
            if cancel.is_cancelled() {
                return Ok(full);
            }
            let id = call.call_id.clone();
            let output = match execute_tool(call).await {
                Ok(out) => out,
                Err(e) => format!("tool error: {e}"),
            };
            responses.push(ToolResponse::new(id, output));
        }
        req = req.append_message(GMessage::from(responses));
    }
    Ok(full)
}

/// Persist (or replace) an in-app chat thread so it is searchable like any other.
pub fn persist_chat(
    conn: &mut Connection,
    thread_id: &str,
    messages: &[ChatMessage],
    assistant_reply: &str,
) -> Result<()> {
    let sid = indexer::source_id(conn, "in_app")?;
    let now = chrono::Utc::now().timestamp();

    let title = messages.iter().find(|m| m.role == "user").map(|m| {
        let t = m.content.trim();
        if t.chars().count() > 80 {
            format!("{}…", t.chars().take(80).collect::<String>())
        } else {
            t.to_string()
        }
    });

    let mut parsed: Vec<ParsedMessage> = messages
        .iter()
        .map(|m| ParsedMessage {
            role: m.role.clone(),
            text: m.content.clone(),
            tool_name: None,
            ts: Some(now),
        })
        .collect();
    parsed.push(ParsedMessage {
        role: "assistant".into(),
        text: assistant_reply.to_string(),
        tool_name: None,
        ts: Some(now),
    });

    let thread = ParsedThread {
        external_id: thread_id.to_string(),
        title,
        project_path: None,
        git_branch: None,
        created_at: Some(now),
        updated_at: Some(now),
        is_subagent: false,
        usage: Vec::new(),
        messages: parsed,
    };
    indexer::upsert_thread(conn, sid, &thread)?;
    Ok(())
}

/// Fetch the list of model ids the provider currently offers, from its public
/// models API. Free-text entry still works in the UI; this just provides the real,
/// current options. Anthropic/OpenAI need a key; OpenRouter/Ollama do not.
pub async fn list_models(
    provider: &str,
    base_url: Option<&str>,
    api_key: Option<&str>,
) -> Result<Vec<String>> {
    use serde_json::Value;
    let client = reqwest::Client::new();

    let json: Value = match provider {
        "anthropic" => {
            let key = api_key.ok_or_else(|| anyhow::anyhow!("missing Anthropic API key"))?;
            client
                .get("https://api.anthropic.com/v1/models?limit=1000")
                .header("x-api-key", key)
                .header("anthropic-version", "2023-06-01")
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?
        }
        "openai" => {
            let key = api_key.ok_or_else(|| anyhow::anyhow!("missing OpenAI API key"))?;
            client
                .get("https://api.openai.com/v1/models")
                .bearer_auth(key)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?
        }
        "openrouter" => {
            // Public listing; key optional.
            let mut req = client.get("https://openrouter.ai/api/v1/models");
            if let Some(key) = api_key {
                req = req.bearer_auth(key);
            }
            req.send().await?.error_for_status()?.json().await?
        }
        "gemini" => {
            let key = api_key.ok_or_else(|| anyhow::anyhow!("missing Gemini API key"))?;
            client
                .get(format!(
                    "https://generativelanguage.googleapis.com/v1beta/models?key={key}&pageSize=1000"
                ))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?
        }
        "ollama" => {
            let base = base_url
                .filter(|b| !b.is_empty())
                .unwrap_or("http://localhost:11434");
            client
                .get(format!("{base}/api/tags"))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?
        }
        other => bail!("unknown provider: {other}"),
    };

    // Ollama and Gemini use `models[].name`; the OpenAI-style APIs use `data[].id`.
    let mut ids: Vec<String> = if provider == "ollama" || provider == "gemini" {
        json.get("models")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|m| m.get("name").and_then(Value::as_str))
                    // Gemini returns "models/gemini-..."; genai wants the bare id.
                    .map(|n| n.strip_prefix("models/").unwrap_or(n).to_string())
                    .collect()
            })
            .unwrap_or_default()
    } else {
        json.get("data")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    ids.sort();
    ids.dedup();
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_mapping() {
        assert!(adapter_for("anthropic").is_ok());
        assert!(adapter_for("openai").is_ok());
        assert!(adapter_for("openrouter").is_ok());
        assert!(adapter_for("ollama").is_ok());
        assert!(adapter_for("bogus").is_err());
    }

    #[test]
    fn cli_provider_detection() {
        assert!(is_cli_provider("claude-cli"));
        assert!(is_cli_provider("codex-cli"));
        assert!(!is_cli_provider("anthropic"));
        assert!(!is_cli_provider("ollama"));
        // CLI backends are NOT genai adapters.
        assert!(adapter_for("claude-cli").is_err());
    }

    /// Live: distill a tiny transcript through the logged-in `claude` CLI (no API key).
    /// Run with: cargo test -p callimachus --lib cli_distill_smoke -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn cli_distill_smoke() {
        let transcript = "User: how do I activate a python venv?\n\
            Assistant: Run `source .venv/bin/activate`. We decided to use a per-project venv. \
            Gotcha: the fish shell needs `activate.fish`, not the bash script.";
        let d = distill("claude-cli", "", None, transcript).await.unwrap();
        eprintln!("summary: {}", d.summary);
        eprintln!("decisions: {:?}", d.decisions);
        eprintln!("gotchas: {:?}", d.gotchas);
        assert!(!d.summary.is_empty() || !d.decisions.is_empty() || !d.gotchas.is_empty());
    }

    /// Live: the Claude CLI streams a reply token-by-token (many `on_token` calls, not one).
    /// Run with: cargo test -p callimachus --lib cli_stream_smoke -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn cli_stream_smoke() {
        use std::sync::{Arc, Mutex};
        let chunks = Arc::new(Mutex::new(0usize));
        let buf = Arc::new(Mutex::new(String::new()));
        let (c, b) = (chunks.clone(), buf.clone());
        let cancel = tokio_util::sync::CancellationToken::new();
        let full = cli_stream(
            "claude-cli",
            "",
            "Reply with exactly: hello world",
            &cancel,
            move |_kind, t| {
                *c.lock().unwrap() += 1;
                b.lock().unwrap().push_str(t);
            },
        )
        .await
        .unwrap();
        let n = *chunks.lock().unwrap();
        eprintln!("chunks={n} full={full:?}");
        assert!(!full.is_empty());
        assert!(n >= 2, "expected multiple streamed chunks, got {n}");
    }

    #[test]
    fn persist_chat_creates_searchable_thread() {
        let mut p = std::env::temp_dir();
        p.push(format!("callimachus_chat_{}.db", std::process::id()));
        let mut conn = crate::db::open(&p).unwrap();
        let msgs = vec![ChatMessage {
            role: "user".into(),
            content: "explain tauri channels".into(),
        }];
        persist_chat(&mut conn, "chat-1", &msgs, "Channels stream events.").unwrap();

        let hits =
            crate::search::search(&conn, "channels", &crate::search::SearchFilters::default())
                .unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].source, "in_app");
    }
}
