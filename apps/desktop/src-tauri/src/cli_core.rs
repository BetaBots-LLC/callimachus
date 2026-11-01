//! `cal` CLI core — the search/recent/cat/stats/export logic, factored into the
//! library so it has two entry points: the standalone `cal` binary, and the
//! desktop app when invoked as `cal` (argv0) or with a cal subcommand. That lets
//! the installer symlink the app itself to `~/.local/bin/cal` — no separate
//! binary to ship. Reads the same local index.db as the GUI and MCP server.
//!
//! Set CALLIMACHUS_DB to point at a specific index.db; CALLIMACHUS_VAULT to a
//! default Obsidian vault for `cal export`.

use crate::{
    agent, context, cost, db, embed, export, gitlink, issues, knowledge, search, secrets, snapshot,
};
use rusqlite::Connection;

/// Subcommands that identify a `cal` invocation when the app is launched directly.
pub const COMMANDS: &[&str] = &[
    "search",
    "related",
    "recent",
    "cat",
    "show",
    "context",
    "stats",
    "export",
    "star",
    "tag",
    "tags",
    "todos",
    "knowledge",
    "distill",
    "decisions",
    "gotchas",
    "similar",
    "ask",
    "files",
    "memory",
    "done",
    "remember",
    "hook",
    "agents",
    "snapshot",
    "snapshots",
    "resume",
    "snapshot-hook",
    "check",
    "commits",
    "issues",
    "cost",
    "recall-now",
    "audit-pr",
    "version",
    "--version",
];

const USAGE: &str = "\
cal — search your indexed AI coding-agent history

USAGE:
  cal search <query…> [-s SOURCE] [-y|--hybrid] [--starred] [-t TAG] [-n LIMIT] [--json]
  cal related [<text…>] [-s SOURCE] [-p PROJECT] [--starred] [-t TAG] [-n LIMIT] [--json]
                                (text via args or stdin; semantic only)
  cal recent [-s SOURCE] [-p PROJECT] [--starred] [-t TAG] [-n LIMIT] [--json]
  cal cat <thread-id>            (aliases: show, context)
  cal stats [--json]
  cal export <thread-id> [--vault DIR] [--out FILE] [-S|--synthesize]
  cal star <thread-id> [--off]   star a thread (--off to unstar)
  cal tag <thread-id> [<tag…>]   set a thread's tags (no tags = clear them)
  cal tags [--json]              list all tags with thread counts
  cal todos [<query…>] [-p PROJECT] [-s SOURCE] [-n LIMIT] [--json]
                                open TODOs (optionally text-searched)
  cal knowledge <thread-id> [--json]
                                distilled summary/decisions/gotchas for a thread
  cal distill <thread-id>       extract knowledge for a thread (needs distillation
                                enabled in the app: local Ollama or an API key)
  cal decisions <query…> [-p PROJECT] [-n LIMIT] [--json]
  cal gotchas <query…> [-p PROJECT] [-n LIMIT] [--json]
                                semantic recall of distilled decisions/gotchas
  cal similar <task…> [-p PROJECT] [-n LIMIT] [--json]
                                prior SESSIONS where you did something similar
                                (the have-I-done-this-before guard)
  cal ask <question…>           answer a question from your history (RAG, cited)
  cal files <path>              threads that mention a file path (e.g. embed/mod.rs)
  cal memory [<project>] [-n LIMIT] [--json]
                                a project's distilled memory (decisions / gotchas /
                                open TODOs); defaults to the current repo
  cal done <todo-id>            mark an open TODO done (id from `cal todos`)
  cal remember <decision|gotcha> <text…> [--because WHY]
                                record a fact in the current repo's project memory
                                (-p PROJECT to override)
  cal check <proposal…> [-p PROJECT] [--json]
                                contradiction guard: settled decisions on this topic
                                (surfaces 'you already decided X because Y')
  cal commits [<repo>] [-n LIMIT] [--json]
                                infer + show which commits your threads produced
                                (run inside a git repo, or pass its path)
  cal audit-pr [<repo>] --changed-files a,b --shas s1,s2 [-n CAP]
                                one JSON bundle for an external PR auditor: per-commit
                                provenance (the session behind each sha), per-file prior
                                threads + reasoning, repo recurring errors + project memory
  cal issues [<project>] [-n LIMIT] [--json]
                                recurring errors you keep hitting across sessions
                                (last 180 days, most frequent first)
  cal cost [<project>] [-n LIMIT] [--json]
                                estimated $ spend by model + priciest threads
                                (needs a Reindex to capture token usage)
  cal hook [<project>] [-n PER_KIND] [--budget CHARS]
                                print the current repo's memory for injection (a Claude
                                Code SessionStart hook command; size-capped, ~6k chars)
  cal agents [<project>] [-o FILE]
                                write/refresh the memory block in AGENTS.md (or -o
                                CLAUDE.md) so any agent reading it opens with the memory
  cal snapshot <thread-id> [-l LABEL]
                                save a resumable checkpoint of a thread (packed transcript +
                                the project's carry-forward memory)
  cal snapshots [<project>] [-n LIMIT] [--json]
                                list saved snapshots (newest first)
  cal resume <snapshot-id> [-a AGENT]
                                relaunch an agent CLI seeded with a snapshot (cross-tool
                                handoff; AGENT defaults to claude, e.g. -a codex)
  cal version                    print the Callimachus version (also --version)
  cal help

OPTIONS:
  -s, --source SOURCE   filter by source kind (claude_code, codex, cursor,
                        gemini, qwen, goose, opencode, continue, cline, roo,
                        kilo, in_app)
  -p, --project PATH    substring-match the project path
      --starred         only starred threads (recent/related/search)
  -t, --tag TAG         only threads with this tag (repeatable)
  -l, --label LABEL     name for a `snapshot`
  -a, --agent AGENT     target agent CLI for `resume` (default claude)
      --because WHY      rationale for a `remember decision`
  -y, --hybrid          fuse keyword + on-device semantic search
  -n, --limit N         max results (default 20; todos 50, files 40; hook: per kind)
      --budget CHARS    max chars `hook` prints (default 6000)
  -V, --vault DIR       Obsidian vault dir for `export` (else CALLIMACHUS_VAULT)
  -o, --out FILE        write `export` output to FILE instead of a vault/stdout
  -S, --synthesize      prepend an LLM summary / decisions / gotchas / TODOs
                        to `export` (uses the first stored provider key)
      --json            machine-readable JSON output";

/// Hook targets Claude Code runs inside the agent loop (SessionStart, PreCompact,
/// UserPromptSubmit). They must never fail visibly: Claude Code shows a non-zero exit plus
/// stderr as a hook error on every session start or prompt.
const HOOK_COMMANDS: &[&str] = &["hook", "snapshot-hook", "recall-now"];

/// Run the CLI and map the result to a process exit code (printing errors). Used
/// by both the `cal` binary and the app's cal-mode dispatch.
pub fn run_and_exit(args: &[String]) -> i32 {
    if args
        .first()
        .is_some_and(|c| HOOK_COMMANDS.contains(&c.as_str()))
    {
        return run_hook_silently(args);
    }
    match run(args) {
        Ok(()) => 0,
        // The lock outlasted the sidecar wait: almost always an index upgrade or a large
        // cleanup in the app. Say that instead of SQLite's raw "database is locked".
        Err(e) if db::is_busy(&e) => {
            eprintln!("cal: the index is busy (an upgrade or a large cleanup may be running); try again shortly");
            1
        }
        Err(e) => {
            eprintln!("cal: {e}");
            1
        }
    }
}

/// Run a hook command best-effort: any error (no index, a locked DB, a schema mismatch
/// after an update, a bad payload) and even a panic exits 0 with nothing on stderr, so the
/// agent carries on. Interactive users who want to see errors use `cal memory`.
fn run_hook_silently(args: &[String]) -> i32 {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let _ = std::panic::catch_unwind(|| run(args));
    std::panic::set_hook(previous);
    0
}

pub fn run(args: &[String]) -> anyhow::Result<()> {
    let Some((cmd, rest)) = args.split_first() else {
        println!("{USAGE}");
        return Ok(());
    };
    match cmd.as_str() {
        "search" => cmd_search(rest),
        "related" => cmd_related(rest),
        "recent" => cmd_recent(rest),
        "cat" | "show" | "context" => cmd_cat(rest),
        "stats" => cmd_stats(rest),
        "export" => cmd_export(rest),
        "star" => cmd_star(rest),
        "tag" => cmd_tag(rest),
        "tags" => cmd_tags(rest),
        "todos" => cmd_todos(rest),
        "knowledge" => cmd_knowledge(rest),
        "distill" => cmd_distill(rest),
        "decisions" => cmd_recall(rest, "decision"),
        "gotchas" => cmd_recall(rest, "gotcha"),
        "similar" => cmd_similar(rest),
        "ask" => cmd_ask(rest),
        "files" => cmd_files(rest),
        "memory" => cmd_memory(rest),
        "done" => cmd_done(rest),
        "remember" => cmd_remember(rest),
        "hook" => cmd_hook(rest),
        "agents" => cmd_agents(rest),
        "snapshot" => cmd_snapshot(rest),
        "snapshots" => cmd_snapshots(rest),
        "resume" => cmd_resume(rest),
        "snapshot-hook" => cmd_snapshot_hook(rest),
        "check" => cmd_check(rest),
        "commits" => cmd_commits(rest),
        "issues" => cmd_issues(rest),
        "cost" => cmd_cost(rest),
        "recall-now" => cmd_recall_now(rest),
        "audit-pr" => cmd_audit_pr(rest),
        // A cheap, explicit signature for tools probing whether a `cal` on PATH is ours
        // (the VS Code extension; the system calendar's `cal` prints a calendar instead).
        "version" | "--version" | "-v" => {
            println!("callimachus {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "help" | "-h" | "--help" => {
            println!("{USAGE}");
            Ok(())
        }
        other => anyhow::bail!("unknown command '{other}'. Run `cal help`."),
    }
}

/// Minimal flag parser: pulls known flags out, leaves the rest as positionals.
#[derive(Default)]
struct Opts {
    source: Option<String>,
    project: Option<String>,
    hybrid: bool,
    json: bool,
    limit: Option<u32>,
    vault: Option<String>,
    out: Option<String>,
    synthesize: bool,
    starred: bool,
    off: bool,
    tags: Vec<String>,
    label: Option<String>,
    agent: Option<String>,
    because: Option<String>,
    /// `hook`: max chars to print.
    budget: Option<usize>,
    positional: Vec<String>,
}

fn parse(args: &[String]) -> anyhow::Result<Opts> {
    let mut o = Opts::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-s" | "--source" => o.source = Some(next(&mut it, "--source")?),
            "-p" | "--project" => o.project = Some(next(&mut it, "--project")?),
            "-n" | "--limit" => {
                o.limit = Some(
                    next(&mut it, "--limit")?
                        .parse()
                        .map_err(|_| anyhow::anyhow!("--limit needs a number"))?,
                )
            }
            "-V" | "--vault" => o.vault = Some(next(&mut it, "--vault")?),
            "-o" | "--out" => o.out = Some(next(&mut it, "--out")?),
            "-y" | "--hybrid" => o.hybrid = true,
            "-S" | "--synthesize" => o.synthesize = true,
            "-t" | "--tag" => o.tags.push(next(&mut it, "--tag")?),
            "-l" | "--label" => o.label = Some(next(&mut it, "--label")?),
            "-a" | "--agent" => o.agent = Some(next(&mut it, "--agent")?),
            "--because" => o.because = Some(next(&mut it, "--because")?),
            "--budget" => {
                o.budget = Some(
                    next(&mut it, "--budget")?
                        .parse()
                        .map_err(|_| anyhow::anyhow!("--budget needs a number of chars"))?,
                )
            }
            "--starred" => o.starred = true,
            "--off" => o.off = true,
            "--json" => o.json = true,
            s if s.starts_with('-') && s.len() > 1 => anyhow::bail!("unknown flag '{s}'"),
            _ => o.positional.push(a.clone()),
        }
    }
    Ok(o)
}

