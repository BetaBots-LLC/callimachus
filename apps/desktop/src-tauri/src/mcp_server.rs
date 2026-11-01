//! Callimachus MCP server core — exposes the local thread index as tools any
//! LLM/agent can call over stdio. Reused by two entry points: the standalone
//! `callimachus-mcp` binary, and the desktop app itself when launched with
//! `--mcp` (so the installed app can register *itself* as an MCP server, with no
//! second binary to ship). Same search + context core as the GUI, same index.db.
//!
//! Token discipline: every response here lands in an agent's context window, so results
//! are compact JSON (never pretty-printed), list defaults are small with documented
//! maximums, transcripts take a `budget`, and project memory defaults to the same
//! size-budgeted markdown the SessionStart hook injects.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::{context, embed, search, snapshot};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, JsonObject, ProtocolVersion, ServerCapabilities,
    ServerConfig,
};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler, ServiceExt};
use rusqlite::Connection;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Limits. Defaults are what an agent gets when it doesn't ask; maximums cap what it can ask
// for. Keep these in sync with the "(default N, max M)" text in the argument docs below.
// ---------------------------------------------------------------------------

const SEARCH_DEFAULT: u32 = 10;
const SEARCH_MAX: u32 = 50;
const RECENT_DEFAULT: u32 = 10;
const RECENT_MAX: u32 = 50;
const TODOS_DEFAULT: u32 = 20;
const TODOS_MAX: u32 = 100;
const RECALL_DEFAULT: u32 = 10;
const RECALL_MAX: u32 = 50;
const PRIOR_WORK_DEFAULT: u32 = 5;
const PRIOR_WORK_MAX: u32 = 20;
const FILE_THREADS_DEFAULT: u32 = 15;
const FILE_THREADS_MAX: u32 = 50;
const ISSUES_DEFAULT: u32 = 10;
const ISSUES_MAX: u32 = 50;
const SNAPSHOTS_DEFAULT: u32 = 20;
const SNAPSHOTS_MAX: u32 = 50;
/// check_decision is a guard, not a browse: a handful of on-topic decisions is the useful set.
const CHECK_DECISION_K: usize = 8;

/// Default transcript budget for get_thread / load_snapshot (~6k tokens). Half the desktop
/// app's DEFAULT_BUDGET_CHARS: an agent can ask for more, but most recall needs the gist.
const TRANSCRIPT_BUDGET_DEFAULT: u32 = 24_000;
const TRANSCRIPT_BUDGET_MIN: u32 = 2_000;
const TRANSCRIPT_BUDGET_MAX: u32 = 96_000;

/// project_memory: facts per kind for format=json when `limit` is omitted, and the hard cap.
const MEMORY_JSON_DEFAULT: u32 = 10;
const MEMORY_PER_KIND_MAX: u32 = 40;

/// `search::search` keeps at most this many message hits per thread; we group them back into
/// one entry per thread, so ask for `limit * this` hits to still fill `limit` threads.
const SEARCH_HITS_PER_THREAD: u32 = 3;

/// Separator `snapshot::create` puts between the carry-forward memory and the transcript.
const SNAPSHOT_DIVIDER: &str = "\n\n---\n\n";

/// Returned by every read tool while the index holds no threads. The standalone server (npx,
/// MCP registry) only reads the index; without the desktop app nothing ever fills it, and a
/// bare `[]` would read to the agent as "the user has no prior work".
const EMPTY_INDEX_NOTICE: &str =
    "No Callimachus index yet: no conversation threads have been indexed. \
This MCP server only reads the index; the Callimachus desktop app builds it. Install the app \
(https://github.com/BetaBots-LLC/callimachus/releases/latest), open it once to index your agent \
history (or click Reindex in Settings), then retry. If the app is already installed and indexed, \
point this server at its index with the CALLIMACHUS_DB environment variable.";

/// The server's own usage guidance, sent once at initialize. Kept under 400 chars.
const INSTRUCTIONS: &str = "Local index of the user's past AI coding sessions. Project memory \
may already be in context via CLAUDE.md/AGENTS.md or a session hook; if so, skip project_memory \
and use check_decision or recall_* with a specific topic. Search with search_current_project or \
search_threads (hybrid=true for fuzzy recall), then get_thread_knowledge before get_thread.";

struct Callimachus {
    conn: Mutex<Connection>,
    embedder: embed::Embedder,
    /// Set once the index is seen to contain threads, so the empty-index probe stops running.
    /// An index never goes back to empty in practice, so this never needs to reset.
    has_threads: AtomicBool,
    tool_router: ToolRouter<Callimachus>,
}

