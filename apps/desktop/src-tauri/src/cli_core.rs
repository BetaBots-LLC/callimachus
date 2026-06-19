//! `cal` CLI core — the search/recent/cat/stats/export logic, factored into the
//! library so it has two entry points: the standalone `cal` binary, and the
//! desktop app when invoked as `cal` (argv0) or with a cal subcommand. That lets
//! the installer symlink the app itself to `~/.local/bin/cal` — no separate
//! binary to ship. Reads the same local index.db as the GUI and MCP server.
//!
//! Set CALLIMACHUS_DB to point at a specific index.db; CALLIMACHUS_VAULT to a
//! default Obsidian vault for `cal export`.

use crate::{agent, context, db, embed, export, knowledge, search, secrets};
use rusqlite::Connection;

/// Subcommands that identify a `cal` invocation when the app is launched directly.
pub const COMMANDS: &[&str] = &[
    "search", "related", "recent", "cat", "show", "context", "stats", "export", "star", "tag",
    "tags", "todos", "knowledge", "distill",
];

const USAGE: &str = "\
cal — search your indexed AI coding-agent history

USAGE:
  cal search <query…> [-s SOURCE] [-y|--hybrid] [-n LIMIT] [--json]
  cal related [<text…>] [-s SOURCE] [-p PROJECT] [-n LIMIT] [--json]
                                (text via args or stdin; semantic only)
  cal recent [-s SOURCE] [-p PROJECT] [--starred] [-t TAG] [-n LIMIT] [--json]
  cal cat <thread-id>            (aliases: show, context)
  cal stats [--json]
  cal export <thread-id> [--vault DIR] [--out FILE] [-S|--synthesize]
  cal star <thread-id> [--off]   star a thread (--off to unstar)
  cal tag <thread-id> [<tag…>]   set a thread's tags (no tags = clear them)
  cal tags [--json]              list all tags with thread counts
  cal todos [-p PROJECT] [-s SOURCE] [-n LIMIT] [--json]
                                open TODOs found across your history
  cal knowledge <thread-id> [--json]
                                distilled summary/decisions/gotchas for a thread
  cal distill <thread-id>       extract knowledge for a thread (needs distillation
                                enabled in the app: local Ollama or an API key)
  cal help

OPTIONS:
  -s, --source SOURCE   filter by source kind (claude_code, codex, cursor,
                        gemini, qwen, goose, opencode, continue, cline, in_app)
  -p, --project PATH    substring-match the project path
      --starred         only starred threads (recent/related/search)
  -t, --tag TAG         only threads with this tag (repeatable)
  -y, --hybrid          fuse keyword + on-device semantic search
  -n, --limit N         max results (default 20)
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
                o.limit = Some(next(&mut it, "--limit")?.parse().map_err(|_| {
                    anyhow::anyhow!("--limit needs a number")
                })?)
            }
            "-V" | "--vault" => o.vault = Some(next(&mut it, "--vault")?),
            "-o" | "--out" => o.out = Some(next(&mut it, "--out")?),
            "-y" | "--hybrid" => o.hybrid = true,
            "-S" | "--synthesize" => o.synthesize = true,
            "-t" | "--tag" => o.tags.push(next(&mut it, "--tag")?),
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
    it.next().cloned().ok_or_else(|| anyhow::anyhow!("{flag} needs a value"))
}

fn open_db() -> anyhow::Result<Connection> {
    let path = db::default_index_path();
    if !path.exists() {
        anyhow::bail!(
            "no index found at {} — open the Callimachus app once to build it, or set CALLIMACHUS_DB",
            path.display()
        );
    }
    db::open(&path)
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
        println!("[{}] {:<11} {}  ({} msgs)", t.id, t.source, title, t.message_count);
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
    let conn = open_db()?;
    search::set_star(&conn, id, !o.off)?;
    eprintln!("thread {id} {}", if o.off { "unstarred" } else { "starred" });
    Ok(())
}

fn cmd_tag(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    let id = thread_id_arg(&o, "tag")?;
    // Tags are the positionals after the id; passing none clears the thread's tags.
    let tags: Vec<String> = o.positional[1..].to_vec();
    let mut conn = open_db()?;
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
    let todos = knowledge::list_open_todos(
        &conn,
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
    let mut conn = open_db()?;
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

    let pct = if s.embeddable > 0 { s.embedded * 100 / s.embeddable } else { 0 };
    println!("{} threads · {} messages", s.threads, s.messages);
    println!("semantic: {}/{} embedded ({pct}%)", s.embedded, s.embeddable);
    println!("range: {} → {}", fmt_time(s.earliest), fmt_time(s.latest));

    println!("\nby source:");
    for src in &s.per_source {
        if src.threads > 0 || src.messages > 0 {
            println!("  {:<12} {:>6} threads · {:>7} msgs", src.kind, src.threads, src.messages);
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

fn fmt_time(epoch: Option<i64>) -> String {
    match epoch.and_then(|e| chrono::DateTime::from_timestamp(e, 0)) {
        Some(dt) => dt.format("%Y-%m-%d").to_string(),
        None => "—".to_string(),
    }
}