fn next(it: &mut std::slice::Iter<'_, String>, flag: &str) -> anyhow::Result<String> {
    it.next()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("{flag} needs a value"))
}

fn open_db() -> anyhow::Result<Connection> {
    // Read-only: the desktop app is the single writer; `cal` only queries, so it
    // runs safely while the app writes (WAL readers never block). open_readonly
    // returns a clear "no index" error if the app hasn't built one yet.
    db::open_readonly(&db::default_index_path())
}

/// Writable connection for the few `cal` subcommands that mutate (star/tag/distill).
/// A read-only connection rejects writes with SQLITE_READONLY; WAL still lets this
/// coexist with the app's writes (busy_timeout waits out the rare overlap). The sidecar
/// open also waits out an index upgrade another process is running.
fn open_db_write() -> anyhow::Result<Connection> {
    db::open_sidecar(&db::default_index_path())
}

/// How long a hook waits for the index lock. Claude Code kills our hooks after 10-15s (see
/// the HookSpecs in integration.rs), so the sidecar's long wait would only get the hook
/// killed mid-run; giving up first lets `run_hook_silently` exit 0 quietly.
const HOOK_DB_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// [`open_db_write`] for a hook, bounded by [`HOOK_DB_WAIT`].
fn open_hook_db_write() -> anyhow::Result<Connection> {
    db::open_with_busy_timeout(&db::default_index_path(), HOOK_DB_WAIT)
}

fn filters(o: &Opts) -> search::SearchFilters {
    search::SearchFilters {
        sources: o.source.clone().into_iter().collect(),
        project: o.project.clone(),
        hybrid: o.hybrid,
        limit: Some(o.limit.unwrap_or(20)),
        starred: if o.starred { Some(true) } else { None },
        tags: o.tags.clone(),
        ..Default::default()
    }
}

/// The CLI's embedder. It never downloads: `cal` runs inside agents' shells with their
/// working directory, and a 128 MB model download must not start from there. Without the
/// model the app downloaded, semantic features fall back to keyword results or come back
/// empty, and this says so once on stderr (stdout stays clean for `--json`).
fn cli_embedder() -> embed::Embedder {
    if !embed::model_is_cached() {
        eprintln!(
            "note: semantic search is unavailable until the embedding model is downloaded \
             (open Callimachus and build the semantic index); using keyword matches only"
        );
    }
    embed::Embedder::cached_only()
}

fn cmd_search(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    if o.positional.is_empty() {
        anyhow::bail!("search needs a query. e.g. `cal search \"vector index\"`");
    }
    let query = o.positional.join(" ");
    let conn = open_db()?;
    let f = filters(&o);
    let hits = if o.hybrid {
        let embedder = cli_embedder();
        search::hybrid(&conn, &embedder, &query, &f)?
    } else {
        search::search(&conn, &query, &f)?
    };

    if o.json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
        return Ok(());
    }
    if hits.is_empty() {
        eprintln!("no matches for {query:?}");
        return Ok(());
    }
    for h in &hits {
        let title = h.title.as_deref().unwrap_or("(untitled)");
        println!("[{}] {:<11} {}", h.thread_id, h.source, title);
        println!("    {}", strip_marks(&h.snippet));
    }
    Ok(())
}

fn cmd_related(args: &[String]) -> anyhow::Result<()> {
    use std::io::Read;
    let o = parse(args)?;
    // Context text comes from the positional args, or stdin when none are given.
    let mut text = if o.positional.is_empty() {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        buf
    } else {
        o.positional.join(" ")
    };
    if text.trim().is_empty() {
        anyhow::bail!("related needs context text — pass it as an argument or pipe it on stdin");
    }
    // bge-small caps around 512 tokens; cap input so embedding stays cheap.
    if text.chars().count() > 1500 {
        text = text.chars().take(1500).collect();
    }

    let conn = open_db()?;
    let embedder = cli_embedder();
    let rows = search::related(&conn, &embedder, &text, &filters(&o))?;

    if o.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        eprintln!("no related threads (is the index embedded yet? open the app once)");
        return Ok(());
    }
    for t in &rows {
        let title = t.title.as_deref().unwrap_or("(untitled)");
        println!(
            "[{}] {:<11} {}  ({} msgs)",
            t.id, t.source, title, t.message_count
        );
    }
    Ok(())
}

fn cmd_recent(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    let conn = open_db()?;
    let rows = search::recent_threads(&conn, &filters(&o))?;

    if o.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        eprintln!("no threads indexed yet");
        return Ok(());
    }
    for t in &rows {
        let title = t.title.as_deref().unwrap_or("(untitled)");
        println!(
            "[{}] {:<11} {}  ({} msgs · {})",
            t.id,
            t.source,
            title,
            t.message_count,
            fmt_time(t.updated_at)
        );
    }
    Ok(())
}

fn thread_id_arg(o: &Opts, cmd: &str) -> anyhow::Result<i64> {
    o.positional
        .first()
        .ok_or_else(|| anyhow::anyhow!("{cmd} needs a thread id. e.g. `cal {cmd} 42`"))?
        .parse()
        .map_err(|_| anyhow::anyhow!("thread id must be a number"))
}

fn cmd_star(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    let id = thread_id_arg(&o, "star")?;
    let conn = open_db_write()?;
    search::set_star(&conn, id, !o.off)?;
    eprintln!(
        "thread {id} {}",
        if o.off { "unstarred" } else { "starred" }
    );
    Ok(())
}

fn cmd_tag(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    let id = thread_id_arg(&o, "tag")?;
    // Tags are the positionals after the id; passing none clears the thread's tags.
    let tags: Vec<String> = o.positional[1..].to_vec();
    let mut conn = open_db_write()?;
    let now = chrono::Utc::now().timestamp();
    search::set_thread_tags(&mut conn, id, &tags, now)?;
    let current = search::thread_tags(&conn, id)?;
    if current.is_empty() {
        eprintln!("thread {id}: tags cleared");
    } else {
        eprintln!("thread {id}: {}", current.join(", "));
    }
    Ok(())
}

fn cmd_tags(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    let conn = open_db()?;
    let tags = search::list_tags(&conn)?;
    if o.json {
        println!("{}", serde_json::to_string_pretty(&tags)?);
        return Ok(());
    }
    if tags.is_empty() {
        eprintln!("no tags yet");
        return Ok(());
    }
    for (tag, n) in &tags {
        println!("{n:>4}  {tag}");
    }
    Ok(())
}

fn cmd_todos(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    let conn = open_db()?;
    let query = o.positional.join(" ");
    let query = (!query.trim().is_empty()).then_some(query.as_str());
    let todos = knowledge::list_open_todos(
        &conn,
        query,
        o.project.as_deref(),
        o.source.as_deref(),
        o.limit.unwrap_or(50) as i64,
    )?;
    if o.json {
        println!("{}", serde_json::to_string_pretty(&todos)?);
        return Ok(());
    }
    if todos.is_empty() {
        eprintln!("no open todos found");
        return Ok(());
    }
    for t in &todos {
        let title = t.title.as_deref().unwrap_or("untitled");
        println!("• {}", t.text);
        println!("    {} · {} · thread {}", t.source, title, t.thread_id);
    }
    Ok(())
}

