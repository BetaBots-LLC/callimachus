//! `cal` CLI core — the search/recent/cat/stats/export logic, factored into the
//! library so it has two entry points: the standalone `cal` binary, and the
//! desktop app when invoked as `cal` (argv0) or with a cal subcommand. That lets
//! the installer symlink the app itself to `~/.local/bin/cal` — no separate
//! binary to ship. Reads the same local index.db as the GUI and MCP server.
//!
//! Set CALLIMACHUS_DB to point at a specific index.db; CALLIMACHUS_VAULT to a
//! default Obsidian vault for `cal export`.

use crate::{agent, context, db, embed, export, gitlink, knowledge, search, secrets, snapshot};
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
                                record a fact for the current repo (-p PROJECT to
                                override), pinned into its project memory
  cal check <proposal…> [-p PROJECT] [--json]
                                contradiction guard: settled decisions on this topic
                                (surfaces 'you already decided X because Y')
  cal commits [<repo>] [-n LIMIT] [--json]
                                infer + show which commits your threads produced
                                (run inside a git repo, or pass its path)
  cal hook [<project>]          print the current repo's memory for injection (use as a
                                Claude Code SessionStart hook command)
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
  -n, --limit N         max results (default 20; todos 50, files 40)
  -V, --vault DIR       Obsidian vault dir for `export` (else CALLIMACHUS_VAULT)
  -o, --out FILE        write `export` output to FILE instead of a vault/stdout
  -S, --synthesize      prepend an LLM summary / decisions / gotchas / TODOs
                        to `export` (uses the first stored provider key)
      --json            machine-readable JSON output";

