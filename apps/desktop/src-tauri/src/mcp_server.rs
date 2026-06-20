//! Callimachus MCP server core — exposes the local thread index as tools any
//! LLM/agent can call over stdio. Reused by two entry points: the standalone
//! `callimachus-mcp` binary, and the desktop app itself when launched with
//! `--mcp` (so the installed app can register *itself* as an MCP server, with no
//! second binary to ship). Same search + context core as the GUI, same index.db.

use std::sync::Mutex;

use crate::{context, embed, search};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler, ServiceExt};
use rusqlite::Connection;
use schemars::JsonSchema;
use serde::Deserialize;

struct Callimachus {
    conn: Mutex<Connection>,
    embedder: embed::Embedder,
    // Read by the #[tool_handler]-generated routing code.
    #[allow(dead_code)]
    tool_router: ToolRouter<Callimachus>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SearchArgs {
    /// The search query.
    query: String,
    /// Optional source filter: any of claude_code, codex, cursor, gemini, qwen,
    /// goose, opencode, continue, cline, roo, kilo, in_app. Empty = all sources.
    #[serde(default)]
    sources: Vec<String>,
    /// Fuse keyword + on-device semantic search (higher recall; loads the embedding model).
    #[serde(default)]
    hybrid: bool,
    /// Include Claude Code subagent transcripts (hidden by default).
    #[serde(default)]
    include_subagents: bool,
    /// Max results to return (default 20).
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetThreadArgs {
    /// The thread id from a search result.
    thread_id: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RecentArgs {
    /// Optional source filter (see search_threads). Empty = all sources.
    #[serde(default)]
    sources: Vec<String>,
    /// Substring-match the project path (e.g. a repo path) to scope results.
    project: Option<String>,
    /// If true, return only threads the user has starred.
    starred: Option<bool>,
    /// Only threads tagged with ANY of these tags (see list_tags). Empty = all.
    #[serde(default)]
    tags: Vec<String>,
    /// Max threads to return (default 20).
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ListTagsArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
struct ListTodosArgs {
    /// Optional text search over the TODO text + thread title (case-insensitive).
    query: Option<String>,
    /// Substring-match the project path (e.g. a repo path) to scope results.
    project: Option<String>,
    /// Optional source filter (see search_threads). Empty = all sources.
    source: Option<String>,
    /// Max TODOs to return (default 100).
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ThreadKnowledgeArgs {
    /// The thread id from a search/recent result.
    thread_id: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CompleteTodoArgs {
    /// The TODO's id (the `id` field from list_open_todos).
    id: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AskArgs {
    /// The question to answer from the user's history.
    question: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FilePathArgs {
    /// A file path or substring (e.g. "embed/mod.rs").
    path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RememberArgs {
    /// What to remember (one concrete sentence).
    text: String,
    /// Project-path substring to attach it to; omit to use the repo the server runs in.
    project: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RecallArgs {
    /// What to recall about (e.g. "auth token refresh", "database migration approach").
    query: String,
    /// Substring-match the project path to scope results. Empty = all projects.
    project: Option<String>,
    /// Max facts to return (default 20).
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ProjectMemoryArgs {
    /// Project-path substring. Omit to use the git repo the server runs in.
    project: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ProjectSearchArgs {
    /// The search query.
    query: String,
    /// Fuse keyword + on-device semantic search.
    #[serde(default)]
    hybrid: bool,
    /// Max results to return (default 20).
    limit: Option<u32>,
}

#[tool_router]
impl Callimachus {
    fn new(conn: Connection) -> Self {
        Self {
            conn: Mutex::new(conn),
            embedder: embed::Embedder::default(),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Search the user's indexed AI coding-agent conversation threads across every tool they use (Claude Code, Codex, Cursor, Gemini, Qwen, Goose, OpenCode, Continue, Cline, Roo Code, Kilo Code, and in-app chats). Keyword full-text by default; set hybrid=true to also use on-device semantic similarity. Returns matching threads with snippets and a threadId to fetch. Use this to recall past decisions, prior solutions, or earlier discussion before redoing work."
    )]
    async fn search_threads(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let filters = search::SearchFilters {
            sources: args.sources,
            hybrid: args.hybrid,
            include_subagents: args.include_subagents,
            limit: Some(args.limit.unwrap_or(20)),
            ..Default::default()
        };
        let conn = self
            .conn
            .lock()
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let hits = if args.hybrid {
            search::hybrid(&conn, &self.embedder, &args.query, &filters)
        } else {
            search::search(&conn, &args.query, &filters)
        }
        .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let json = serde_json::to_string_pretty(&hits)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Fetch one indexed thread as a packed markdown transcript (budget-limited, ready to drop into context). Pass a threadId from search_threads."
    )]
    async fn get_thread(
        &self,
        Parameters(args): Parameters<GetThreadArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let packed = context::pack_thread(&conn, args.thread_id, context::DEFAULT_BUDGET_CHARS)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?
            .ok_or_else(|| ErrorData::invalid_params("thread not found", None))?;
        Ok(CallToolResult::success(vec![Content::text(packed)]))
    }

    #[tool(
        description = "List the user's most recently updated conversation threads (newest first), optionally filtered by source or project path. Use this to see what the user has been working on lately. Returns thread summaries with a threadId to fetch."
    )]
    async fn recent_threads(
        &self,
        Parameters(args): Parameters<RecentArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let filters = search::SearchFilters {
            sources: args.sources,
            project: args.project,
            starred: args.starred,
            tags: args.tags,
            limit: Some(args.limit.unwrap_or(20)),
            ..Default::default()
        };
        let conn = self
            .conn
            .lock()
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let rows = search::recent_threads(&conn, &filters)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let json = serde_json::to_string_pretty(&rows)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Search only the conversation history for the CURRENT project — the git repository (or directory) this MCP server was launched in. Use this first when the user asks about prior work on the project you're in; it scopes results to this repo. Falls back to all sources within that project."
    )]
    async fn search_current_project(
        &self,
        Parameters(args): Parameters<ProjectSearchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let project = current_project_root().ok_or_else(|| {
            ErrorData::invalid_params("could not determine current project dir", None)
        })?;
        let filters = search::SearchFilters {
            project: Some(project),
            hybrid: args.hybrid,
            limit: Some(args.limit.unwrap_or(20)),
            ..Default::default()
        };
        let conn = self
            .conn
            .lock()
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let hits = if args.hybrid {
            search::hybrid(&conn, &self.embedder, &args.query, &filters)
        } else {
            search::search(&conn, &args.query, &filters)
        }
        .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let json = serde_json::to_string_pretty(&hits)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "List all tags the user has applied to their threads, each with the number of threads it's on. Use to discover the user's topic labels / collections, then pass a tag to recent_threads to filter by it."
    )]
    async fn list_tags(
        &self,
        Parameters(_args): Parameters<ListTagsArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let tags =
            search::list_tags(&conn).map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let json = serde_json::to_string_pretty(&tags)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "List unfinished TODOs / action items the user left across past coding sessions (newest first), optionally scoped to a project path or source. Extracted heuristically from the history (markdown task checkboxes + TODO/FIXME markers), so it works with NO API key and no AI distillation. Each TODO carries the threadId it came from — fetch that thread for full context."
    )]
    async fn list_open_todos(
        &self,
        Parameters(args): Parameters<ListTodosArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let todos = crate::knowledge::list_open_todos(
            &conn,
            args.query.as_deref(),
            args.project.as_deref(),
            args.source.as_deref(),
            args.limit.unwrap_or(100) as i64,
        )
        .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let json = serde_json::to_string_pretty(&todos)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Get the distilled knowledge for one thread by threadId: a short summary plus key decisions, gotchas, and open TODOs. A fast, high-signal recap instead of reading the whole transcript. Decisions/gotchas/summary exist only if the user enabled distillation; TODOs are always available."
    )]
    async fn get_thread_knowledge(
        &self,
        Parameters(args): Parameters<ThreadKnowledgeArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let k = crate::knowledge::get_thread_knowledge(&conn, args.thread_id)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let json = serde_json::to_string_pretty(&k)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Recall concrete technical DECISIONS the user made across ALL past sessions (and why), semantically matched to a query. Use BEFORE re-deciding something the user may have already settled. Returns decision facts, each with the threadId it came from. Requires the user to have distilled some threads."
    )]
    async fn recall_decisions(
        &self,
        Parameters(args): Parameters<RecallArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.recall_facts(args, "decision")
    }

    #[tool(
        description = "Recall GOTCHAS / pitfalls / non-obvious constraints the user discovered across ALL past sessions, semantically matched to a query. Use to avoid repeating a known mistake. Returns gotcha facts with the threadId they came from. Requires the user to have distilled some threads."
    )]
    async fn recall_gotchas(
        &self,
        Parameters(args): Parameters<RecallArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.recall_facts(args, "gotcha")
    }

    #[tool(
        description = "Find PRIOR SESSIONS where the user already worked on something similar to a task you're about to start — the 'have I done this before?' guard. Pass a short description of the task/problem as `query`; returns past threads (each with its most-relevant decision or gotcha and the threadId) so you can reuse the earlier solution instead of redoing or re-deciding it. Searches ALL projects unless `project` is given. Call at the START of a task. Requires distilled threads."
    )]
    async fn find_prior_work(
        &self,
        Parameters(args): Parameters<RecallArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let qv = embed::embed_query(&self.embedder, &args.query)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let Some(qv) = qv else {
            return Ok(CallToolResult::success(vec![Content::text(
                "[]".to_string(),
            )]));
        };
        let conn = self
            .conn
            .lock()
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let hits = crate::knowledge::find_prior_work(
            &conn,
            &qv,
            args.project.as_deref(),
            args.limit.unwrap_or(8) as usize,
        )
        .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let json = serde_json::to_string_pretty(&hits)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Get a project's durable MEMORY: the decisions, gotchas, and open TODOs distilled across ALL past AI-coding sessions on it, with coverage counts. Omit `project` to use the repo the server runs in. Call this at the START of work on a project to recall what was already decided and what to watch out for. Decisions/gotchas need the user to have distilled threads; TODOs are always available."
    )]
    async fn project_memory(
        &self,
        Parameters(args): Parameters<ProjectMemoryArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let raw = args
            .project
            .or_else(current_project_root)
            .unwrap_or_default();
        let project = crate::indexer::canonical_project(&raw).unwrap_or(raw);
        let conn = self
            .conn
            .lock()
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let mem = crate::knowledge::get_project_memory(&conn, &project, 60)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let json = serde_json::to_string_pretty(&mem)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Find every past thread that mentioned a file path (e.g. 'embed/mod.rs') — which sessions touched this file. Substring match over indexed file references. Returns thread summaries."
    )]
    async fn threads_for_file(
        &self,
        Parameters(args): Parameters<FilePathArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let hits = crate::search::threads_with_file(&conn, &args.path, 50)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let json = serde_json::to_string_pretty(&hits)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Mark a TODO done so it drops out of the open-TODO lists. Pass the `id` from list_open_todos. The completion persists across re-indexing."
    )]
    async fn complete_todo(
        &self,
        Parameters(args): Parameters<CompleteTodoArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        crate::knowledge::set_todo_done(&conn, args.id, true)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "TODO {} marked done",
            args.id
        ))]))
    }

    #[tool(
        description = "Answer a question from the user's OWN past sessions (RAG): retrieves the most relevant threads and returns a synthesized answer with [thread N] citations + the source list. Use for 'how did we...' / 'what did I decide about...' instead of reading many threads. Needs an LLM engine configured (distillation enabled)."
    )]
    async fn ask_history(
        &self,
        Parameters(args): Parameters<AskArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let q = args.question.trim().to_string();
        if q.is_empty() {
            return Err(ErrorData::internal_error("question is required", None));
        }
        // Embed outside the lock; prepare under it; LLM after the conn is dropped.
        let qv = embed::embed_query(&self.embedder, &q)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let prep = {
            let conn = self
                .conn
                .lock()
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
            crate::prepare_ask(&conn, &q, qv.as_deref())
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?
        };
        let Some(prep) = prep else {
            return Ok(CallToolResult::success(vec![Content::text(
                "No relevant threads found in history.".to_string(),
            )]));
        };
        let answer = crate::agent::answer(
            &prep.provider,
            &prep.model,
            prep.key.as_deref(),
            &q,
            &prep.context,
        )
        .await
        .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let mut out = answer;
        out.push_str("\n\nSources:\n");
        for s in &prep.sources {
            out.push_str(&format!(
                "- [thread {}] {}\n",
                s.thread_id,
                s.title.as_deref().unwrap_or("untitled")
            ));
        }
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(
        description = "Record a DECISION you/the user just made for a project, so it persists in the project's memory and future cross-thread recall. Omit `project` to use the repo the server runs in. Use when you settle a technical choice worth remembering."
    )]
    async fn record_decision(
        &self,
        Parameters(args): Parameters<RememberArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.record(args, "decision")
    }

    #[tool(
        description = "Record a GOTCHA / pitfall just discovered for a project, so it persists in the project's memory and future recall. Omit `project` to use the current repo."
    )]
    async fn record_gotcha(
        &self,
        Parameters(args): Parameters<RememberArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.record(args, "gotcha")
    }
}