fn print_knowledge(k: &knowledge::ThreadKnowledge) {
    if let Some(s) = &k.summary {
        println!("Summary: {s}\n");
    }
    if !k.decisions.is_empty() {
        println!("Decisions:");
        for f in &k.decisions {
            println!("  • {}", f.text);
        }
        println!();
    }
    if !k.gotchas.is_empty() {
        println!("Gotchas:");
        for f in &k.gotchas {
            println!("  • {}", f.text);
        }
        println!();
    }
    if !k.todos.is_empty() {
        println!("TODOs:");
        for f in &k.todos {
            println!("  • {}", f.text);
        }
    }
    if k.stale {
        eprintln!("(stale — thread changed since it was distilled; re-run `cal distill`)");
    }
}

fn cmd_knowledge(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    let id = thread_id_arg(&o, "knowledge")?;
    let conn = open_db()?;
    let k = knowledge::get_thread_knowledge(&conn, id)?;
    if o.json {
        println!("{}", serde_json::to_string_pretty(&k)?);
        return Ok(());
    }
    if !k.extracted && k.todos.is_empty() {
        eprintln!("no knowledge for thread {id} yet — run `cal distill {id}`");
        return Ok(());
    }
    print_knowledge(&k);
    Ok(())
}

fn cmd_distill(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    let id = thread_id_arg(&o, "distill")?;
    let mut conn = open_db_write()?;
    let (provider, model, key, base_url) = crate::resolve_distill_engine(&conn)?;
    let packed = context::pack_thread(&conn, id, context::DEFAULT_BUDGET_CHARS)?
        .ok_or_else(|| anyhow::anyhow!("thread {id} not found"))?;
    eprintln!("distilling thread {id} with {provider}/{model}…");
    let rt = tokio::runtime::Runtime::new()?;
    let distilled = rt.block_on(agent::distill(
        &provider,
        &model,
        key.as_deref(),
        base_url.as_deref(),
        &packed,
    ))?;
    let now = chrono::Utc::now().timestamp();
    knowledge::store_distilled(&mut conn, id, &distilled, now)?;
    print_knowledge(&knowledge::get_thread_knowledge(&conn, id)?);
    Ok(())
}

/// The project key for the cwd: its git root, with worktrees resolved to their main repo
/// and the walk stopping at $HOME (`indexer::canonical_project`, the same normalization the
/// indexer stores), else the cwd itself. For memory lookups and write-backs.
fn cwd_project_root() -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    let raw = cwd.to_string_lossy().into_owned();
    crate::indexer::canonical_project(&raw).unwrap_or(raw)
}

/// The git WORKING directory containing `dir` (the nearest `.git` dir or worktree file),
/// else `dir`. Unlike the project key this keeps a worktree as itself, which is what git
/// commands and files written into the checkout need. Never climbs above $HOME, so a
/// `~/.git` dotfiles repo doesn't capture every directory under home.
fn git_root_of(dir: &std::path::Path) -> std::path::PathBuf {
    let home = dirs::home_dir().map(|h| std::fs::canonicalize(&h).unwrap_or(h));
    let start = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let mut cur = start.as_path();
    loop {
        if home.as_deref() == Some(cur) && cur != start.as_path() {
            break;
        }
        if cur.join(".git").exists() {
            return cur.to_path_buf();
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => break,
        }
    }
    start
}

/// `git_root_of` the cwd, as a string. For `cal commits` / `audit-pr` / `agents`.
fn cwd_git_root() -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    git_root_of(&cwd).to_string_lossy().into_owned()
}

fn cmd_memory(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    let raw = if o.positional.is_empty() {
        cwd_project_root()
    } else {
        o.positional.join(" ")
    };
    // Normalize to the canonical project key so we match however the threads were indexed.
    let project = crate::indexer::canonical_project(&raw).unwrap_or(raw);
    let conn = open_db()?;
    let mem = knowledge::get_project_memory(&conn, &project, o.limit.unwrap_or(40) as usize)?;
    if o.json {
        println!("{}", serde_json::to_string_pretty(&mem)?);
        return Ok(());
    }
    println!(
        "Project memory: {project}  ({}/{} threads distilled)",
        mem.distilled_count, mem.thread_count
    );
    let section = |title: &str, facts: &[knowledge::MemoryFact]| {
        if facts.is_empty() {
            return;
        }
        println!("\n{title}:");
        for f in facts {
            println!("  - {}", f.text.trim());
        }
    };
    section("Decisions", &mem.decisions);
    section("Gotchas", &mem.gotchas);
    section("Open TODOs", &mem.open_todos);
    if mem.decisions.is_empty() && mem.gotchas.is_empty() && mem.open_todos.is_empty() {
        eprintln!("\n(no distilled knowledge yet — distill this project's threads in the app)");
    }
    Ok(())
}

fn cmd_done(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    let id = thread_id_arg(&o, "done")?; // a TODO fact id from `cal todos --json`
    let conn = open_db_write()?;
    knowledge::set_todo_done(&conn, id, true)?;
    eprintln!("todo {id} marked done");
    Ok(())
}

/// Resolve the project arg (positional or cwd) to its canonical key.
fn project_arg(o: &Opts) -> String {
    let raw = if o.positional.is_empty() {
        cwd_project_root()
    } else {
        o.positional.join(" ")
    };
    crate::indexer::canonical_project(&raw).unwrap_or(raw)
}

/// `cal hook`: the SessionStart hook target. Prints the repo's memory, size-capped, for
/// Claude Code to inject. Reads the hook payload from stdin (when there is one) to decide
/// how much to send:
/// - `resume`: nothing, the resumed transcript already holds the earlier injection;
/// - `compact`: a short refresher (`MemoryBudget::COMPACT`), since compaction summarized
///   the original injection away;
/// - `startup` / `clear` / no payload: `MemoryBudget::HOOK`.
///
/// Also silent when the memory is already in context through a CLAUDE.md (or a file it
/// `@`-imports) holding the Callimachus managed block, when a launcher that already put
/// memory in the context file set `CALLIMACHUS_SKIP_HOOK=1`, and when there are no
/// decisions, gotchas or pinned TODOs. Errors are swallowed by `run_hook_silently`.
fn cmd_hook(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    let payload = read_hook_payload();
    let source = payload
        .as_ref()
        .and_then(|v| v.get("source"))
        .and_then(|x| x.as_str())
        .unwrap_or("startup");
    let Some(mut budget) = hook_budget(source) else {
        return Ok(());
    };
    if source == "startup" && std::env::var("CALLIMACHUS_SKIP_HOOK").as_deref() == Ok("1") {
        return Ok(());
    }
    if let Some(n) = o.limit {
        budget.max_decisions = n as usize;
        budget.max_gotchas = n as usize;
    }
    if let Some(chars) = o.budget {
        budget.max_chars = chars;
    }

    let cwd = hook_cwd(payload.as_ref());
    let project = if o.positional.is_empty() {
        let raw = cwd.to_string_lossy().into_owned();
        crate::indexer::canonical_project(&raw).unwrap_or(raw)
    } else {
        let raw = o.positional.join(" ");
        crate::indexer::canonical_project(&raw).unwrap_or(raw)
    };
    if claude_md_has_managed_block(&claude_md_dirs(&cwd)) {
        return Ok(());
    }
    // Never create an index from inside a hook.
    if !db::default_index_path().exists() {
        return Ok(());
    }
    let conn = open_db()?;
    let per_kind = budget
        .fetch_limit()
        .max(budget.max_decisions)
        .max(budget.max_gotchas);
    let mem = knowledge::get_project_memory(&conn, &project, per_kind)?;
    let pinned_todo = mem.open_todos.iter().any(|t| t.pinned);
    if mem.decisions.is_empty() && mem.gotchas.is_empty() && !pinned_todo {
        return Ok(());
    }
    println!(
        "{}",
        export::agent_memory_md_budgeted(&project, &mem, None, &budget)
    );
    Ok(())
}

/// The memory budget for a SessionStart `source`, or None to stay silent.
fn hook_budget(source: &str) -> Option<export::MemoryBudget> {
    match source {
        "resume" => None,
        "compact" => Some(export::MemoryBudget::COMPACT),
        _ => Some(export::MemoryBudget::HOOK),
    }
}

/// The session's working directory: the hook payload's `cwd`, else `CLAUDE_PROJECT_DIR`,
/// else the process cwd.
fn hook_cwd(payload: Option<&serde_json::Value>) -> std::path::PathBuf {
    payload
        .and_then(|v| v.get("cwd"))
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("CLAUDE_PROJECT_DIR").map(std::path::PathBuf::from))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

/// The JSON payload Claude Code writes to a hook's stdin, read without ever hanging: a TTY,
/// an empty pipe, a pipe nobody closes, or malformed JSON all yield None. The read runs on
/// a helper thread with a deadline because a blocked hook stalls the agent; if it is still
/// blocked when we finish, process exit ends it.
fn read_hook_payload() -> Option<serde_json::Value> {
    use std::io::{IsTerminal, Read};
    if std::io::stdin().is_terminal() {
        return None;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = String::new();
        // Cap the read: a giant pasted prompt shouldn't cost us memory; we bail on it.
        let _ = std::io::stdin()
            .lock()
            .take(8 * 1024 * 1024)
            .read_to_string(&mut buf);
        let _ = tx.send(buf);
    });
    let input = rx.recv_timeout(std::time::Duration::from_secs(2)).ok()?;
    serde_json::from_str(input.trim()).ok()
}