// ---------------------------------------------------------------------------
// Tool arguments. Doc comments become the JSON-schema `description` an agent sees, so they
// stay short and state defaults/maximums.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
struct SearchArgs {
    /// The search query.
    query: String,
    /// Source filter, e.g. claude_code, codex, cursor, gemini, copilot, in_app. Empty = all.
    #[serde(default)]
    sources: Vec<String>,
    /// Also match semantically (fuzzy or paraphrased queries; slower).
    #[serde(default)]
    hybrid: bool,
    /// Include Claude Code subagent transcripts (hidden by default).
    #[serde(default)]
    include_subagents: bool,
    /// Max threads (default 10, max 50).
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetThreadArgs {
    /// Thread id from a search or recent result.
    thread_id: i64,
    /// Max chars to return (default 24000, max 96000).
    budget: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RecentArgs {
    /// Source filter (see search_threads). Empty = all.
    #[serde(default)]
    sources: Vec<String>,
    /// Project path substring.
    project: Option<String>,
    /// Only starred threads.
    starred: Option<bool>,
    /// Only threads with ANY of these tags (see list_tags).
    #[serde(default)]
    tags: Vec<String>,
    /// Max threads (default 10, max 50).
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ListTagsArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
struct ListTodosArgs {
    /// Case-insensitive text match on the TODO or its thread title.
    query: Option<String>,
    /// Project path substring.
    project: Option<String>,
    /// Source filter (see search_threads).
    source: Option<String>,
    /// Max TODOs (default 20, max 100).
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ThreadKnowledgeArgs {
    /// Thread id from a search or recent result.
    thread_id: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CompleteTodoArgs {
    /// The TODO's `id` from list_open_todos.
    id: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AskArgs {
    /// The question to answer from the user's history.
    question: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FilePathArgs {
    /// File path or substring, e.g. "embed/mod.rs".
    path: String,
    /// Max threads (default 15, max 50).
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RecurringIssuesArgs {
    /// Project path substring; omit for the current repo, "*" for all projects.
    project: Option<String>,
    /// Look-back window in days (default 180).
    since_days: Option<u32>,
    /// Max error clusters (default 10, max 50).
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RememberArgs {
    /// What to remember, as one concrete sentence.
    text: String,
    /// Project path; omit for the current repo.
    project: Option<String>,
    /// For a decision: why it was made.
    rationale: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CheckDecisionArgs {
    /// The decision or change you are about to make.
    proposal: String,
    /// Project path substring. Empty = all projects.
    project: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RecallArgs {
    /// Topic to recall, e.g. "auth token refresh".
    query: String,
    /// Project path substring. Empty = all projects.
    project: Option<String>,
    /// Max facts (default 10, max 50).
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PriorWorkArgs {
    /// Short description of the task or problem.
    query: String,
    /// Project path substring. Empty = all projects.
    project: Option<String>,
    /// Max threads (default 5, max 20).
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ProjectMemoryArgs {
    /// Project path; omit for the repo this server runs in.
    project: Option<String>,
    /// "markdown" (default, size-budgeted) or "json".
    format: Option<String>,
    /// Max facts per kind (max 40).
    limit: Option<u32>,
    /// Any of "decisions", "gotchas", "todos". Empty = all.
    #[serde(default)]
    kinds: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ProjectSearchArgs {
    /// The search query.
    query: String,
    /// Also match semantically (fuzzy or paraphrased queries; slower).
    #[serde(default)]
    hybrid: bool,
    /// Max threads (default 10, max 50).
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SnapshotArgs {
    /// Thread id to checkpoint.
    thread_id: i64,
    /// Snapshot label (defaults to the thread title).
    label: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ListSnapshotsArgs {
    /// Project path substring. Empty = all projects.
    project: Option<String>,
    /// Max snapshots (default 20, max 50).
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct LoadSnapshotArgs {
    /// Snapshot id from list_snapshots or snapshot_session.
    id: i64,
    /// Max chars to return (default 24000, max 96000).
    budget: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct LinkedCommitsArgs {
    /// Thread id to find produced commits for.
    thread_id: i64,
}

// ---------------------------------------------------------------------------
// MCP-specific result shapes (more compact than the GUI's).
// ---------------------------------------------------------------------------

/// One thread in a search result: the thread's identity once, then its best-matching
/// snippets. The raw hit list repeats title/source/path for every matching message.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ThreadMatch {
    thread_id: i64,
    source: String,
    title: Option<String>,
    project_path: Option<String>,
    /// Timestamp of the best-matching message.
    ts: Option<i64>,
    /// Best first, each prefixed with the speaker role.
    snippets: Vec<String>,
}

/// project_memory format=json: counts plus id/threadId/text per fact. Leaves out the per-fact
/// thread title and createdAt that `knowledge::ProjectMemory` carries for the GUI.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CompactMemory<'a> {
    project: &'a str,
    thread_count: i64,
    distilled_count: i64,
    pending_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    decisions: Option<Vec<CompactFact<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    gotchas: Option<Vec<CompactFact<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    open_todos: Option<Vec<CompactFact<'a>>>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CompactFact<'a> {
    id: i64,
    thread_id: i64,
    /// Redacted: TODOs are lifted from raw transcripts and can carry credentials.
    text: std::borrow::Cow<'a, str>,
    #[serde(skip_serializing_if = "is_false")]
    pinned: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[tool_router]
impl Callimachus {
    fn new(conn: Connection) -> Self {
        let mut tool_router = Self::tool_router();
        // schemars stamps every input schema with a `$schema` URL and the Rust struct name as
        // `title`. Clients that load all tools up front (Cursor, Gemini) pay for both on every
        // session, and neither helps a model fill in arguments.
        for route in tool_router.map.values_mut() {
            route.attr.input_schema = slim_input_schema(&route.attr.input_schema);
        }
        Self {
            conn: Mutex::new(conn),
            // Never downloads: the server runs inside the user's repo (an agent's CWD) and
            // a tool call must not block on a 128 MB fetch. Without the model the app
            // downloaded, semantic arms fall back to keyword search.
            embedder: embed::Embedder::cached_only(),
            has_threads: AtomicBool::new(false),
            tool_router,
        }
    }

    #[tool(
        description = "Keyword search over the user's past AI coding sessions in every tool (Claude Code, Codex, Cursor, Gemini, Copilot, ...). One entry per thread with snippets; hybrid=true adds semantic matching.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn search_threads(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(notice) = self.empty_index_notice()? {
            return Ok(notice);
        }
        let limit = clamp_limit(args.limit, SEARCH_DEFAULT, SEARCH_MAX);
        let filters = search::SearchFilters {
            sources: args.sources,
            hybrid: args.hybrid,
            include_subagents: args.include_subagents,
            limit: Some(limit * SEARCH_HITS_PER_THREAD),
            ..Default::default()
        };
        let hits = self.run_search(&args.query, &filters)?;
        json_result(&group_by_thread(hits, limit as usize))
    }

    #[tool(
        description = "Read one thread as a packed markdown transcript, trimmed to `budget` chars (default 24000). Try get_thread_knowledge first for a short recap; use this when you need the actual discussion.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_thread(
        &self,
        Parameters(args): Parameters<GetThreadArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(notice) = self.empty_index_notice()? {
            return Ok(notice);
        }
        let budget = clamp_budget(args.budget);
        let conn = self.lock()?;
        let packed = context::pack_thread(&conn, args.thread_id, budget)
            .map_err(internal)?
            .ok_or_else(|| ErrorData::invalid_params("thread not found", None))?;
        Ok(text_result(packed))
    }

    #[tool(
        description = "Save a resumable checkpoint of a thread (packed transcript plus the project's decisions, gotchas and TODOs) before context compaction or a handoff to another agent. Returns the snapshot id.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn snapshot_session(
        &self,
        Parameters(args): Parameters<SnapshotArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(notice) = self.empty_index_notice()? {
            return Ok(notice);
        }
        let conn = self.lock()?;
        let snap =
            snapshot::create(&conn, args.thread_id, args.label.as_deref()).map_err(internal)?;
        json_result(&snap)
    }

    #[tool(
        description = "List saved session snapshots, newest first, optionally scoped to a project path substring. Returns ids for load_snapshot.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_snapshots(
        &self,
        Parameters(args): Parameters<ListSnapshotsArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(notice) = self.empty_index_notice()? {
            return Ok(notice);
        }
        let limit = clamp_limit(args.limit, SNAPSHOTS_DEFAULT, SNAPSHOTS_MAX);
        let conn = self.lock()?;
        let snaps =
            snapshot::list(&conn, args.project.as_deref(), limit as usize).map_err(internal)?;
        json_result(&snaps)
    }

    #[tool(
        description = "Load a saved snapshot (carried-forward project memory plus transcript) as markdown to continue that session, trimmed to `budget` chars (default 24000). Pass an id from list_snapshots.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn load_snapshot(
        &self,
        Parameters(args): Parameters<LoadSnapshotArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(notice) = self.empty_index_notice()? {
            return Ok(notice);
        }
        let budget = clamp_budget(args.budget);
        let conn = self.lock()?;
        let snap = snapshot::load(&conn, args.id)
            .map_err(internal)?
            .ok_or_else(|| ErrorData::invalid_params("snapshot not found", None))?;
        // Snapshots saved before pack_thread learned to redact still hold raw credentials;
        // redact on the way out too (a no-op on already-redacted bodies).
        let body = context::redact_secrets(&snap.body);
        Ok(text_result(fit_snapshot_body(&body, budget)))
    }

    #[tool(
        description = "Git commits a thread likely produced, inferred by matching its files against git log in the same time window: short SHA, subject, file-overlap count. Empty until the user runs `cal commits` in the repo.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn linked_commits(
        &self,
        Parameters(args): Parameters<LinkedCommitsArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(notice) = self.empty_index_notice()? {
            return Ok(notice);
        }
        let conn = self.lock()?;
        let links = crate::gitlink::linked_commits(&conn, args.thread_id).map_err(internal)?;
        json_result(&links)
    }

    #[tool(
        description = "Errors the user keeps hitting across sessions, grouped by normalized signature with counts and first/last seen. Current repo by default; project=\"*\" for all. Check before retrying an approach.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn recurring_issues(
        &self,
        Parameters(args): Parameters<RecurringIssuesArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(notice) = self.empty_index_notice()? {
            return Ok(notice);
        }
        let days = args.since_days.unwrap_or(180).max(1) as i64;
        let since = chrono::Utc::now().timestamp() - days * 24 * 3600;
        let limit = clamp_limit(args.limit, ISSUES_DEFAULT, ISSUES_MAX) as usize;
        // Default: scope to the current repo (matches record_*/search_current_project);
        // "*"/"all" searches every project.
        let project: Option<String> = match args.project.as_deref().map(str::trim) {
            Some("*") | Some("all") => None,
            Some(p) if !p.is_empty() => Some(crate::knowledge::project_key(p)),
            _ => cwd_project_key(),
        };
        let conn = self.lock()?;
        let clusters = crate::issues::recurring_issues(&conn, project.as_deref(), since, limit)
            .map_err(internal)?;
        json_result(&clusters)
    }

    #[tool(
        description = "The user's most recently updated threads, newest first, filterable by source, project path, starred or tags. Use to see what they worked on lately.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn recent_threads(
        &self,
        Parameters(args): Parameters<RecentArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(notice) = self.empty_index_notice()? {
            return Ok(notice);
        }
        let filters = search::SearchFilters {
            sources: args.sources,
            project: args.project,
            starred: args.starred,
            tags: args.tags,
            limit: Some(clamp_limit(args.limit, RECENT_DEFAULT, RECENT_MAX)),
            ..Default::default()
        };
        let conn = self.lock()?;
        let rows = search::recent_threads(&conn, &filters).map_err(internal)?;
        json_result(&rows)
    }

    #[tool(
        description = "Keyword search scoped to the git repo this server runs in. Prefer it over search_threads for questions about the current project. One entry per thread; hybrid=true adds semantic matching.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn search_current_project(
        &self,
        Parameters(args): Parameters<ProjectSearchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(notice) = self.empty_index_notice()? {
            return Ok(notice);
        }
        // The same key the indexer stores (worktrees resolved to the main repo, the walk
        // stopping at $HOME). A raw walk-up root missed every thread of a linked worktree and
        // let a dotfiles ~/.git scope the search to all of home.
        let project = cwd_project_key().ok_or_else(|| {
            ErrorData::invalid_params("could not determine current project dir", None)
        })?;
        let limit = clamp_limit(args.limit, SEARCH_DEFAULT, SEARCH_MAX);
        let filters = search::SearchFilters {
            project: Some(project),
            hybrid: args.hybrid,
            limit: Some(limit * SEARCH_HITS_PER_THREAD),
            ..Default::default()
        };
        let hits = self.run_search(&args.query, &filters)?;
        json_result(&group_by_thread(hits, limit as usize))
    }

    #[tool(
        description = "List the user's thread tags with thread counts. Pass a tag to recent_threads to filter by it.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_tags(
        &self,
        Parameters(_args): Parameters<ListTagsArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(notice) = self.empty_index_notice()? {
            return Ok(notice);
        }
        let conn = self.lock()?;
        let tags = search::list_tags(&conn).map_err(internal)?;
        json_result(&tags)
    }

    #[tool(
        description = "Open TODOs left in past sessions (task checkboxes, TODO/FIXME markers), newest first, filterable by text, project or source. Each has an id for complete_todo and the threadId it came from.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_open_todos(
        &self,
        Parameters(args): Parameters<ListTodosArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(notice) = self.empty_index_notice()? {
            return Ok(notice);
        }
        let limit = clamp_limit(args.limit, TODOS_DEFAULT, TODOS_MAX);
        let conn = self.lock()?;
        let todos = crate::knowledge::list_open_todos(
            &conn,
            args.query.as_deref(),
            args.project.as_deref(),
            args.source.as_deref(),
            limit as i64,
        )
        .map_err(internal)?
        .into_iter()
        .map(|mut t| {
            t.text = crate::context::redact_secrets(&t.text).into_owned();
            t
        })
        .collect::<Vec<_>>();
        json_result(&todos)
    }

    #[tool(
        description = "Short distilled recap of one thread: summary, decisions, gotchas and open TODOs. Much cheaper than get_thread, so try it first. Summary, decisions and gotchas exist only for distilled threads.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_thread_knowledge(
        &self,
        Parameters(args): Parameters<ThreadKnowledgeArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(notice) = self.empty_index_notice()? {
            return Ok(notice);
        }
        let conn = self.lock()?;
        let mut k =
            crate::knowledge::get_thread_knowledge(&conn, args.thread_id).map_err(internal)?;
        // Facts and TODOs come from raw transcripts and are stored unredacted; scrub them like
        // every other tool that returns their text.
        let redact = |s: &mut String| {
            if let std::borrow::Cow::Owned(r) = crate::context::redact_secrets(s) {
                *s = r;
            }
        };
        if let Some(summary) = k.summary.as_mut() {
            redact(summary);
        }
        for f in k
            .decisions
            .iter_mut()
            .chain(&mut k.gotchas)
            .chain(&mut k.todos)
        {
            redact(&mut f.text);
        }
        json_result(&k)
    }

    #[tool(
        description = "Semantic recall of technical decisions the user made in past sessions, with rationale and source threadId. Use before re-deciding something that may be settled. Needs distilled or recorded facts.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn recall_decisions(
        &self,
        Parameters(args): Parameters<RecallArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.recall_facts(args, "decision")
    }

    #[tool(
        description = "Semantic recall of pitfalls and non-obvious constraints found in past sessions, with source threadId. Use to avoid repeating a known mistake. Needs distilled or recorded facts.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn recall_gotchas(
        &self,
        Parameters(args): Parameters<RecallArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.recall_facts(args, "gotcha")
    }

    #[tool(
        description = "\"Have I done this before?\" Pass a short task description; returns past threads that tackled something similar, each with its best-matching decision or gotcha. Call at the start of a task.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn find_prior_work(
        &self,
        Parameters(args): Parameters<PriorWorkArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        // Check before embedding: the first embed loads (and may download) the model.
        if let Some(notice) = self.empty_index_notice()? {
            return Ok(notice);
        }
        let Some(qv) = embed::embed_query(&self.embedder, &args.query).map_err(internal)? else {
            return Ok(no_query_vector());
        };
        let limit = clamp_limit(args.limit, PRIOR_WORK_DEFAULT, PRIOR_WORK_MAX);
        let conn = self.lock()?;
        let hits =
            crate::knowledge::find_prior_work(&conn, &qv, args.project.as_deref(), limit as usize)
                .map_err(internal)?;
        json_result(&hits)
    }

    #[tool(
        description = "Contradiction guard: pass a decision you are about to make as `proposal` to get settled prior decisions on the same topic, with rationale. Empty means no conflict; if one contradicts your plan, ask the user.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn check_decision(
        &self,
        Parameters(args): Parameters<CheckDecisionArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(notice) = self.empty_index_notice()? {
            return Ok(notice);
        }
        let Some(qv) = embed::embed_query(&self.embedder, &args.proposal).map_err(internal)? else {
            return Ok(no_query_vector());
        };
        let conn = self.lock()?;
        let hits = crate::knowledge::check_contradiction(
            &conn,
            &qv,
            args.project.as_deref(),
            CHECK_DECISION_K,
        )
        .map_err(internal)?;
        json_result(&hits)
    }

    #[tool(
        description = "A project's distilled decisions, gotchas and open TODOs as size-budgeted markdown (format=json for data). Skip it when a Callimachus memory block is already in context via CLAUDE.md, AGENTS.md or a hook.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn project_memory(
        &self,
        Parameters(args): Parameters<ProjectMemoryArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(notice) = self.empty_index_notice()? {
            return Ok(notice);
        }
        let format = MemoryFormat::parse(args.format.as_deref())?;
        let kinds = MemoryKinds::parse(&args.kinds)?;
        let limit = args.limit.map(|n| n.clamp(1, MEMORY_PER_KIND_MAX) as usize);

        let project = project_key_or_cwd(args.project);

        // Markdown fetches the full per-kind cap and lets the budget pick what fits, so its
        // "+N more" footer counts what was actually left out. JSON returns exactly `limit`.
        let per_kind = match format {
            MemoryFormat::Markdown => MEMORY_PER_KIND_MAX as usize,
            MemoryFormat::Json => limit.unwrap_or(MEMORY_JSON_DEFAULT as usize),
        };
        let mut mem = {
            let conn = self.lock()?;
            crate::knowledge::get_project_memory(&conn, &project, per_kind).map_err(internal)?
        };
        kinds.retain_in(&mut mem);

        match format {
            MemoryFormat::Json => json_result(&compact_memory(&mem, kinds)),
            MemoryFormat::Markdown => {
                if mem.decisions.is_empty() && mem.gotchas.is_empty() && mem.open_todos.is_empty() {
                    return Ok(text_result(empty_memory_message(&mem)));
                }
                let budget = memory_budget(limit);
                Ok(text_result(crate::export::agent_memory_md_budgeted(
                    &project, &mem, None, &budget,
                )))
            }
        }
    }