impl Callimachus {
    /// Shared write-back path for record_decision/record_gotcha: record the fact for the
    /// project (synthetic notes thread) and embed it so it surfaces in recall immediately.
    fn record(&self, args: RememberArgs, kind: &str) -> Result<CallToolResult, ErrorData> {
        let text = args.text.trim().to_string();
        if text.is_empty() {
            return Err(ErrorData::internal_error("text is required", None));
        }
        let raw = args
            .project
            .or_else(current_project_root)
            .unwrap_or_default();
        let project = crate::indexer::canonical_project(&raw).unwrap_or(raw);
        let now = chrono::Utc::now().timestamp();
        let mut conn = self
            .conn
            .lock()
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        crate::knowledge::record_fact(&conn, &project, kind, &text, now)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        crate::embed::embed_pending_facts_conn(&mut conn, &self.embedder)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Recorded {kind} for {project}"
        ))]))
    }

    /// Shared recall path for the recall_decisions/recall_gotchas tools: embed the query,
    /// then run the SQL KNN over `vec_facts`.
    fn recall_facts(&self, args: RecallArgs, kind: &str) -> Result<CallToolResult, ErrorData> {
        let qv = embed::embed_query(&self.embedder, &args.query)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let Some(qv) = qv else {
            return Ok(CallToolResult::success(vec![Content::text(
                "[]".to_string(),
            )]));
        };
        let conn = self
            .conn
            .lock()
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let hits = crate::knowledge::recall(
            &conn,
            &qv,
            kind,
            args.project.as_deref(),
            args.limit.unwrap_or(20) as usize,
        )
        .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let json = serde_json::to_string_pretty(&hits)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }
}