/// The dirs whose CLAUDE.md Claude Code loads for a session in `cwd`: cwd and each of its
/// ancestors, excluding the filesystem root (raw and resolved spellings). NOT the canonical
/// project key: for a linked worktree that is the main checkout, whose CLAUDE.md a session
/// in a sibling worktree never sees. A worktree nested under the repo
/// (`<repo>/.claude/worktrees/x`) still reaches the repo's file as an ancestor.
fn claude_md_dirs(cwd: &std::path::Path) -> Vec<std::path::PathBuf> {
    let resolved = std::fs::canonicalize(cwd).ok();
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    for start in std::iter::once(cwd).chain(resolved.as_deref()) {
        for dir in start.ancestors().filter(|d| d.parent().is_some()) {
            if !dir.as_os_str().is_empty() && !dirs.iter().any(|d| d == dir) {
                dirs.push(dir.to_path_buf());
            }
        }
    }
    dirs
}

/// Whether a CLAUDE.md that Claude Code auto-loads for this session (`<dir>/CLAUDE.md` or
/// `<dir>/.claude/CLAUDE.md` for each dir), or a file one of them `@`-imports, already
/// carries the Callimachus managed block, in which case the hook would inject the same
/// memory a second time.
fn claude_md_has_managed_block<P: AsRef<std::path::Path>>(dirs: &[P]) -> bool {
    let mut checked = std::collections::HashSet::new();
    for dir in dirs {
        let dir = dir.as_ref();
        for rel in ["CLAUDE.md", ".claude/CLAUDE.md"] {
            let path = dir.join(rel);
            if !checked.insert(path.clone()) {
                continue;
            }
            let Some(text) = read_small_text(&path) else {
                continue;
            };
            if text.contains(export::MANAGED_START) {
                return true;
            }
            let base = path.parent().unwrap_or(dir);
            for import in claude_md_imports(&text).into_iter().take(10) {
                let target = resolve_import(base, import);
                if read_small_text(&target).is_some_and(|t| t.contains(export::MANAGED_START)) {
                    return true;
                }
            }
        }
    }
    false
}

/// `@path` imports in a CLAUDE.md (`@AGENTS.md`, `@docs/notes.md`, `@~/x.md`). Only tokens
/// that look like paths, so `@someone` mentions are skipped.
fn claude_md_imports(text: &str) -> Vec<&str> {
    text.split_whitespace()
        .filter_map(|tok| {
            tok.trim_start_matches(['(', '"', '\'', '`'])
                .strip_prefix('@')
        })
        .map(|p| p.trim_end_matches([',', '.', ')', ';', ':', '"', '\'', '`']))
        .filter(|p| !p.is_empty() && (p.contains('.') || p.contains('/')))
        .collect()
}

fn resolve_import(base: &std::path::Path, import: &str) -> std::path::PathBuf {
    if let Some(rest) = import.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    let p = std::path::Path::new(import);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

/// A context file's text, skipping anything unreadable or implausibly large for one.
fn read_small_text(path: &std::path::Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > 2 * 1024 * 1024 {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// `cal agents`: write/refresh the managed memory block in the repo's AGENTS.md (or `-o`
/// CLAUDE.md). Uses the AGENTS_FILE budget (the block is committed and read by every agent
/// and teammate), and the safe writer, which refuses to touch a file it can't read as UTF-8.
fn cmd_agents(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    let project = project_arg(&o);
    // Write into the checkout we're in (a worktree stays a worktree), keyed on the project.
    let dir = if o.positional.is_empty() {
        cwd_git_root()
    } else {
        project.clone()
    };
    let filename = o.out.clone().unwrap_or_else(|| "AGENTS.md".into());
    let conn = open_db()?;
    let budget = export::MemoryBudget::AGENTS_FILE;
    let mem = knowledge::get_project_memory(&conn, &project, budget.fetch_limit())?;
    let body = export::agent_memory_md_budgeted(&project, &mem, None, &budget);
    let path = std::path::Path::new(&dir).join(&filename);
    let changed = export::write_managed_block_file(&path, &body)?;
    eprintln!(
        "{} {} ({}/{} threads distilled)",
        if changed { "wrote" } else { "unchanged:" },
        path.display(),
        mem.distilled_count,
        mem.thread_count
    );
    Ok(())
}

fn cmd_remember(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    if o.positional.len() < 2 {
        anyhow::bail!("usage: cal remember <decision|gotcha> <text…>  [-p PROJECT]");
    }
    let kind = o.positional[0].to_ascii_lowercase();
    let text = o.positional[1..].join(" ");
    let raw = o.project.clone().unwrap_or_else(cwd_project_root);
    let project = crate::indexer::canonical_project(&raw).unwrap_or(raw);
    let mut conn = open_db_write()?;
    let now = chrono::Utc::now().timestamp();
    let rationale = o
        .because
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty());
    // Agent origin: agents run `cal remember` too, and an agent write-back is not pinned.
    knowledge::record_fact(&conn, &project, &kind, &text, rationale, now)?;
    // Embed now so it is immediately recallable, but only with a model already on disk
    // (agents call this mid-task; a 128 MB download must not happen here). The app's
    // background job embeds it otherwise. The fact is saved either way, so an embedding
    // failure is not reported as a failed command (which would invite a duplicate retry).
    if embed::model_is_cached() {
        let embedder = embed::Embedder::cached_only();
        if let Err(e) = embed::embed_pending_facts_conn(&mut conn, &embedder) {
            eprintln!("(saved; embedding deferred to the app: {e})");
        }
    }
    eprintln!("remembered {kind} for {project}");
    Ok(())
}

/// `cal check <proposal…> [-p PROJECT]` — the contradiction guard: show settled decisions on
/// the same topic before you re-litigate one.
fn cmd_check(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    if o.positional.is_empty() {
        anyhow::bail!("usage: cal check <proposal…> [-p PROJECT]");
    }
    let proposal = o.positional.join(" ");
    let embedder = cli_embedder();
    let Some(qv) = embed::embed_query(&embedder, &proposal)? else {
        return Ok(());
    };
    let conn = open_db()?;
    let hits = knowledge::check_contradiction(&conn, &qv, o.project.as_deref(), 8)?;
    if o.json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
        return Ok(());
    }
    if hits.is_empty() {
        println!("No settled decisions conflict with that.");
        return Ok(());
    }
    println!("Prior decisions on this topic (reconcile before overriding):");
    for h in &hits {
        println!(
            "\n  • {}  ({:.0}% match)",
            h.text.trim(),
            h.similarity * 100.0
        );
        if let Some(why) = h.rationale.as_deref().filter(|w| !w.is_empty()) {
            println!("    because: {}", why.trim());
        }
        if let Some(title) = h.title.as_deref() {
            println!("    from: {title}");
        }
    }
    Ok(())
}

/// `cal commits [repo] [-n N] [--json]` — infer which commits a project's threads produced and
/// show the timeline. Run inside a git repo, or pass its path.
/// Compact distilled knowledge for one thread (summary + decision/gotcha/todo texts), as JSON.
/// Null when the thread has no knowledge row. Used to inline reasoning into the audit bundle.
fn audit_thread_knowledge(conn: &Connection, id: i64) -> serde_json::Value {
    match knowledge::get_thread_knowledge(conn, id) {
        Ok(k) => serde_json::json!({
            "summary": k.summary,
            "decisions": k.decisions.iter().map(|f| f.text.clone()).collect::<Vec<_>>(),
            "gotchas": k.gotchas.iter().map(|f| f.text.clone()).collect::<Vec<_>>(),
            "openTodos": k.todos.iter().map(|f| f.text.clone()).collect::<Vec<_>>(),
            "extracted": k.extracted,
        }),
        Err(_) => serde_json::Value::Null,
    }
}

/// `cal audit-pr <repo> --changed-files a,b --shas s1,s2` — the one-call PR-audit bundle for an
/// external reviewer (e.g. a local PR-audit app). Refreshes thread↔commit links, then returns ONE
/// JSON object: commit provenance (per sha → originating thread + its distilled reasoning),
/// per-changed-file prior threads + knowledge, the repo's recurring errors, and its project memory.
/// Always JSON. Degrades gracefully: with distillation off, knowledge fields are null but
/// provenance, file-touch, open TODOs, and repo errors still populate.
fn cmd_audit_pr(args: &[String]) -> anyhow::Result<()> {
    let mut repo: Option<String> = None;
    let mut files: Vec<String> = Vec::new();
    let mut shas: Vec<String> = Vec::new();
    let mut cap: i64 = 5; // per-file / per-sha thread cap
    let split = |v: &str| -> Vec<String> {
        v.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--changed-files" | "--files" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    files.extend(split(v));
                }
            }
            "--shas" | "--commits" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    shas.extend(split(v));
                }
            }
            "--limit" | "-n" => {
                i += 1;
                cap = args.get(i).and_then(|v| v.parse().ok()).unwrap_or(cap);
            }
            "--json" => {} // JSON is this command's only output
            s if !s.starts_with('-') && repo.is_none() => repo = Some(s.to_string()),
            _ => {}
        }
        i += 1;
    }
    let repo = repo.unwrap_or_else(cwd_git_root);
    let key = crate::indexer::canonical_project(&repo).unwrap_or_else(|| repo.clone());

    let conn = open_db_write()?;
    // Refresh thread↔commit links for the branch first; provenance is empty without this.
    let links_refreshed = gitlink::link_project(&conn, &repo).unwrap_or(0);
    let distill_on = knowledge::get_config(&conn)
        .map(|c| c.enabled)
        .unwrap_or(false);

    // One distilled-knowledge lookup per referenced thread, deduped across shas + files.
    let mut kcache: std::collections::HashMap<i64, serde_json::Value> =
        std::collections::HashMap::new();

    // 1. Commit provenance: per input sha → originating thread(s) + reasoning. Empty array, not
    //    omitted, when a sha has no inferred link (explicit null case).
    let commit_threads = gitlink::commits_by_sha(&conn, &shas)?;
    for c in &commit_threads {
        kcache
            .entry(c.thread_id)
            .or_insert_with(|| audit_thread_knowledge(&conn, c.thread_id));
    }
    let mut by_sha = serde_json::Map::new();
    for sha in &shas {
        // commits_by_sha prefix-matches, so an abbreviated input maps to the full stored sha;
        // regroup by the same prefix (keyed by the caller's original sha string below).
        let needle = sha.trim().to_ascii_lowercase();
        let produced: Vec<_> = commit_threads
            .iter()
            .filter(|c| !needle.is_empty() && c.sha.to_ascii_lowercase().starts_with(&needle))
            .map(|c| {
                serde_json::json!({
                    "id": c.thread_id,
                    "title": c.title,
                    "overlap": c.overlap,
                    "knowledge": kcache.get(&c.thread_id).cloned().unwrap_or(serde_json::Value::Null),
                })
            })
            .collect();
        by_sha.insert(
            sha.clone(),
            serde_json::json!({ "threadsProduced": produced }),
        );
    }

    // 2. Per-changed-file history: prior threads that touched each file + their reasoning.
    let mut by_file = serde_json::Map::new();
    for path in &files {
        let threads = search::threads_with_file(&conn, path, cap).unwrap_or_default();
        for t in &threads {
            kcache
                .entry(t.id)
                .or_insert_with(|| audit_thread_knowledge(&conn, t.id));
        }
        let arr: Vec<_> = threads
            .iter()
            .map(|t| {
                serde_json::json!({
                    "id": t.id,
                    "title": t.title,
                    "knowledge": kcache.get(&t.id).cloned().unwrap_or(serde_json::Value::Null),
                })
            })
            .collect();
        by_file.insert(path.clone(), serde_json::json!({ "threads": arr }));
    }

    // 3. Repo-scoped recurring errors (no error→file edge exists; the caller intersects with the
    //    touched-thread set client-side). 4. Repo project memory (decisions/gotchas/open TODOs).
    let since = chrono::Utc::now().timestamp() - 180 * 24 * 3600;
    let errors = issues::recurring_issues(&conn, Some(&key), since, 20).unwrap_or_default();
    let memory = knowledge::get_project_memory(&conn, &key, 20).ok();

    let bundle = serde_json::json!({
        "repo": key,
        "bySha": by_sha,
        "byFile": by_file,
        "recurringErrors": errors,
        "projectMemory": memory,
        "notes": {
            "distillationEnabled": distill_on,
            "linksRefreshed": links_refreshed,
            // When false, knowledge fields above are null but provenance/file-touch/TODOs/errors
            // still populate — the bundle is meant to degrade, not fail.
            "threadKnowledgeAvailable": distill_on,
        },
    });
    println!("{}", serde_json::to_string_pretty(&bundle)?);
    Ok(())
}