    #[tool(
        description = "Past threads that mentioned a file path (substring match, e.g. \"embed/mod.rs\"), newest first. Use to find which sessions touched a file before editing it.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn threads_for_file(
        &self,
        Parameters(args): Parameters<FilePathArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(notice) = self.empty_index_notice()? {
            return Ok(notice);
        }
        let limit = clamp_limit(args.limit, FILE_THREADS_DEFAULT, FILE_THREADS_MAX);
        let conn = self.lock()?;
        let hits =
            crate::search::threads_with_file(&conn, &args.path, limit as i64).map_err(internal)?;
        json_result(&hits)
    }

    #[tool(
        description = "Mark a TODO done (by its id from list_open_todos) so it drops out of the open lists. Persists across re-indexing.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn complete_todo(
        &self,
        Parameters(args): Parameters<CompleteTodoArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(notice) = self.empty_index_notice()? {
            return Ok(notice);
        }
        let conn = self.lock()?;
        if let Err(e) = crate::knowledge::set_todo_done(&conn, args.id, true) {
            return write_error(e);
        }
        Ok(text_result(format!("TODO {} marked done", args.id)))
    }

    // Read-only as far as the index goes, but the retrieved history is sent to the configured
    // LLM provider, so this is the one tool flagged open-world.
    #[tool(
        description = "Answer a question from the user's own past sessions (RAG) with [thread N] citations and a source list. Use for \"how did we...\" questions instead of reading many threads. Needs an LLM engine set up in the app.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn ask_history(
        &self,
        Parameters(args): Parameters<AskArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let q = args.question.trim().to_string();
        if q.is_empty() {
            return Err(ErrorData::invalid_params("question is required", None));
        }
        if let Some(notice) = self.empty_index_notice()? {
            return Ok(notice);
        }
        // Embed outside the lock; prepare under it; LLM after the conn is dropped.
        let qv = embed::embed_query(&self.embedder, &q).map_err(internal)?;
        let prep = {
            let conn = self.lock()?;
            crate::prepare_ask(&conn, &q, qv.as_deref()).map_err(internal)?
        };
        let Some(prep) = prep else {
            return Ok(text_result(
                "No relevant threads found in history.".to_string(),
            ));
        };
        let answer = crate::agent::answer(
            &prep.provider,
            &prep.model,
            prep.key.as_deref(),
            prep.base_url.as_deref(),
            &q,
            &prep.context,
        )
        .await
        .map_err(internal)?;
        let mut out = answer;
        out.push_str("\n\nSources:\n");
        for s in &prep.sources {
            out.push_str(&format!(
                "- [thread {}] {}\n",
                s.thread_id,
                s.title.as_deref().unwrap_or("untitled")
            ));
        }
        Ok(text_result(out))
    }

    #[tool(
        description = "Record a decision just made (with `rationale` when known) into the project's memory, so later recall and check_decision see it. Omit `project` to use the current repo.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn record_decision(
        &self,
        Parameters(args): Parameters<RememberArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.record(args, "decision")
    }

    #[tool(
        description = "Record a pitfall just discovered into the project's memory so later sessions see it. Omit `project` to use the current repo.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn record_gotcha(
        &self,
        Parameters(args): Parameters<RememberArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.record(args, "gotcha")
    }
}

