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
    /// goose, opencode, continue, cline, in_app. Empty = all sources.
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
        description = "Search the user's indexed AI coding-agent conversation threads across every tool they use (Claude Code, Codex, Cursor, Gemini, Qwen, Goose, OpenCode, Continue, Cline, and in-app chats). Keyword full-text by default; set hybrid=true to also use on-device semantic similarity. Returns matching threads with snippets and a threadId to fetch. Use this to recall past decisions, prior solutions, or earlier discussion before redoing work."
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
        let project = current_project_root()
            .ok_or_else(|| ErrorData::invalid_params("could not determine current project dir", None))?;
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
        let tags = search::list_tags(&conn)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
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