fn cmd_commits(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    let repo = if o.positional.is_empty() {
        cwd_git_root()
    } else {
        o.positional.join(" ")
    };
    let key = crate::indexer::canonical_project(&repo).unwrap_or_else(|| repo.clone());
    let conn = open_db_write()?;
    let n = gitlink::link_project(&conn, &repo)?;
    let rows = gitlink::commit_timeline(&conn, &key, o.limit.unwrap_or(40) as usize)?;
    if o.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        eprintln!(
            "(no thread↔commit links for {key} — need indexed threads with file mentions in this git repo)"
        );
        return Ok(());
    }
    eprintln!("{n} thread↔commit link(s) for {key}; recent commits:");
    for r in &rows {
        let threads = if r.thread_count == 1 {
            "1 thread".to_string()
        } else {
            format!("{} threads", r.thread_count)
        };
        println!(
            "{}  {}  [{}, {} file{}]  {}",
            fmt_time(Some(r.committed_at)),
            r.short_sha,
            threads,
            r.best_overlap,
            if r.best_overlap == 1 { "" } else { "s" },
            r.subject.as_deref().unwrap_or("")
        );
    }
    Ok(())
}

fn cmd_files(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    let path = o.positional.join(" ");
    if path.trim().is_empty() {
        anyhow::bail!("usage: cal files <path>  (e.g. `cal files embed/mod.rs`)");
    }
    let conn = open_db()?;
    let threads = search::threads_with_file(&conn, &path, o.limit.unwrap_or(40) as i64)?;
    if o.json {
        println!("{}", serde_json::to_string_pretty(&threads)?);
        return Ok(());
    }
    if threads.is_empty() {
        eprintln!("no threads mention '{path}'");
        return Ok(());
    }
    for t in &threads {
        println!("{:>6}  {}", t.id, t.title.as_deref().unwrap_or("untitled"));
    }
    Ok(())
}

fn cmd_ask(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    let question = o.positional.join(" ");
    if question.trim().is_empty() {
        anyhow::bail!("usage: cal ask <question…>");
    }
    let conn = open_db()?;
    let embedder = cli_embedder();
    let qv = embed::embed_query(&embedder, &question)?;
    let Some(prep) = crate::prepare_ask(&conn, &question, qv.as_deref())? else {
        eprintln!("nothing relevant found in your history");
        return Ok(());
    };
    eprintln!("asking {}/{}…", prep.provider, prep.model);
    let rt = tokio::runtime::Runtime::new()?;
    let answer = rt.block_on(agent::answer(
        &prep.provider,
        &prep.model,
        prep.key.as_deref(),
        prep.base_url.as_deref(),
        &question,
        &prep.context,
    ))?;
    println!("{answer}\n");
    println!("Sources:");
    for s in &prep.sources {
        println!(
            "  [thread {}] {}",
            s.thread_id,
            s.title.as_deref().unwrap_or("untitled")
        );
    }
    Ok(())
}

fn cmd_recall(args: &[String], kind: &str) -> anyhow::Result<()> {
    let o = parse(args)?;
    let query = o.positional.join(" ");
    if query.trim().is_empty() {
        let cmd = if kind == "decision" {
            "decisions"
        } else {
            "gotchas"
        };
        anyhow::bail!("usage: cal {cmd} <query…>");
    }
    let conn = open_db()?;
    let embedder = cli_embedder();
    let Some(qv) = embed::embed_query(&embedder, &query)? else {
        return Ok(());
    };
    let limit = o.limit.unwrap_or(20) as usize;
    let hits = knowledge::recall(&conn, &qv, kind, o.project.as_deref(), limit)?;
    if o.json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
        return Ok(());
    }
    if hits.is_empty() {
        eprintln!("nothing recalled — distill some threads first (`cal distill <id>`)");
        return Ok(());
    }
    for h in &hits {
        println!("• {}", h.text);
        println!(
            "    {} · thread {} · {:.0}% match",
            h.title.as_deref().unwrap_or("untitled"),
            h.thread_id,
            h.similarity * 100.0
        );
    }
    Ok(())
}

/// `cal similar <task…>` — the "have I done this before?" guard: prior SESSIONS where the
/// user worked on something like `task`, each with the most-relevant decision/gotcha.
fn cmd_similar(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    let query = o.positional.join(" ");
    if query.trim().is_empty() {
        anyhow::bail!("usage: cal similar <task description…>  [-p <project>]");
    }
    let conn = open_db()?;
    let embedder = cli_embedder();
    let Some(qv) = embed::embed_query(&embedder, &query)? else {
        return Ok(());
    };
    let limit = o.limit.unwrap_or(8) as usize;
    let hits = knowledge::find_prior_work(&conn, &qv, o.project.as_deref(), limit)?;
    if o.json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
        return Ok(());
    }
    if hits.is_empty() {
        eprintln!("no prior work found — distill some threads first (`cal distill <id>`)");
        return Ok(());
    }
    for h in &hits {
        println!(
            "• {} · thread {} · {:.0}% match",
            h.title.as_deref().unwrap_or("untitled"),
            h.thread_id,
            h.similarity * 100.0
        );
        println!("    {}: {}", h.kind, h.snippet);
    }
    Ok(())
}

fn cmd_cat(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    let id: i64 = o
        .positional
        .first()
        .ok_or_else(|| anyhow::anyhow!("cat needs a thread id. e.g. `cal cat 42`"))?
        .parse()
        .map_err(|_| anyhow::anyhow!("thread id must be a number"))?;
    let conn = open_db()?;
    let packed = context::pack_thread(&conn, id, context::DEFAULT_BUDGET_CHARS)?
        .ok_or_else(|| anyhow::anyhow!("thread {id} not found"))?;
    println!("{packed}");
    Ok(())
}

fn cmd_stats(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    let conn = open_db()?;
    let s = search::stats(&conn)?;

    if o.json {
        println!("{}", serde_json::to_string_pretty(&s)?);
        return Ok(());
    }

    let pct = if s.embeddable > 0 {
        s.embedded * 100 / s.embeddable
    } else {
        0
    };
    println!("{} threads · {} messages", s.threads, s.messages);
    println!(
        "semantic: {}/{} embedded ({pct}%)",
        s.embedded, s.embeddable
    );
    println!("range: {} → {}", fmt_time(s.earliest), fmt_time(s.latest));

    println!("\nby source:");
    for src in &s.per_source {
        if src.threads > 0 || src.messages > 0 {
            println!(
                "  {:<12} {:>6} threads · {:>7} msgs",
                src.kind, src.threads, src.messages
            );
        }
    }
    println!("\nby role:");
    for r in &s.per_role {
        println!("  {:<12} {:>7} msgs", r.role, r.messages);
    }
    if !s.top_projects.is_empty() {
        println!("\ntop projects:");
        for p in &s.top_projects {
            println!("  {:>4}  {}", p.threads, p.project);
        }
    }
    Ok(())
}