impl Callimachus {
    fn lock(&self) -> Result<MutexGuard<'_, Connection>, ErrorData> {
        self.conn.lock().map_err(internal)
    }

    /// The empty-index guidance as a tool result, or None once the index has real threads.
    /// The synthetic "Recorded memory" threads that record_* creates don't count: they would
    /// otherwise flip a never-indexed server to "has data" after one agent write.
    fn empty_index_notice(&self) -> Result<Option<CallToolResult>, ErrorData> {
        if self.has_threads.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let indexed: bool = self
            .lock()?
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM threads WHERE external_id NOT LIKE 'callimachus-notes:%')",
                [],
                |r| r.get(0),
            )
            .map_err(internal)?;
        if indexed {
            self.has_threads.store(true, Ordering::Relaxed);
            Ok(None)
        } else {
            Ok(Some(text_result(EMPTY_INDEX_NOTICE.to_string())))
        }
    }

    /// Keyword or hybrid search under the connection lock.
    fn run_search(
        &self,
        query: &str,
        filters: &search::SearchFilters,
    ) -> Result<Vec<search::SearchHit>, ErrorData> {
        let conn = self.lock()?;
        if filters.hybrid {
            search::hybrid(&conn, &self.embedder, query, filters)
        } else {
            search::search(&conn, query, filters)
        }
        .map_err(internal)
    }

    /// Shared write-back path for record_decision/record_gotcha: record the fact for the
    /// project (synthetic notes thread) and embed it so it surfaces in recall immediately.
    fn record(&self, args: RememberArgs, kind: &str) -> Result<CallToolResult, ErrorData> {
        let text = args.text.trim().to_string();
        if text.is_empty() {
            return Err(ErrorData::invalid_params("text is required", None));
        }
        // Writes still go through on an empty index (the note persists and shows up once the
        // app indexes), but the agent should learn that recall has nothing to search yet.
        let empty_notice = self.empty_index_notice()?.is_some();
        let project = project_key_or_cwd(args.project);
        let now = chrono::Utc::now().timestamp();
        let mut conn = self.lock()?;
        let rationale = args
            .rationale
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty());
        if let Err(e) = crate::knowledge::record_fact(&conn, &project, kind, &text, rationale, now)
        {
            return write_error(e);
        }
        let mut msg = format!("Recorded {kind} for {project}");
        // Best effort: the fact is already saved, so an embedding failure (model not
        // downloaded yet, offline) must not report the write as failed and invite a duplicate
        // retry. The desktop app embeds pending facts on its next pass.
        if let Err(e) = crate::embed::embed_pending_facts_conn(&mut conn, &self.embedder) {
            msg.push_str(&format!(
                " (not yet searchable by meaning: {e}; the desktop app will embed it later)"
            ));
        }
        if empty_notice {
            msg.push_str("\n\n");
            msg.push_str(EMPTY_INDEX_NOTICE);
        }
        Ok(text_result(msg))
    }

    /// Shared recall path for the recall_decisions/recall_gotchas tools: embed the query,
    /// then run the SQL KNN over `vec_facts`.
    fn recall_facts(&self, args: RecallArgs, kind: &str) -> Result<CallToolResult, ErrorData> {
        if let Some(notice) = self.empty_index_notice()? {
            return Ok(notice);
        }
        let Some(qv) = embed::embed_query(&self.embedder, &args.query).map_err(internal)? else {
            return Ok(no_query_vector());
        };
        let limit = clamp_limit(args.limit, RECALL_DEFAULT, RECALL_MAX);
        let conn = self.lock()?;
        let hits =
            crate::knowledge::recall(&conn, &qv, kind, args.project.as_deref(), limit as usize)
                .map_err(internal)?;
        json_result(&hits)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn internal<E: std::fmt::Display>(e: E) -> ErrorData {
    // `{:#}` prints an anyhow error's whole context chain ("what failed: why"), where
    // to_string() keeps only the outermost layer. Other error types ignore the flag.
    ErrorData::internal_error(format!("{e:#}"), None)
}

/// What a write tool says when the index lock outlasts the sidecar's busy wait.
const INDEX_BUSY_NOTICE: &str =
    "The index is busy right now (an upgrade or a large cleanup); try again in a minute.";

/// A failed write as a tool result. A lock that outlasted the wait is a temporary state the
/// agent can act on (retry later), so it comes back as a soft result rather than a JSON-RPC
/// error; anything else stays an internal error.
fn write_error(e: anyhow::Error) -> Result<CallToolResult, ErrorData> {
    if crate::db::is_busy(&e) {
        Ok(text_result(INDEX_BUSY_NOTICE.to_string()))
    } else {
        Err(internal(e))
    }
}

/// Shown instead of an empty result when a semantic tool can't run because the embedding
/// model isn't on this machine (this server never downloads it). An empty list would read
/// as "nothing found" (for check_decision, "no conflict"), which would be wrong.
const SEMANTIC_UNAVAILABLE_NOTICE: &str = "Semantic recall is unavailable: the on-device \
embedding model hasn't been downloaded yet (open the Callimachus app and build the semantic \
index once). This is not an empty result. Use search_threads or search_current_project for \
keyword search meanwhile.";

/// The result for a semantic tool whose query produced no vector: the notice when the model
/// is missing, otherwise an empty list (a blank query).
fn no_query_vector() -> CallToolResult {
    if embed::model_is_cached() {
        text_result("[]".to_string())
    } else {
        text_result(SEMANTIC_UNAVAILABLE_NOTICE.to_string())
    }
}

/// A plain-text tool result, scrubbed of credential-shaped text.
///
/// Every tool answers through here or [`json_result`], and both redact on the way out: the
/// index stores transcripts raw, so titles, snippets, fact text and error signatures would
/// otherwise reach the calling agent verbatim. Text that was already packed (and redacted)
/// passes through unchanged.
fn text_result(mut text: String) -> CallToolResult {
    if let std::borrow::Cow::Owned(r) = context::redact_secrets(&text) {
        text = r;
    }
    CallToolResult::success(vec![ContentBlock::text(text)])
}

/// Compact JSON tool result with every string redacted ([`context::to_redacted_json`]).
/// Never pretty-printed: indentation was 15-20% of every response and models read minified
/// JSON just as well.
fn json_result<T: Serialize>(value: &T) -> Result<CallToolResult, ErrorData> {
    let json = context::to_redacted_json(value).map_err(internal)?;
    Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
}

/// A caller's `limit`, or `default`, kept within 1..=max.
fn clamp_limit(requested: Option<u32>, default: u32, max: u32) -> u32 {
    requested.unwrap_or(default).clamp(1, max)
}

/// A caller's transcript `budget` in chars, or the default, kept within the min/max.
fn clamp_budget(requested: Option<u32>) -> usize {
    requested
        .unwrap_or(TRANSCRIPT_BUDGET_DEFAULT)
        .clamp(TRANSCRIPT_BUDGET_MIN, TRANSCRIPT_BUDGET_MAX) as usize
}

fn slim_input_schema(schema: &JsonObject) -> Arc<JsonObject> {
    let mut slim = schema.clone();
    slim.remove("$schema");
    slim.remove("title");
    Arc::new(slim)
}

/// Collapse ranked message hits into one entry per thread, preserving rank order, and keep the
/// first `limit` threads. Later hits from an already-listed thread add a snippet to it.
fn group_by_thread(hits: Vec<search::SearchHit>, limit: usize) -> Vec<ThreadMatch> {
    let mut out: Vec<ThreadMatch> = Vec::with_capacity(limit);
    let mut slot: HashMap<i64, usize> = HashMap::new();
    for h in hits {
        // Snippets come straight from the FTS index: strip its \u{1}/\u{2} match sentinels
        // (the GUI turns them into highlights; an agent would see raw control bytes) and scrub
        // credentials, since the index is not redacted at the source (pack_thread-based tools are).
        let plain = h.snippet.replace(['\u{1}', '\u{2}'], "");
        let snippet = format!(
            "{}: {}",
            h.role,
            crate::context::redact_secrets(plain.trim())
        );
        if let Some(&i) = slot.get(&h.thread_id) {
            out[i].snippets.push(snippet);
            continue;
        }
        if out.len() >= limit {
            continue;
        }
        slot.insert(h.thread_id, out.len());
        out.push(ThreadMatch {
            thread_id: h.thread_id,
            source: h.source,
            title: h.title,
            project_path: h.project_path,
            ts: h.ts,
            snippets: vec![snippet],
        });
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum MemoryFormat {
    Markdown,
    Json,
}

impl MemoryFormat {
    fn parse(raw: Option<&str>) -> Result<Self, ErrorData> {
        match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            None | Some("") | Some("markdown") | Some("md") => Ok(Self::Markdown),
            Some("json") => Ok(Self::Json),
            Some(other) => Err(ErrorData::invalid_params(
                format!("unknown format `{other}`: use \"markdown\" or \"json\""),
                None,
            )),
        }
    }
}

/// Which memory kinds a project_memory call asked for. An empty `kinds` list means all three.
#[derive(Debug, Clone, Copy, PartialEq)]
struct MemoryKinds {
    decisions: bool,
    gotchas: bool,
    todos: bool,
}

impl MemoryKinds {
    fn parse(raw: &[String]) -> Result<Self, ErrorData> {
        let mut kinds = Self {
            decisions: false,
            gotchas: false,
            todos: false,
        };
        let mut any = false;
        for k in raw {
            let k = k.trim().to_ascii_lowercase();
            match k.as_str() {
                "" => continue,
                "decision" | "decisions" => kinds.decisions = true,
                "gotcha" | "gotchas" => kinds.gotchas = true,
                "todo" | "todos" | "open_todos" | "opentodos" => kinds.todos = true,
                other => {
                    return Err(ErrorData::invalid_params(
                        format!("unknown kind `{other}`: use decisions, gotchas or todos"),
                        None,
                    ))
                }
            }
            any = true;
        }
        if !any {
            kinds = Self {
                decisions: true,
                gotchas: true,
                todos: true,
            };
        }
        Ok(kinds)
    }

    /// Drop the kinds the caller didn't ask for, so neither renderer can emit them.
    fn retain_in(self, mem: &mut crate::knowledge::ProjectMemory) {
        if !self.decisions {
            mem.decisions.clear();
        }
        if !self.gotchas {
            mem.gotchas.clear();
        }
        if !self.todos {
            mem.open_todos.clear();
        }
    }
}

fn compact_memory(mem: &crate::knowledge::ProjectMemory, kinds: MemoryKinds) -> CompactMemory<'_> {
    fn facts(list: &[crate::knowledge::MemoryFact]) -> Vec<CompactFact<'_>> {
        list.iter()
            .map(|f| CompactFact {
                id: f.id,
                thread_id: f.thread_id,
                // Same scrub as the markdown path (export::fact_line) and list_open_todos.
                text: crate::context::redact_secrets(f.text.trim()),
                pinned: f.pinned,
            })
            .collect()
    }
    CompactMemory {
        project: &mem.project,
        thread_count: mem.thread_count,
        distilled_count: mem.distilled_count,
        pending_count: mem.pending_count,
        decisions: kinds.decisions.then(|| facts(&mem.decisions)),
        gotchas: kinds.gotchas.then(|| facts(&mem.gotchas)),
        open_todos: kinds.todos.then(|| facts(&mem.open_todos)),
    }
}