/// The git repo root for the process's cwd, walking up for a `.git`; falls back
/// to the cwd itself. Used to scope `search_current_project`.
fn current_project_root() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let mut dir = cwd.as_path();
    loop {
        if dir.join(".git").exists() {
            return Some(dir.to_string_lossy().to_string());
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => return Some(cwd.to_string_lossy().to_string()), // no repo — scope to cwd
        }
    }
}

#[tool_handler]
impl ServerHandler for Callimachus {
    fn get_info(&self) -> ServerInfo {
        // Implementation is #[non_exhaustive]; build via from_build_env then set fields.
        let mut server_info = Implementation::from_build_env();
        server_info.name = "callimachus".into();
        server_info.title = Some("Callimachus".into());
        server_info.version = env!("CARGO_PKG_VERSION").into();
        server_info.description =
            Some("Search the user's indexed AI agent conversation history".into());
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(server_info)
            .with_instructions(
                "Search and read the user's indexed AI coding-agent conversation history. \
                 Use search_threads to find relevant threads, then get_thread to read one in full.",
            )
    }
}

/// Serve the MCP protocol over stdio against `conn`, blocking until the client
/// disconnects. Both the `callimachus-mcp` binary and the app's `--mcp` mode call this.
pub async fn serve(conn: Connection) -> anyhow::Result<()> {
    let service = Callimachus::new(conn)
        .serve((tokio::io::stdin(), tokio::io::stdout()))
        .await?;
    service.waiting().await?;
    Ok(())
}