fn cmd_export(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    let id: i64 = o
        .positional
        .first()
        .ok_or_else(|| anyhow::anyhow!("export needs a thread id. e.g. `cal export 42`"))?
        .parse()
        .map_err(|_| anyhow::anyhow!("thread id must be a number"))?;
    let conn = open_db()?;
    let detail = search::thread_detail(&conn, id)?
        .ok_or_else(|| anyhow::anyhow!("thread {id} not found"))?;
    let synthesis = if o.synthesize {
        let (provider, model) = crate::pick_synth_provider()
            .ok_or_else(|| anyhow::anyhow!("--synthesize needs an API key; add one in the app"))?;
        let key = secrets::get_key(provider)?;
        let base_url = knowledge::get_provider_base_url(&conn, provider)?;
        let packed = context::pack_thread(&conn, id, context::DEFAULT_BUDGET_CHARS)?
            .ok_or_else(|| anyhow::anyhow!("thread {id} not found"))?;
        eprintln!("synthesizing with {provider}/{model}…");
        let rt = tokio::runtime::Runtime::new()?;
        Some(rt.block_on(agent::synthesize(
            provider,
            model,
            key.as_deref(),
            base_url.as_deref(),
            &packed,
        ))?)
    } else {
        None
    };
    let md = export::to_obsidian(&detail, synthesis.as_deref());

    // Destination precedence: --out FILE > --vault/CALLIMACHUS_VAULT > stdout.
    let vault = o.vault.or_else(|| std::env::var("CALLIMACHUS_VAULT").ok());
    let dest: Option<std::path::PathBuf> = if let Some(out) = o.out {
        Some(std::path::PathBuf::from(out))
    } else {
        vault.map(|v| {
            std::path::PathBuf::from(v)
                .join("Callimachus")
                .join(format!("{}.md", export::note_filename(&detail)))
        })
    };

    match dest {
        Some(path) => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, md)?;
            eprintln!("wrote {}", path.display());
        }
        None => print!("{md}"),
    }
    Ok(())
}

/// Search snippets wrap matches in \u{1}…\u{2} sentinels (the GUI swaps them for
/// <mark>). Strip them for plain terminal output.
fn strip_marks(s: &str) -> String {
    s.replace(['\u{1}', '\u{2}'], "")
}

/// `cal snapshot <thread-id> [-l label]` — save a resumable checkpoint of a thread.
fn cmd_snapshot(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    let id: i64 = o
        .positional
        .first()
        .ok_or_else(|| anyhow::anyhow!("usage: cal snapshot <thread-id> [-l LABEL]"))?
        .parse()
        .map_err(|_| anyhow::anyhow!("thread-id must be a number"))?;
    let conn = open_db_write()?;
    let snap = snapshot::create(&conn, id, o.label.as_deref())?;
    if o.json {
        println!("{}", serde_json::to_string_pretty(&snap)?);
        return Ok(());
    }
    println!(
        "Saved snapshot #{} \"{}\" (~{} tokens). Resume with: cal resume {}",
        snap.id, snap.label, snap.token_estimate, snap.id
    );
    Ok(())
}

/// `cal snapshots [project] [-n N] [--json]` — list saved snapshots (newest first).
fn cmd_snapshots(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    let project = if o.positional.is_empty() {
        None
    } else {
        Some(o.positional.join(" "))
    };
    let conn = open_db()?;
    let snaps = snapshot::list(&conn, project.as_deref(), o.limit.unwrap_or(40) as usize)?;
    if o.json {
        println!("{}", serde_json::to_string_pretty(&snaps)?);
        return Ok(());
    }
    if snaps.is_empty() {
        eprintln!("(no snapshots yet — create one with `cal snapshot <thread-id>`)");
        return Ok(());
    }
    for s in &snaps {
        println!(
            "#{:<4} {}  [{}]  {}",
            s.id,
            fmt_time(Some(s.created_at)),
            s.source_kind.as_deref().unwrap_or("?"),
            s.label
        );
    }
    Ok(())
}

/// `cal resume <snapshot-id> [-a agent]` — relaunch an agent CLI seeded with a snapshot.
///
/// On macOS and Linux `cal` already runs in a terminal, so the agent runs right here with
/// inherited stdio (no new Terminal window). On Windows it opens in a new console window
/// instead: `cal.exe` is the GUI-subsystem app exe, which cmd and PowerShell don't wait for,
/// so an agent TUI run in place would fight the shell's prompt for the same keystrokes.
/// Either way the session gets `CALLIMACHUS_SKIP_HOOK=1`: the snapshot already carries the
/// project's memory block, so the SessionStart hook must not inject a second copy.
fn cmd_resume(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    let id: i64 = o
        .positional
        .first()
        .ok_or_else(|| anyhow::anyhow!("usage: cal resume <snapshot-id> [-a AGENT]"))?
        .parse()
        .map_err(|_| anyhow::anyhow!("snapshot-id must be a number"))?;
    let conn = open_db()?;
    let snap = snapshot::load(&conn, id)?
        .ok_or_else(|| anyhow::anyhow!("no snapshot #{id} (see `cal snapshots`)"))?;
    drop(conn);
    let agent = o.agent.as_deref().unwrap_or("claude");
    if cfg!(windows) {
        let file = agent::cli_resume::launch_with_context(
            agent,
            &snap.body,
            snap.meta.project_path.as_deref(),
        )?;
        eprintln!(
            "Opened snapshot #{id} (\"{}\") in {agent} in a new window. Context: {file}",
            snap.meta.label
        );
        return Ok(());
    }
    let file = agent::cli_resume::write_context_file(&snap.body)?;
    let prompt = agent::cli_resume::context_prompt(&file);
    eprintln!(
        "Resuming snapshot #{id} (\"{}\") in {agent}. Context: {}",
        snap.meta.label,
        file.display()
    );
    run_agent_here(agent, &prompt, snap.meta.project_path.as_deref())?;
    Ok(())
}

/// Run an agent CLI in this terminal (inherited stdio) and wait for it to exit.
fn run_agent_here(
    agent: &str,
    prompt: &str,
    cwd: Option<&str>,
) -> anyhow::Result<std::process::ExitStatus> {
    let command = |program: &str| {
        let mut c = std::process::Command::new(program);
        c.arg(prompt).env(agent::cli_resume::SKIP_HOOK_ENV, "1");
        if let Some(dir) = cwd.filter(|d| std::path::Path::new(d).is_dir()) {
            c.current_dir(dir);
        }
        c
    };
    match command(agent).status() {
        Ok(status) => Ok(status),
        // npm installs agent CLIs on Windows as `.cmd` shims, which a bare name doesn't
        // resolve to.
        #[cfg(windows)]
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => command(&format!("{agent}.cmd"))
            .status()
            .map_err(|e| anyhow::anyhow!("couldn't start `{agent}`: {e}")),
        Err(e) => Err(anyhow::anyhow!("couldn't start `{agent}`: {e}")),
    }
}

/// Internal hook target for Claude Code's PreCompact event. Reads the hook's JSON from
/// stdin, maps the live session's `transcript_path` to its indexed thread, and saves a
/// rolling snapshot so context survives the compaction. Best-effort and SILENT
/// (`run_hook_silently`): a hook must never break the agent loop.
fn cmd_snapshot_hook(_args: &[String]) -> anyhow::Result<()> {
    let Some(v) = read_hook_payload() else {
        return Ok(());
    };
    let event = v
        .get("hook_event_name")
        .and_then(|x| x.as_str())
        .unwrap_or("hook");
    // SubagentStop's transcript_path is the PARENT session, so snapshotting there re-packed
    // the whole parent on every subagent stop (dozens per workflow) and captured nothing of
    // the subagent. The installer no longer registers it; ignore it from older installs.
    if event == "SubagentStop" {
        return Ok(());
    }
    let Some(transcript) = v.get("transcript_path").and_then(|x| x.as_str()) else {
        return Ok(());
    };
    // Only act when an index already exists — never create one from inside a hook.
    if !db::default_index_path().exists() {
        return Ok(());
    }
    let conn = open_hook_db_write()?;
    // Fail fast rather than holding up the compaction while the app's writer is busy.
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    // A relocated Claude config saved in Settings must map transcripts to the same ids the
    // indexer wrote, so apply it before deriving the id.
    crate::indexer::claude::load_config_dir_setting(&conn);
    let Some(external_id) = claude_external_id(transcript) else {
        return Ok(());
    };
    let thread_id: Option<i64> = conn
        .query_row(
            "SELECT t.id FROM threads t JOIN sources s ON s.id = t.source_id
             WHERE s.kind = 'claude_code' AND t.external_id = ?1",
            [&external_id],
            |r| r.get(0),
        )
        .ok();
    if let Some(tid) = thread_id {
        snapshot::create_rolling_auto(&conn, tid, event)?;
    }
    Ok(())
}

/// Map a Claude Code hook's absolute `transcript_path` back to the indexed thread's
/// `external_id`. Must produce byte-identical ids to the indexer (same projects root,
/// honoring `CLAUDE_CONFIG_DIR`, and '/' separators on every OS), or snapshots silently stop
/// matching their thread on Windows and for relocated configs.
fn claude_external_id(transcript_path: &str) -> Option<String> {
    let root = crate::indexer::claude::projects_root()?;
    external_id_under(transcript_path, &root)
}

/// The id of `transcript_path` relative to `base`, as the indexer writes it, or None when
/// the transcript isn't under `base`.
fn external_id_under(transcript_path: &str, base: &std::path::Path) -> Option<String> {
    let path = std::path::Path::new(transcript_path);
    path.strip_prefix(base).ok()?;
    Some(crate::indexer::claude::external_id_for(base, path))
}

/// Cosine floor for an UNPROMPTED "you may have solved this before" note. bge-small scores
/// cluster high, so this sits well above the on-demand guard's 0.55.
const PROACTIVE_FLOOR: f32 = 0.68;