/// The markdown budget for project_memory: the shared MCP budget by default. An explicit
/// `limit` raises the per-kind caps to it and grows the char cap to match (~600 chars per row
/// of decision + gotcha + TODO), so asking for more actually returns more.
fn memory_budget(limit: Option<usize>) -> crate::export::MemoryBudget {
    let base = crate::export::MemoryBudget::MCP;
    match limit {
        None => base,
        Some(n) => crate::export::MemoryBudget {
            max_chars: base.max_chars.max(n * 600),
            max_decisions: n,
            max_gotchas: n,
            max_todos: n,
            ..base
        },
    }
}

fn empty_memory_message(mem: &crate::knowledge::ProjectMemory) -> String {
    format!(
        "No project memory yet for `{}` ({} threads indexed, {} distilled). Decisions and \
         gotchas appear after threads are distilled in the Callimachus app (Projects > Build \
         memory) or recorded with record_decision / record_gotcha.",
        mem.project, mem.thread_count, mem.distilled_count
    )
}

/// Trim a stored snapshot body to `budget` bytes (cut on char boundaries). A body is the
/// carry-forward memory block, a `---` divider, then the packed transcript. Snapshots are
/// frozen checkpoints, so instead of re-packing the (possibly since-changed) thread this keeps
/// the memory block (capped at a third of the budget) and elides the middle of the transcript,
/// favoring its tail: where the session left off is what a resume needs most.
fn fit_snapshot_body(body: &str, budget: usize) -> String {
    if body.len() <= budget {
        return body.to_string();
    }
    // pack_thread output always opens with the envelope tag; a body that starts with it has no
    // carry block, and any `---` inside it belongs to the transcript.
    let (carry, transcript) = if body.starts_with("<reference_thread") {
        ("", body)
    } else {
        body.split_once(SNAPSHOT_DIVIDER).unwrap_or(("", body))
    };

    let mut out = String::with_capacity(budget + 128);
    if !carry.is_empty() {
        let cap = budget / 3;
        if carry.len() <= cap {
            out.push_str(carry);
        } else {
            // Cut at a line break so a fact bullet isn't left half-written.
            let head = char_prefix(carry, cap);
            let head = head.rfind('\n').map_or(head, |i| &head[..i]);
            out.push_str(head);
            out.push_str("\n- … (more project memory trimmed to fit budget)");
        }
        out.push_str(SNAPSHOT_DIVIDER);
    }

    // Leave room for the elision marker.
    let room = budget.saturating_sub(out.len()).saturating_sub(96);
    if transcript.len() <= room {
        out.push_str(transcript);
        return out;
    }
    let head = char_prefix(transcript, room / 3);
    let tail = char_suffix(transcript, room - head.len());
    let elided = transcript[head.len()..transcript.len() - tail.len()]
        .chars()
        .count();
    out.push_str(head);
    out.push_str(&format!(
        "\n\n… {elided} chars of the transcript elided to fit budget …\n\n"
    ));
    out.push_str(tail);
    out
}