/// Run the CLI and map the result to a process exit code (printing errors). Used
/// by both the `cal` binary and the app's cal-mode dispatch.
pub fn run_and_exit(args: &[String]) -> i32 {
    match run(args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("cal: {e}");
            1
        }
    }
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
/// coexist with the app's writes (busy_timeout waits out the rare overlap).
fn open_db_write() -> anyhow::Result<Connection> {
    db::open(&db::default_index_path())
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

fn cmd_search(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    if o.positional.is_empty() {
        anyhow::bail!("search needs a query. e.g. `cal search \"vector index\"`");
    }
    let query = o.positional.join(" ");
    let conn = open_db()?;
    let f = filters(&o);
    let hits = if o.hybrid {
        let embedder = embed::Embedder::default();
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
    let embedder = embed::Embedder::default();
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
    let (provider, model, key) = crate::resolve_distill_engine(&conn)?;
    let packed = context::pack_thread(&conn, id, context::DEFAULT_BUDGET_CHARS)?
        .ok_or_else(|| anyhow::anyhow!("thread {id} not found"))?;
    eprintln!("distilling thread {id} with {provider}/{model}…");
    let rt = tokio::runtime::Runtime::new()?;
    let distilled = rt.block_on(agent::distill(&provider, &model, key.as_deref(), &packed))?;
    let now = chrono::Utc::now().timestamp();
    knowledge::store_distilled(&mut conn, id, &distilled, now)?;
    print_knowledge(&knowledge::get_thread_knowledge(&conn, id)?);
    Ok(())
}

/// The git repo root for the cwd (walks up for `.git`), else the cwd. For `cal memory`.
fn cwd_project_root() -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    let mut dir = cwd.as_path();
    loop {
        if dir.join(".git").exists() {
            return dir.to_string_lossy().into_owned();
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => break,
        }
    }
    cwd.to_string_lossy().into_owned()
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

fn cmd_hook(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    let project = project_arg(&o);
    let conn = open_db()?;
    let mem = knowledge::get_project_memory(&conn, &project, 40)?;
    // Emit nothing when there's no memory — a SessionStart hook shouldn't add noise.
    if mem.decisions.is_empty() && mem.gotchas.is_empty() && mem.open_todos.is_empty() {
        return Ok(());
    }
    println!("{}", export::agent_memory_md(&project, &mem, None));
    Ok(())
}

fn cmd_agents(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    let project = project_arg(&o);
    let filename = o.out.clone().unwrap_or_else(|| "AGENTS.md".into());
    let conn = open_db()?;
    let mem = knowledge::get_project_memory(&conn, &project, 100)?;
    let body = export::agent_memory_md(&project, &mem, None);
    let path = std::path::Path::new(&project).join(&filename);
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    std::fs::write(&path, export::upsert_managed_block(&existing, &body))?;
    eprintln!(
        "wrote {} ({}/{} threads distilled)",
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
    knowledge::record_fact(&conn, &project, &kind, &text, rationale, now)?;
    // Embed so it is immediately recallable.
    let embedder = embed::Embedder::default();
    embed::embed_pending_facts_conn(&mut conn, &embedder)?;
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
    let embedder = embed::Embedder::default();
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
fn cmd_commits(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    let repo = if o.positional.is_empty() {
        cwd_project_root()
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
    eprintln!("Linked {n} commit(s) for {key}:");
    for r in &rows {
        println!(
            "{}  {}  [{} file{}]  {}",
            fmt_time(Some(r.committed_at)),
            r.short_sha,
            r.overlap,
            if r.overlap == 1 { "" } else { "s" },
            r.subject.as_deref().unwrap_or("")
        );
        if let Some(t) = r.thread_title.as_deref() {
            println!("    from thread #{}: {t}", r.thread_id);
        }
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
    let embedder = embed::Embedder::default();
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
    let embedder = embed::Embedder::default();
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
    let embedder = embed::Embedder::default();
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
        let packed = context::pack_thread(&conn, id, context::DEFAULT_BUDGET_CHARS)?
            .ok_or_else(|| anyhow::anyhow!("thread {id} not found"))?;
        eprintln!("synthesizing with {provider}/{model}…");
        let rt = tokio::runtime::Runtime::new()?;
        Some(rt.block_on(agent::synthesize(provider, model, key.as_deref(), &packed))?)
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
    let agent = o.agent.as_deref().unwrap_or("claude");
    let file = agent::cli_resume::launch_with_context(
        agent,
        &snap.body,
        snap.meta.project_path.as_deref(),
    )?;
    println!(
        "Launched {agent} with snapshot #{id} (\"{}\"). Context: {file}",
        snap.meta.label
    );
    Ok(())
}

/// Internal hook target for Claude Code's PreCompact / SubagentStop events. Reads the hook's
/// JSON from stdin, maps the live session's `transcript_path` to its indexed thread, and saves
/// a snapshot so context survives the compaction / the finished subagent's work is captured.
/// Best-effort and SILENT: a hook must never break the agent loop, so every failure (no index,
/// session not indexed yet, bad payload) just exits 0 without output.
fn cmd_snapshot_hook(_args: &[String]) -> anyhow::Result<()> {
    use std::io::Read;
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return Ok(());
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&input) else {
        return Ok(());
    };
    let Some(transcript) = v.get("transcript_path").and_then(|x| x.as_str()) else {
        return Ok(());
    };
    let event = v
        .get("hook_event_name")
        .and_then(|x| x.as_str())
        .unwrap_or("hook");
    let Some(external_id) = claude_external_id(transcript) else {
        return Ok(());
    };
    // Only act when an index already exists — never create one from inside a hook.
    if !db::default_index_path().exists() {
        return Ok(());
    }
    let conn = open_db_write()?;
    let thread_id: Option<i64> = conn
        .query_row(
            "SELECT t.id FROM threads t JOIN sources s ON s.id = t.source_id
             WHERE s.kind = 'claude_code' AND t.external_id = ?1",
            [&external_id],
            |r| r.get(0),
        )
        .ok();
    if let Some(tid) = thread_id {
        // Ignore errors: a failed auto-snapshot must not surface to the agent.
        let _ = snapshot::create_rolling_auto(&conn, tid, event);
    }
    Ok(())
}

/// Map a Claude Code hook's absolute `transcript_path` back to the indexed thread's
/// `external_id` (the path relative to `~/.claude/projects`).
fn claude_external_id(transcript_path: &str) -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    external_id_under(
        transcript_path,
        &std::path::Path::new(&home).join(".claude/projects"),
    )
}

/// The path of `transcript_path` relative to `base`, as a string (the indexed `external_id`).
fn external_id_under(transcript_path: &str, base: &std::path::Path) -> Option<String> {
    std::path::Path::new(transcript_path)
        .strip_prefix(base)
        .ok()?
        .to_str()
        .map(str::to_string)
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
}