/// Most notes per prompt, and most prompts per session that get one.
const PROACTIVE_MAX_HITS: usize = 2;
const PROACTIVE_MAX_INJECTIONS: u32 = 3;

/// `cal recall-now`: a Claude Code UserPromptSubmit hook target. Reads the submitted
/// prompt from the hook JSON on stdin; when it STRONGLY matches prior solved work, prints a
/// short note that Claude Code injects into the agent's context ("you may have worked on
/// this before"). It runs on every prompt, so the cheap gates come first and the embedding
/// model is touched last:
/// 1. the prompt is a real request (not a slash command, at least ~5 words);
/// 2. an index exists and the model is already downloaded (a hook never downloads it);
/// 3. this session hasn't used up its injections;
/// 4. some decision/gotcha in the project mentions the prompt's key terms (plain SQL).
///
/// SILENT and best-effort: no match or any error exits 0 with no output.
fn cmd_recall_now(_args: &[String]) -> anyhow::Result<()> {
    let Some(v) = read_hook_payload() else {
        return Ok(());
    };
    let prompt = v
        .get("prompt")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim();
    if !prompt_worth_recalling(prompt) {
        return Ok(());
    }
    if !db::default_index_path().exists() || !embed::model_is_cached() {
        return Ok(());
    }
    let session = v.get("session_id").and_then(|x| x.as_str()).unwrap_or("");
    let mut state = load_recall_state(session);
    if state.injections >= PROACTIVE_MAX_INJECTIONS {
        return Ok(());
    }
    let project = v
        .get("cwd")
        .and_then(|x| x.as_str())
        .and_then(crate::indexer::canonical_project);

    let conn = open_db()?;
    let terms = knowledge::prompt_terms(prompt, 8);
    if !knowledge::has_keyword_candidates(&conn, &terms, project.as_deref())? {
        return Ok(());
    }
    // bge-small reads ~512 tokens; embedding more of a long prompt only costs time.
    let query: String = prompt.chars().take(1500).collect();
    // Cached-only: even if the model vanished since the check above, never download here.
    let embedder = embed::Embedder::cached_only();
    let Some(qv) = embed::embed_query_cached_only(&embedder, &query)? else {
        return Ok(());
    };
    let hits = knowledge::find_prior_work(&conn, &qv, project.as_deref(), 5)?;
    let hits = select_recall_hits(hits, &state.threads);
    if hits.is_empty() {
        return Ok(());
    }
    state.threads.extend(hits.iter().map(|h| h.thread_id));
    state.injections += 1;
    save_recall_state(session, &state);

    println!("[Callimachus] You may have worked on this before. Reuse it before redoing:");
    for h in &hits {
        println!("{}", recall_line(h));
    }
    Ok(())
}

/// One recall hit as a bullet line, scrubbed like every other injection surface
/// (`export::fact_line`): titles are often the first prompt and older facts were stored
/// unredacted, so secrets are redacted, and a multi-line snippet is flattened so it can't
/// break the list.
fn recall_line(h: &knowledge::PriorWork) -> String {
    let title = h
        .title
        .as_deref()
        .map(|t| export::fact_line(t, 120))
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "(untitled)".into());
    let snippet = export::fact_line(&h.snippet, 300);
    format!("  • {title}  ({}: {snippet})", h.kind)
}

/// Whether a submitted prompt is worth a recall lookup: not a slash command, and long
/// enough (5+ words) to say what the task is.
fn prompt_worth_recalling(prompt: &str) -> bool {
    !prompt.starts_with('/')
        && prompt.chars().count() >= 12
        && prompt.split_whitespace().count() >= 5
}

/// Pick what to surface from `find_prior_work`'s hits (best first). Strict on purpose: if
/// the BEST match was already shown this session, show nothing rather than falling through
/// to weaker matches (repeating a prompt used to surface progressively less related
/// threads). Otherwise keep unseen hits above the floor, at most PROACTIVE_MAX_HITS.
fn select_recall_hits(
    hits: Vec<knowledge::PriorWork>,
    seen: &std::collections::HashSet<i64>,
) -> Vec<knowledge::PriorWork> {
    match hits.first() {
        Some(best) if !seen.contains(&best.thread_id) => {}
        _ => return Vec::new(),
    }
    hits.into_iter()
        .filter(|h| h.similarity >= PROACTIVE_FLOOR && !seen.contains(&h.thread_id))
        .take(PROACTIVE_MAX_HITS)
        .collect()
}

/// Per-session `recall-now` state: threads already surfaced, and how many prompts got a note.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct RecallState {
    #[serde(default)]
    threads: std::collections::HashSet<i64>,
    #[serde(default)]
    injections: u32,
}

/// Per-session cache file for `recall-now`, so it doesn't repeat itself.
fn recall_seen_path(session: &str) -> Option<std::path::PathBuf> {
    let home = dirs::home_dir()?;
    let safe: String = session
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    let name = if safe.is_empty() { "session" } else { &safe };
    Some(
        home.join(".callimachus/recall")
            .join(format!("{name}.json")),
    )
}

fn load_recall_state(session: &str) -> RecallState {
    let Some(text) = recall_seen_path(session).and_then(|p| std::fs::read_to_string(p).ok()) else {
        return RecallState::default();
    };
    serde_json::from_str::<RecallState>(&text)
        .or_else(|_| {
            // Older versions stored just the set of thread ids (up to 2 per injection).
            serde_json::from_str::<std::collections::HashSet<i64>>(&text).map(|threads| {
                RecallState {
                    injections: threads.len().div_ceil(2) as u32,
                    threads,
                }
            })
        })
        .unwrap_or_default()
}

fn save_recall_state(session: &str, state: &RecallState) {
    if let Some(p) = recall_seen_path(session) {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string(state) {
            let _ = std::fs::write(p, json);
        }
    }
}

/// `cal issues [project] [-n N] [--json]` — recurring errors across sessions (last 180 days).
fn cmd_issues(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    let project = if o.positional.is_empty() {
        None
    } else {
        Some(o.positional.join(" "))
    };
    let conn = open_db()?;
    let since = chrono::Utc::now().timestamp() - 180 * 86_400;
    let found = issues::recurring_issues(
        &conn,
        project.as_deref(),
        since,
        o.limit.unwrap_or(20) as usize,
    )?;
    if o.json {
        println!("{}", serde_json::to_string_pretty(&found)?);
        return Ok(());
    }
    if found.is_empty() {
        eprintln!("(no recurring errors found in the last 180 days)");
        return Ok(());
    }
    println!("Recurring errors (last 180 days, most frequent first):");
    for i in &found {
        let threads = if i.threads == 1 {
            "1 thread".to_string()
        } else {
            format!("{} threads", i.threads)
        };
        println!(
            "\n  {}×  across {}  (last seen {})",
            i.count,
            threads,
            fmt_time(Some(i.last_seen))
        );
        println!("    {}", i.example.trim());
    }
    Ok(())
}

/// `cal cost [project] [-n N] [--json]` — estimated $ spend by model + the priciest threads.
fn cmd_cost(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    let project = if o.positional.is_empty() {
        None
    } else {
        Some(o.positional.join(" "))
    };
    let conn = open_db()?;
    // All-time by default (cost is a lifetime figure); `since = 0`.
    let s = cost::spend(&conn, 0, project.as_deref(), o.limit.unwrap_or(8) as usize)?;
    if o.json {
        println!("{}", serde_json::to_string_pretty(&s)?);
        return Ok(());
    }
    if s.tracked_calls == 0 && s.untracked_calls == 0 {
        eprintln!(
            "(no token usage captured yet — run Reindex in the app, then retry. The source files\n carry it; data indexed before this feature doesn't.)"
        );
        return Ok(());
    }
    println!(
        "Estimated spend: ${:.2}  ({} tracked LLM calls)",
        s.total_cost, s.tracked_calls
    );
    if s.untracked_calls > 0 {
        println!(
            "  (+{} calls on models with no price on file — not counted)",
            s.untracked_calls
        );
    }
    println!("\nBy model:");
    for m in &s.by_model {
        if m.priced {
            println!("  ${:>9.2}   {}  ({} calls)", m.cost, m.model, m.calls);
        } else {
            println!("  {:>10}   {}  ({} calls, no price)", "—", m.model, m.calls);
        }
    }
    println!("\nMost expensive threads:");
    for t in &s.top_threads {
        println!(
            "  ${:>9.2}   #{}  {}",
            t.cost,
            t.thread_id,
            t.title.as_deref().unwrap_or("")
        );
    }
    println!("\n(estimate from list prices; not a billing record)");
    Ok(())
}