/// The longest prefix of `s` that is at most `max` bytes and ends on a char boundary.
fn char_prefix(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// The longest suffix of `s` that is at most `max` bytes and starts on a char boundary.
fn char_suffix(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// The project key for the server's cwd (`knowledge::cwd_project_key`, the resolver `cal`
/// and its hooks use), the one the tools scope to by default. None only when the cwd can't
/// be read.
fn cwd_project_key() -> Option<String> {
    crate::knowledge::cwd_project_key()
}

/// The project key for a tool's `project` argument, else (unset or blank) for the cwd.
fn project_key_or_cwd(project: Option<String>) -> String {
    match project.filter(|p| !p.trim().is_empty()) {
        Some(p) => crate::knowledge::project_key(&p),
        None => cwd_project_key().unwrap_or_default(),
    }
}

// Route through the router built (and slimmed) in `new()`. The macro's default,
// `Self::tool_router()`, would rebuild it on every call and serve the unslimmed schemas.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for Callimachus {
    fn get_info(&self) -> ServerConfig {
        // Implementation is #[non_exhaustive]; build via from_build_env then set fields.
        let mut server_info = Implementation::from_build_env();
        server_info.name = "callimachus".into();
        server_info.title = Some("Callimachus".into());
        server_info.version = env!("CARGO_PKG_VERSION").into();
        server_info.description =
            Some("Search the user's indexed AI agent conversation history".into());
        // The default is ProtocolVersion::LATEST, which has no initialize handshake; name the
        // handshake version this server answers a client it can't echo with.
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::LATEST_WITH_INITIALIZE)
            .with_server_info(server_info)
            .with_instructions(INSTRUCTIONS)
    }

    // Initialize-based revisions only: the handshake Claude Code, Codex and Cursor negotiate,
    // and the only lifecycle this server is tested on. rmcp 3's default list also offers
    // 2026-07-28 (per-request metadata, no initialize); a client probing with it gets these
    // versions back in the -32022 error and falls back to initialize.
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(ProtocolVersion::known_up_to(
            &ProtocolVersion::LATEST_WITH_INITIALIZE,
        ))
    }
}

/// Serve the MCP protocol over stdio against `conn`, until the client disconnects. Only
/// [`serve_stdio`] calls it, after opening the index and building the runtime.
async fn serve(conn: Connection) -> anyhow::Result<()> {
    let service = Callimachus::new(conn)
        .serve((tokio::io::stdin(), tokio::io::stdout()))
        .await?;
    service.waiting().await?;
    Ok(())
}

/// The whole stdio MCP server process, shared by both entry points (the `callimachus-mcp`
/// binary and the app's `--mcp` mode) so they can't drift: say so when there is no index
/// yet, open `index_path` as a sidecar, serve until the client disconnects, and return the
/// process exit code. `prog` prefixes the stderr lines, which MCP clients show in their
/// server log.
pub fn serve_stdio(prog: &str, index_path: &Path) -> i32 {
    if let Some(notice) = no_index_notice(prog, index_path) {
        eprintln!("{notice}");
    }
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("{prog}: starting the async runtime: {e}");
            return 1;
        }
    };
    let result = rt.block_on(async {
        // Writable: the server has write tools (complete_todo, record_*). WAL + busy_timeout
        // let it coexist with the desktop app's writer; the sidecar open also waits out an
        // index upgrade another process is running.
        let conn = crate::db::open_sidecar(index_path)?;
        serve(conn).await
    });
    // Don't wait on tokio's stdin reader: it is a blocking read that can't be cancelled, so a
    // plain drop could hang an exit that didn't come from stdin EOF. The process ends next.
    rt.shutdown_background();
    match result {
        Ok(()) => 0,
        // A lock held past the sidecar wait is almost always an upgrade in progress: say that
        // instead of SQLite's raw "database is locked".
        Err(e) if crate::db::is_busy(&e) => {
            eprintln!("{prog}: the index is busy (an upgrade may be running); try again shortly");
            1
        }
        Err(e) => {
            eprintln!("{prog}: {e:#}");
            1
        }
    }
}