fn fmt_time(epoch: Option<i64>) -> String {
    match epoch.and_then(|e| chrono::DateTime::from_timestamp(e, 0)) {
        Some(dt) => dt.format("%Y-%m-%d").to_string(),
        None => "—".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_pulls_flags_and_keeps_positionals() {
        let o = parse(&argv(&["-s", "claude", "--json", "auth", "bug"])).unwrap();
        assert_eq!(o.source.as_deref(), Some("claude"));
        assert!(o.json);
        assert_eq!(o.positional, vec!["auth", "bug"]);
    }

    #[test]
    fn parse_repeats_tags_and_reads_limit() {
        let o = parse(&argv(&["-t", "a", "--tag", "b", "-n", "5", "--starred"])).unwrap();
        assert_eq!(o.tags, vec!["a", "b"]);
        assert_eq!(o.limit, Some(5));
        assert!(o.starred);
    }

    #[test]
    fn parse_rejects_unknown_flag_and_non_numeric_limit() {
        assert!(parse(&argv(&["--nope"])).is_err());
        assert!(parse(&argv(&["-n", "abc"])).is_err());
    }

    #[test]
    fn parse_reads_hook_budget() {
        let o = parse(&argv(&["--budget", "4000", "-n", "5"])).unwrap();
        assert_eq!(o.budget, Some(4000));
        assert_eq!(o.limit, Some(5));
        assert!(parse(&argv(&["--budget", "lots"])).is_err());
    }

    #[test]
    fn hook_commands_never_fail_visibly() {
        // A bad flag is an error for a normal command but a silent exit 0 for a hook.
        assert_eq!(run_and_exit(&argv(&["hook", "--nope"])), 0);
        assert_eq!(run_and_exit(&argv(&["bogus-cmd"])), 1);
    }

    #[test]
    fn hook_budget_follows_session_source() {
        assert_eq!(hook_budget("resume"), None, "resume re-sends nothing");
        assert_eq!(hook_budget("compact"), Some(export::MemoryBudget::COMPACT));
        assert_eq!(hook_budget("startup"), Some(export::MemoryBudget::HOOK));
        assert_eq!(hook_budget("clear"), Some(export::MemoryBudget::HOOK));
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "calli_cli_{tag}_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn detects_managed_block_in_claude_md_and_its_imports() {
        let plain = temp_dir("plain");
        std::fs::write(plain.join("CLAUDE.md"), "# rules\n\nbe nice\n").unwrap();
        assert!(!claude_md_has_managed_block(&[plain.as_path()]));

        let direct = temp_dir("direct");
        std::fs::create_dir_all(direct.join(".claude")).unwrap();
        std::fs::write(
            direct.join(".claude/CLAUDE.md"),
            export::upsert_managed_block("", "BODY"),
        )
        .unwrap();
        assert!(claude_md_has_managed_block(&[direct.as_path()]));

        let imported = temp_dir("import");
        std::fs::write(
            imported.join("CLAUDE.md"),
            "See @AGENTS.md for shared rules.\n",
        )
        .unwrap();
        std::fs::write(
            imported.join("AGENTS.md"),
            export::upsert_managed_block("# Agents", "BODY"),
        )
        .unwrap();
        assert!(claude_md_has_managed_block(&[imported.as_path()]));

        for d in [plain, direct, imported] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn managed_block_check_follows_what_claude_code_loads() {
        // A main checkout with the block, a sibling worktree, and one nested in the repo.
        let root = temp_dir("worktrees");
        let main = root.join("repo");
        let sibling = root.join("repo-feature");
        let nested = main.join(".claude/worktrees/x/src");
        for d in [&main, &sibling, &nested] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(
            main.join("CLAUDE.md"),
            export::upsert_managed_block("", "BODY"),
        )
        .unwrap();
        // The sibling session never loads repo/CLAUDE.md, so the hook must still inject.
        assert!(!claude_md_has_managed_block(&claude_md_dirs(&sibling)));
        // A nested worktree (or any subdir) does load it as an ancestor.
        assert!(claude_md_has_managed_block(&claude_md_dirs(&nested)));
        assert!(claude_md_dirs(&nested).iter().all(|d| d.parent().is_some()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn claude_md_imports_only_path_like_tokens() {
        let text = "Read @AGENTS.md, then @docs/style.md. Ping @ari or mail me@x.dev (@~/notes.md)";
        assert_eq!(
            claude_md_imports(text),
            vec!["AGENTS.md", "docs/style.md", "~/notes.md"]
        );
    }

    #[test]
    fn git_root_of_finds_the_checkout() {
        let root = temp_dir("git");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        let want = std::fs::canonicalize(&root).unwrap();
        assert_eq!(git_root_of(&root.join("a/b")), want);
        let _ = std::fs::remove_dir_all(&root);
    }

    fn prior(thread_id: i64, similarity: f32) -> knowledge::PriorWork {
        knowledge::PriorWork {
            thread_id,
            title: None,
            project_path: None,
            source: "claude_code".into(),
            kind: "decision".into(),
            snippet: "s".into(),
            similarity,
            matches: 1,
        }
    }

    #[test]
    fn recall_lines_are_redacted_and_flat() {
        let mut h = prior(1, 0.9);
        h.title = Some("use sk-ant-api03-AbCdEfGhIjKlMnOpQrStUv0123456789 for staging".into());
        h.snippet = "first line\n\n- second   line".into();
        let line = recall_line(&h);
        assert!(!line.contains("sk-ant-api03"), "{line}");
        assert!(!line.contains('\n'), "{line}");
        assert!(line.contains("first line - second line"), "{line}");
        h.title = None;
        assert!(recall_line(&h).starts_with("  • (untitled)"));
    }

    #[test]
    fn recall_selection_is_strict() {
        let none = std::collections::HashSet::new();
        let picked = select_recall_hits(vec![prior(1, 0.9), prior(2, 0.8), prior(3, 0.75)], &none);
        assert_eq!(
            picked.iter().map(|h| h.thread_id).collect::<Vec<_>>(),
            vec![1, 2],
            "at most two notes"
        );
        let weak = select_recall_hits(vec![prior(1, 0.66), prior(2, 0.6)], &none);
        assert!(weak.is_empty(), "below the proactive floor");
        // The best match was already shown: stay quiet instead of offering weaker ones.
        let seen: std::collections::HashSet<i64> = [1].into_iter().collect();
        assert!(select_recall_hits(vec![prior(1, 0.9), prior(2, 0.85)], &seen).is_empty());
        assert!(select_recall_hits(Vec::new(), &none).is_empty());
    }

    #[test]
    fn recall_skips_commands_and_short_prompts() {
        assert!(!prompt_worth_recalling(
            "/compact keep the auth details please"
        ));
        assert!(!prompt_worth_recalling("fix the typo"));
        assert!(prompt_worth_recalling(
            "how do we install the claude code hooks into settings json"
        ));
    }

    #[test]
    fn run_with_no_args_prints_usage_ok() {
        // Empty argv is the help path — must not touch the DB or error.
        run(&[]).unwrap();
    }

    #[test]
    fn run_rejects_unknown_command() {
        assert!(run(&argv(&["bogus-cmd"])).is_err());
    }

    #[test]
    fn external_id_maps_transcript_path_to_indexed_id() {
        let base = std::path::Path::new("/home/me/.claude/projects");
        // Top-level session.
        assert_eq!(
            external_id_under("/home/me/.claude/projects/-proj/abc-123.jsonl", base).as_deref(),
            Some("-proj/abc-123.jsonl")
        );
        // Subagent transcript keeps its full relative path (matches the indexed subagent id).
        assert_eq!(
            external_id_under(
                "/home/me/.claude/projects/-proj/sess/subagents/agent-x.jsonl",
                base
            )
            .as_deref(),
            Some("-proj/sess/subagents/agent-x.jsonl")
        );
        // A path outside the projects dir maps to nothing (we don't snapshot it).
        assert!(external_id_under("/tmp/elsewhere/x.jsonl", base).is_none());
    }

    // ---- docs-in-sync guards: keep `cal help` + the README/website from drifting ----

    /// Flags added for `hook` must stay documented in `cal help`.
    #[test]
    fn usage_documents_hook_flags() {
        assert!(USAGE.contains("--budget"));
        assert!(USAGE.contains("cal hook [<project>] [-n PER_KIND] [--budget CHARS]"));
    }

    /// `cal help` (USAGE) must document every real command, so a new subcommand can't ship
    /// undocumented. Internal hook targets are intentionally not user-facing.
    #[test]
    fn usage_documents_every_command() {
        const INTERNAL: &[&str] = &["snapshot-hook", "recall-now"];
        for &c in COMMANDS {
            if INTERNAL.contains(&c) {
                continue;
            }
            assert!(
                USAGE.contains(c),
                "`cal {c}` is a command but isn't in USAGE (cal help) — document it"
            );
        }
    }

    /// Every `cal <subcommand>` referenced in a CODE context (a command example), so prose like
    /// "the cal CLI is pipe-friendly" doesn't read as a `cal cli` / `cal pipe-friendly` invocation.
    /// We only count a `cal ` whose preceding char marks code: a backtick, quote, newline (a
    /// code-block / cheat-sheet line), or `(` / `$` (a shell substitution).
    fn cal_commands_in(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut from = 0;
        while let Some(rel) = text[from..].find("cal ") {
            let start = from + rel;
            from = start + 4;
            let before = text[..start].chars().last();
            let code_ctx = matches!(before, None | Some('`' | '"' | '\n' | '(' | '$'));
            if !code_ctx {
                continue;
            }
            // The command token is the lowercase [a-z-] run right after "cal ".
            let cmd: String = text[from..]
                .chars()
                .take_while(|c| c.is_ascii_lowercase() || *c == '-')
                .collect();
            if cmd.starts_with(|c: char| c.is_ascii_lowercase()) {
                out.push(cmd);
            }
        }
        out
    }

    /// The README and the website must only reference commands that actually exist — catches a
    /// renamed/removed command or a typo'd `cal foo` in the docs. Files missing (partial checkout)
    /// are skipped, not failed.
    #[test]
    fn docs_only_reference_real_commands() {
        // `help` is dispatched separately, not in COMMANDS, but is a valid documented invocation.
        let known: std::collections::HashSet<&str> = COMMANDS
            .iter()
            .copied()
            .chain(std::iter::once("help"))
            .collect();
        let docs = [
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../../README.md"),
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../web/src/routes/cli.tsx"),
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../web/src/routes/index.tsx"
            ),
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../web/src/routes/desktop.tsx"
            ),
        ];
        let mut checked = 0;
        for path in docs {
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            checked += 1;
            for cmd in cal_commands_in(&text) {
                assert!(
                    known.contains(cmd.as_str()),
                    "doc {path} references `cal {cmd}`, which is not a real command (typo or removed?)"
                );
            }
        }
        assert!(
            checked > 0,
            "no docs found to cross-check — are the relative paths right?"
        );
    }
}