/// The server-log line for a missing index, or None when `index_path` exists. Only the
/// desktop app indexes, so without it (an npx / MCP-registry install, or CALLIMACHUS_DB
/// pointing somewhere new) the server starts on an empty index; the tools also answer with
/// [`EMPTY_INDEX_NOTICE`] until threads show up. Worded for both entry points: the app's own
/// `--mcp` mode can hit this too, when it was never opened or CALLIMACHUS_DB moved the index.
fn no_index_notice(prog: &str, index_path: &Path) -> Option<String> {
    if index_path.exists() {
        return None;
    }
    Some(format!(
        "{prog}: no index at {}; starting with an empty one. The Callimachus desktop app builds \
         the index: open it to index your agent history (install it from \
         https://github.com/BetaBots-LLC/callimachus/releases/latest), or set CALLIMACHUS_DB to \
         an existing index.db.",
        index_path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::{source_id, upsert_thread, ParsedMessage, ParsedThread};
    use std::sync::atomic::AtomicU32;

    static N: AtomicU32 = AtomicU32::new(0);

    fn msg(role: &str, text: &str, ts: i64) -> ParsedMessage {
        ParsedMessage {
            role: role.into(),
            text: text.into(),
            tool_name: None,
            ts: Some(ts),
        }
    }

    /// A fresh, migrated temp DB with no threads.
    fn empty_db() -> Connection {
        let p = std::env::temp_dir().join(format!(
            "calli_mcp_{}_{}.db",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        crate::db::open(&p).unwrap()
    }

    fn seed(conn: &mut Connection, external_id: &str, title: &str, messages: Vec<ParsedMessage>) {
        let sid = source_id(conn, "claude_code").unwrap();
        let t = ParsedThread {
            external_id: external_id.into(),
            title: Some(title.into()),
            project_path: Some("/proj/app".into()),
            messages,
            ..Default::default()
        };
        upsert_thread(conn, sid, &t).unwrap();
    }

    /// A migrated temp DB seeded with one thread, wrapped in the MCP server. Exercises
    /// the real entry point: tool dispatch -> conn lock -> search/pack -> CallToolResult.
    fn seeded_server() -> Callimachus {
        let mut conn = empty_db();
        seed(
            &mut conn,
            "t1",
            "auth token refresh",
            vec![
                msg("user", "how do we refresh the auth token", 100),
                msg("assistant", "rotate the refresh token on a 401", 150),
            ],
        );
        Callimachus::new(conn)
    }

    /// The single text block of a tool result.
    fn text_of(res: &CallToolResult) -> String {
        assert_eq!(res.content.len(), 1, "expected one content block");
        res.content[0].as_text().expect("text content").text.clone()
    }

    #[test]
    fn busy_write_is_a_soft_result_and_other_errors_stay_errors() {
        let busy = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some("database is locked".into()),
        );
        let res = write_error(anyhow::Error::from(busy).context("saving")).expect("soft result");
        assert_eq!(text_of(&res), INDEX_BUSY_NOTICE);
        assert!(write_error(anyhow::anyhow!("no such table: facts")).is_err());
    }

    /// A fresh scratch dir under the temp dir (not under $HOME, so the home stop stays out of
    /// the way), canonicalized so keys compare equal on macOS's /var -> /private/var.
    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "calli_mcp_{tag}_{}_{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    #[test]
    fn project_keys_match_the_indexer_inside_a_worktree() {
        // A linked worktree's threads are indexed under its main repo. The old walk-up
        // stopped at the worktree's own `.git` file, so search_current_project matched none.
        let base = scratch_dir("wt");
        let main = base.join("repo");
        let wt_git = main.join(".git/worktrees/feature");
        std::fs::create_dir_all(&wt_git).unwrap();
        std::fs::write(wt_git.join("commondir"), "../..\n").unwrap();
        let wt = base.join("repo-feature");
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", wt_git.display())).unwrap();

        let k_main = crate::indexer::canonical_project(main.to_str().unwrap()).unwrap();
        assert_eq!(
            crate::knowledge::project_key(&wt.join("src").to_string_lossy()),
            k_main
        );
        assert_eq!(
            project_key_or_cwd(Some(wt.to_string_lossy().into_owned())),
            k_main
        );
        // An unset or blank argument falls back to the cwd's key, not to an empty project.
        let cwd_key = cwd_project_key().expect("cwd");
        assert_eq!(project_key_or_cwd(None), cwd_key);
        assert_eq!(project_key_or_cwd(Some("  ".into())), cwd_key);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn no_index_notice_only_when_the_index_is_missing() {
        let dir = scratch_dir("noindex");
        let path = dir.join("index.db");
        let notice = no_index_notice("callimachus --mcp", &path).expect("missing index");
        assert!(notice.starts_with("callimachus --mcp: no index at "));
        assert!(notice.contains(&path.display().to_string()));
        assert!(notice.contains("CALLIMACHUS_DB"));
        std::fs::write(&path, b"").unwrap();
        assert_eq!(no_index_notice("callimachus-mcp", &path), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn search_threads_tool_surfaces_seeded_thread() {
        let srv = seeded_server();
        let res = srv
            .search_threads(Parameters(SearchArgs {
                query: "auth".into(),
                sources: vec![],
                hybrid: false,
                include_subagents: false,
                limit: Some(5),
            }))
            .await
            .expect("search tool should succeed");
        let body = text_of(&res);
        assert!(
            body.contains("auth token refresh"),
            "search should return the seeded thread; got: {body}"
        );
    }

    #[tokio::test]
    async fn search_results_are_compact_and_grouped_by_thread() {
        let srv = seeded_server();
        // Both seeded messages match "refresh": they must collapse into one thread entry.
        let res = srv
            .search_threads(Parameters(SearchArgs {
                query: "refresh".into(),
                sources: vec![],
                hybrid: false,
                include_subagents: false,
                limit: None,
            }))
            .await
            .unwrap();
        let body = text_of(&res);
        assert!(!body.contains('\n'), "results must be compact JSON: {body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let arr = v.as_array().expect("array of threads");
        assert_eq!(arr.len(), 1, "one entry per thread: {body}");
        assert_eq!(arr[0]["threadId"], 1);
        assert!(arr[0]["snippets"].as_array().unwrap().len() >= 2);
    }

    #[tokio::test]
    async fn get_thread_tool_packs_transcript() {
        let srv = seeded_server();
        // Single thread in a fresh DB has id 1.
        let res = srv
            .get_thread(Parameters(GetThreadArgs {
                thread_id: 1,
                budget: None,
            }))
            .await
            .expect("get_thread tool should succeed");
        let body = text_of(&res);
        assert!(
            body.contains("refresh"),
            "packed transcript should contain message text; got: {body}"
        );
    }

    #[tokio::test]
    async fn get_thread_honors_budget() {
        let mut conn = empty_db();
        let messages = (0..40)
            .map(|i| {
                let role = if i % 2 == 0 { "user" } else { "assistant" };
                msg(role, &format!("turn {i} {}", "lorem ipsum ".repeat(400)), i)
            })
            .collect();
        seed(&mut conn, "big", "a very long thread", messages);
        let srv = Callimachus::new(conn);

        let small = text_of(
            &srv.get_thread(Parameters(GetThreadArgs {
                thread_id: 1,
                budget: Some(5_000),
            }))
            .await
            .unwrap(),
        );
        assert!(
            small.len() <= 5_000 + 500,
            "budget 5000 gave {}",
            small.len()
        );

        let default = text_of(
            &srv.get_thread(Parameters(GetThreadArgs {
                thread_id: 1,
                budget: None,
            }))
            .await
            .unwrap(),
        );
        assert!(
            default.len() <= TRANSCRIPT_BUDGET_DEFAULT as usize + 500,
            "default budget gave {}",
            default.len()
        );
        assert!(default.len() > small.len(), "a bigger budget returns more");
    }

    #[tokio::test]
    async fn snapshot_tools_roundtrip() {
        let srv = seeded_server();
        // Checkpoint the seeded thread (id 1 in a fresh DB).
        let created = srv
            .snapshot_session(Parameters(SnapshotArgs {
                thread_id: 1,
                label: None,
            }))
            .await
            .expect("snapshot_session should succeed");
        assert!(
            text_of(&created).contains("auth token refresh"),
            "snapshot label defaults to the thread title"
        );

        // It shows up in the list.
        let listed = srv
            .list_snapshots(Parameters(ListSnapshotsArgs {
                project: None,
                limit: None,
            }))
            .await
            .expect("list_snapshots should succeed");
        assert!(text_of(&listed).contains("auth token refresh"));

        // Loading returns the checkpoint body (the packed transcript).
        let loaded = srv
            .load_snapshot(Parameters(LoadSnapshotArgs {
                id: 1,
                budget: None,
            }))
            .await
            .expect("load_snapshot should succeed");
        assert!(
            text_of(&loaded).contains("refresh"),
            "loaded checkpoint should contain the transcript"
        );
    }

    #[test]
    fn fit_snapshot_body_keeps_memory_and_transcript_tail() {
        let carry = format!("## Project memory (app)\n\n{}", "- a decision\n".repeat(50));
        let transcript = format!(
            "<reference_thread source=\"claude_code\" title=\"t\" project=\"/p\">\n### user\nSTART {}END\n</reference_thread>",
            "é middle text ".repeat(2_000)
        );
        let body = format!("{carry}{SNAPSHOT_DIVIDER}{transcript}");

        let fitted = fit_snapshot_body(&body, 4_000);
        assert!(fitted.len() <= 4_000, "len {}", fitted.len());
        assert!(
            fitted.starts_with("## Project memory"),
            "memory block first"
        );
        assert!(fitted.contains("START"), "transcript head kept");
        assert!(
            fitted.ends_with("</reference_thread>"),
            "transcript tail (closing tag) kept"
        );
        assert!(fitted.contains("elided to fit budget"));

        // Bodies that already fit come back untouched.
        assert_eq!(fit_snapshot_body("short", 4_000), "short");
        // A transcript-only body never treats a `---` inside the transcript as the divider.
        let only = format!("{transcript}{SNAPSHOT_DIVIDER}trailing");
        let fitted = fit_snapshot_body(&only, 3_000);
        assert!(fitted.starts_with("<reference_thread"));
    }

    #[tokio::test]
    async fn project_memory_formats_and_kinds() {
        let mut conn = empty_db();
        seed(
            &mut conn,
            "t1",
            "memory thread",
            vec![msg("user", "hello", 1), msg("assistant", "hi", 2)],
        );
        // record_fact needs no embedding model; the MCP record_* path would also embed.
        crate::knowledge::record_fact(
            &conn,
            "/proj/app",
            "decision",
            "Use WAL mode for the index",
            Some("concurrent readers"),
            10,
        )
        .unwrap();
        crate::knowledge::record_fact(&conn, "/proj/app", "gotcha", "fts5 needs rebuild", None, 11)
            .unwrap();
        crate::knowledge::record_fact(
            &conn,
            "/proj/app",
            "gotcha",
            "rotated token ghp_abcdefghijklmnopqrstuvwxyz0123456789 after the leak",
            None,
            12,
        )
        .unwrap();
        let srv = Callimachus::new(conn);

        // JSON: compact, no per-fact title/createdAt, only the requested kinds.
        let json = text_of(
            &srv.project_memory(Parameters(ProjectMemoryArgs {
                project: Some("/proj/app".into()),
                format: Some("json".into()),
                limit: None,
                kinds: vec!["decisions".into()],
            }))
            .await
            .unwrap(),
        );
        assert!(!json.contains('\n'), "compact JSON: {json}");
        assert!(
            !json.contains("createdAt") && !json.contains("\"title\""),
            "{json}"
        );
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["decisions"][0]["text"], "Use WAL mode for the index");
        assert!(
            v.get("gotchas").is_none(),
            "unrequested kind omitted: {json}"
        );

        // JSON redacts fact text exactly like the markdown path does.
        let gotchas = text_of(
            &srv.project_memory(Parameters(ProjectMemoryArgs {
                project: Some("/proj/app".into()),
                format: Some("json".into()),
                limit: None,
                kinds: vec!["gotchas".into()],
            }))
            .await
            .unwrap(),
        );
        assert!(!gotchas.contains("ghp_"), "secret leaked: {gotchas}");
        assert!(gotchas.contains("[REDACTED]"), "{gotchas}");

        // Markdown (the default) renders the facts as text, not JSON.
        let md = text_of(
            &srv.project_memory(Parameters(ProjectMemoryArgs {
                project: Some("/proj/app".into()),
                format: None,
                limit: None,
                kinds: vec![],
            }))
            .await
            .unwrap(),
        );
        assert!(
            !md.trim_start().starts_with('{'),
            "markdown by default: {md}"
        );
        assert!(
            md.contains("Use WAL mode") && md.contains("fts5 needs rebuild"),
            "{md}"
        );

        // Bad arguments are rejected, not silently ignored.
        for (format, kinds) in [(Some("yaml"), vec![]), (None, vec!["facts".to_string()])] {
            let res = srv
                .project_memory(Parameters(ProjectMemoryArgs {
                    project: Some("/proj/app".into()),
                    format: format.map(str::to_string),
                    limit: None,
                    kinds,
                }))
                .await;
            assert!(res.is_err(), "invalid format/kinds must error");
        }
    }

    #[tokio::test]
    async fn issue_recent_and_search_tools_redact_a_planted_key() {
        // The index stores transcripts raw, so the key must be scrubbed on the way out: in the
        // error signature recurring_issues returns and the title recent_threads lists.
        let key = "sk-ant-api03-AbCdEfGhIjKlMnOpQrStUv0123456789";
        let now = chrono::Utc::now().timestamp();
        let err = format!("Error: invalid x-api-key {key}");
        let mut conn = empty_db();
        seed(
            &mut conn,
            "leaky",
            &format!("debug auth with {key}"),
            vec![msg("tool", &err, now - 60), msg("tool", &err, now)],
        );
        let srv = Callimachus::new(conn);

        let issues = srv
            .recurring_issues(Parameters(RecurringIssuesArgs {
                project: Some("*".into()),
                since_days: None,
                limit: None,
            }))
            .await
            .unwrap();
        let issues = text_of(&issues);
        assert!(issues.contains("invalid x-api-key [REDACTED]"), "{issues}");

        let recent = srv
            .recent_threads(Parameters(RecentArgs {
                sources: vec![],
                project: None,
                starred: None,
                tags: vec![],
                limit: None,
            }))
            .await
            .unwrap();
        let recent = text_of(&recent);
        assert!(recent.contains("debug auth with [REDACTED]"), "{recent}");

        // Search results share the exit, titles included (snippets were already scrubbed).
        let found = srv
            .search_threads(Parameters(SearchArgs {
                query: "invalid".into(),
                sources: vec![],
                hybrid: false,
                include_subagents: false,
                limit: None,
            }))
            .await
            .unwrap();
        let found = text_of(&found);
        assert!(found.contains("debug auth with [REDACTED]"), "{found}");

        for body in [&issues, &recent, &found] {
            assert!(!body.contains("sk-ant-api03"), "{body}");
            serde_json::from_str::<serde_json::Value>(body).expect("still valid JSON");
        }
    }

    #[tokio::test]
    async fn empty_index_returns_actionable_notice() {
        let srv = Callimachus::new(empty_db());
        let search = text_of(
            &srv.search_threads(Parameters(SearchArgs {
                query: "anything".into(),
                sources: vec![],
                hybrid: false,
                include_subagents: false,
                limit: None,
            }))
            .await
            .unwrap(),
        );
        let todos = text_of(
            &srv.list_open_todos(Parameters(ListTodosArgs {
                query: None,
                project: None,
                source: None,
                limit: None,
            }))
            .await
            .unwrap(),
        );
        let memory = text_of(
            &srv.project_memory(Parameters(ProjectMemoryArgs {
                project: None,
                format: None,
                limit: None,
                kinds: vec![],
            }))
            .await
            .unwrap(),
        );
        for body in [search, todos, memory] {
            assert!(
                body.starts_with("No Callimachus index yet"),
                "empty index should explain itself, got: {body}"
            );
            assert!(body.contains("github.com/BetaBots-LLC/callimachus"));
        }

        // A synthetic "Recorded memory" thread doesn't count as an indexed history.
        crate::knowledge::record_fact(&srv.lock().unwrap(), "/proj/x", "gotcha", "g", None, 1)
            .unwrap();
        assert!(srv.empty_index_notice().unwrap().is_some());
    }

    /// Clients negotiate over initialize: a version this server knows is echoed, and one
    /// without a handshake (2026-07-28) or one it doesn't know gets the newest handshake
    /// version. Pinned here because rmcp's defaults moved past initialize in 3.x.
    #[test]
    fn initialize_echoes_handshake_versions_and_falls_back_for_newer_ones() {
        use rmcp::model::{ClientCapabilities, InitializeRequestParams};
        let srv = Callimachus::new(empty_db());
        let negotiated = |v: ProtocolVersion| {
            let req = InitializeRequestParams::new(
                ClientCapabilities::default(),
                Implementation::new("test-client", "1.0"),
            )
            .with_protocol_version(v);
            srv.negotiate_initialize(&req).unwrap().protocol_version
        };
        for v in [
            ProtocolVersion::V_2024_11_05,
            ProtocolVersion::V_2025_03_26,
            ProtocolVersion::V_2025_06_18,
            ProtocolVersion::V_2025_11_25,
        ] {
            assert_eq!(negotiated(v.clone()), v);
        }
        assert_eq!(
            negotiated(ProtocolVersion::V_2026_07_28),
            ProtocolVersion::V_2025_11_25
        );
        let unknown: ProtocolVersion = serde_json::from_str("\"2099-01-01\"").unwrap();
        assert_eq!(negotiated(unknown), ProtocolVersion::V_2025_11_25);
        let info = srv.get_info();
        assert!(info.capabilities.tools.is_some());
        assert_eq!(info.instructions.as_deref(), Some(INSTRUCTIONS));
    }

    #[test]
    fn limits_are_clamped() {
        assert_eq!(clamp_limit(None, 10, 50), 10);
        assert_eq!(clamp_limit(Some(0), 10, 50), 1);
        assert_eq!(clamp_limit(Some(10_000), 10, 50), 50);
        assert_eq!(clamp_budget(None), TRANSCRIPT_BUDGET_DEFAULT as usize);
        assert_eq!(clamp_budget(Some(1)), TRANSCRIPT_BUDGET_MIN as usize);
        assert_eq!(clamp_budget(Some(u32::MAX)), TRANSCRIPT_BUDGET_MAX as usize);
    }

    /// The tool surface is paid for on every session by clients that load all tools up front,
    /// so keep it lean: short descriptions, no schema boilerplate, annotations on every tool.
    #[test]
    fn tool_surface_stays_lean_and_annotated() {
        let srv = Callimachus::new(empty_db());
        let tools = srv.tool_router.list_all();
        let writes = [
            "complete_todo",
            "record_decision",
            "record_gotcha",
            "snapshot_session",
        ];
        let mut total = 0;
        for t in &tools {
            let desc = t.description.as_deref().unwrap_or("");
            assert!(!desc.is_empty(), "{} has no description", t.name);
            assert!(
                desc.chars().count() <= 220,
                "{} description is {} chars (max 220)",
                t.name,
                desc.chars().count()
            );
            total += desc.chars().count();

            assert!(!t.input_schema.contains_key("$schema"), "{} schema", t.name);
            assert!(!t.input_schema.contains_key("title"), "{} schema", t.name);

            let ann = t
                .annotations
                .as_ref()
                .unwrap_or_else(|| panic!("{} has no annotations", t.name));
            let name: &str = &t.name;
            let is_write = writes.contains(&name);
            assert_eq!(
                ann.read_only_hint,
                Some(!is_write),
                "{} readOnlyHint",
                t.name
            );
            if is_write {
                assert_eq!(
                    ann.destructive_hint,
                    Some(false),
                    "{} destructiveHint",
                    t.name
                );
            }
        }
        assert!(
            total <= 4_500,
            "tool descriptions total {total} chars (max 4500)"
        );
        assert!(
            INSTRUCTIONS.chars().count() <= 400,
            "instructions are {} chars (max 400)",
            INSTRUCTIONS.chars().count()
        );
    }

    /// Every tool must be documented where agents and users learn about it: the bundled
    /// /recall skill and the README. Catches the next `recurring_issues`-style drift.
    #[test]
    fn docs_list_every_tool() {
        let names: Vec<String> = Callimachus::new(empty_db())
            .tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        let docs = [
            concat!(env!("CARGO_MANIFEST_DIR"), "/resources/recall/SKILL.md"),
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../../README.md"),
        ];
        let mut checked = 0;
        for path in docs {
            // Skip docs that aren't present (e.g. building from a packaged crate).
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            checked += 1;
            for n in &names {
                // Backtick-anchored so `get_thread` isn't satisfied by `get_thread_knowledge`.
                assert!(
                    text.contains(&format!("`{n}`")) || text.contains(&format!("`{n}(")),
                    "MCP tool `{n}` is not documented in {path}"
                );
            }
        }
        assert!(
            checked > 0,
            "no docs found to cross-check: are the paths right?"
        );
    }
}
