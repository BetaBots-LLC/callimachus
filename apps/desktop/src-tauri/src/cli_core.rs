//! `cal` CLI core — the search/recent/cat/stats/export logic, factored into the
//! library so it has two entry points: the standalone `cal` binary, and the
//! desktop app when invoked as `cal` (argv0) or with a cal subcommand. That lets
//! the installer symlink the app itself to `~/.local/bin/cal` — no separate
//! binary to ship. Reads the same local index.db as the GUI and MCP server.
//!
//! Set CALLIMACHUS_DB to point at a specific index.db; CALLIMACHUS_VAULT to a
//! default Obsidian vault for `cal export`.

use crate::{
    agent, context, cost, db, embed, export, gitlink, issues, knowledge, ledger, search, secrets,
    snapshot,
};
use rusqlite::Connection;
use std::borrow::Cow;
use std::fmt::Write as _;
use std::mem::ManuallyDrop;

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
    "session-end",
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
                                Code SessionStart hook command; size-capped, ~6k chars),
                                with a recent-sessions digest while it has no memory yet
  cal hook --explain [<path>]   show what a new session in the repo would get, or why
                                the hook stays silent there
  cal hook --mute|--unmute [<path>]
                                turn the recent-sessions digest off / on for one repo
                                (every agent's hook honors it)
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
  -s, --source SOURCE   filter by source kind ({SOURCE_KINDS})
  -p, --project PATH    substring-match the project path
      --starred         only starred threads (recent/related/search)
  -t, --tag TAG         only threads with this tag (repeatable)
  -l, --label LABEL     name for a `snapshot`
  -a, --agent AGENT     target agent CLI for `resume` (default claude)
      --because WHY      rationale for a `remember decision`
  -y, --hybrid          fuse keyword + on-device semantic search
  -n, --limit N         max results (default 20; todos 50, files 40; hook: per kind)
      --budget CHARS    max chars `hook` prints (default 6000)
      --for AGENT       hook protocol: claude (default), codex or gemini
  -V, --vault DIR       Obsidian vault dir for `export` (else CALLIMACHUS_VAULT)
  -o, --out FILE        write `export` output to FILE instead of a vault/stdout
  -S, --synthesize      prepend an LLM summary / decisions / gotchas / TODOs
                        to `export` (uses the first stored provider key)
      --json            machine-readable JSON output
      --raw             show stored text unredacted (query commands redact
                        secrets by default; cat, export and hooks always do)";

/// Where [`usage`] puts the `-s` source kinds.
const SOURCE_KINDS_SLOT: &str = "{SOURCE_KINDS}";
/// The OPTIONS description column, and the width the source kinds wrap to, matching the
/// hand-wrapped lines around them.
const OPTION_DESC_COL: usize = 24;
const USAGE_WIDTH: usize = 76;

/// `cal help`: [`USAGE`] with the `-s` source kinds filled in from `indexer::SOURCES` (plus
/// `in_app`, the app's own chats, which no indexer scans). The registry is the one list to
/// extend when adding a source, so a hand-written copy here would drift (it once lacked
/// copilot).
fn usage() -> String {
    let kinds: Vec<&str> = crate::indexer::SOURCES
        .iter()
        .map(|s| s.kind)
        .chain(["in_app"])
        .collect();
    let at = USAGE
        .find(SOURCE_KINDS_SLOT)
        .expect("USAGE has the source kinds slot");
    let mut col = at - USAGE[..at].rfind('\n').map_or(0, |nl| nl + 1);
    let mut list = String::new();
    for (i, kind) in kinds.iter().enumerate() {
        // Each kind carries its trailing "," or, for the last one, the closing ")".
        let width = kind.len() + 1;
        if i > 0 {
            if col + 1 + width > USAGE_WIDTH {
                list.push('\n');
                list.push_str(&" ".repeat(OPTION_DESC_COL));
                col = OPTION_DESC_COL;
            } else {
                list.push(' ');
                col += 1;
            }
        }
        list.push_str(kind);
        if i + 1 < kinds.len() {
            list.push(',');
        }
        col += width;
    }
    USAGE.replacen(SOURCE_KINDS_SLOT, &list, 1)
}

/// Hook targets Claude Code runs inside the agent loop (SessionStart, PreCompact,
/// UserPromptSubmit, SessionEnd). They must never fail visibly: Claude Code shows a non-zero
/// exit plus stderr as a hook error on every session start or prompt.
const HOOK_COMMANDS: &[&str] = &["hook", "snapshot-hook", "recall-now", "session-end"];

/// Run the CLI and map the result to a process exit code (printing errors). Used
/// by both the `cal` binary and the app's cal-mode dispatch.
pub fn run_and_exit(args: &[String]) -> i32 {
    if args
        .first()
        .is_some_and(|c| HOOK_COMMANDS.contains(&c.as_str()))
        && !is_interactive_hook(args)
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
        println!("{}", usage());
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
        "session-end" => cmd_session_end(rest),
        "audit-pr" => cmd_audit_pr(rest),
        // A cheap, explicit signature for tools probing whether a `cal` on PATH is ours
        // (the VS Code extension; the system calendar's `cal` prints a calendar instead).
        "version" | "--version" | "-v" => {
            println!("callimachus {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "help" | "-h" | "--help" => {
            println!("{}", usage());
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
    /// `--for`: which agent's hook protocol a hook command speaks (see [`HookAgent`]).
    for_agent: Option<String>,
    /// `--raw`: print stored text as is instead of redacting secrets (see [`Opts::shown`]).
    raw: bool,
    positional: Vec<String>,
}

/// The output boundary for stored history. Query commands print titles, snippets, facts and
/// error text straight from the index, which keeps transcripts unredacted, and agents run
/// them too: the /recall skill falls back to `cal` when MCP isn't wired, and the VS Code
/// extension reads `--json`. So everything printed goes through these two helpers, redacted
/// unless the user passed `--raw` for their own terminal. `cat`, `export` and the hooks
/// redact on their own (packed transcripts, `export::fact_line`) and never honor `--raw`.
impl Opts {
    /// Stored text for human output: secrets redacted unless `--raw`.
    fn shown<'a>(&self, text: &'a str) -> Cow<'a, str> {
        if self.raw {
            Cow::Borrowed(text)
        } else {
            context::redact_secrets(text)
        }
    }

    /// A `--json` payload, pretty-printed, with every string redacted unless `--raw`.
    fn json<T: serde::Serialize + ?Sized>(&self, value: &T) -> anyhow::Result<String> {
        if self.raw {
            return Ok(serde_json::to_string_pretty(value)?);
        }
        // `to_redacted_json` writes compact JSON; re-indent it so `--json` prints exactly as
        // before. Lossless: serde_json keeps key order (preserve_order), and every number
        // parses back to the value whose shortest form it was printed as.
        let compact = context::to_redacted_json(value)?;
        let v: serde_json::Value = serde_json::from_str(&compact)?;
        Ok(serde_json::to_string_pretty(&v)?)
    }
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
            "--for" => o.for_agent = Some(next(&mut it, "--for")?),
            "--starred" => o.starred = true,
            "--off" => o.off = true,
            "--json" => o.json = true,
            "--raw" => o.raw = true,
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

/// [`open_db_write`] for a hook: never creates the index and never migrates it (an
/// upgrade can hold the lock for tens of seconds, past every hook's budget, so it is left to
/// the app or a `cal` command the user runs), and waits at most `wait` for the lock. A
/// schema mismatch or a busy index is an error the hook swallows (`run_hook_silently`).
fn open_hook_db_write(wait: std::time::Duration) -> anyhow::Result<Connection> {
    db::open_sidecar_current_with(&db::default_index_path(), wait.min(HOOK_DB_WAIT))
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
fn cli_embedder() -> ManuallyDrop<embed::Embedder> {
    if !embed::model_is_cached() {
        eprintln!(
            "note: semantic search is unavailable until the embedding model is downloaded \
             (open Callimachus and build the semantic index); using keyword matches only"
        );
    }
    one_shot_embedder()
}

/// A cached-only embedder that is never dropped. Every `cal` command is a one-shot process
/// that exits right after it prints, and tearing down a loaded model (the ONNX session and
/// its thread pool) only spends ~3 ms freeing memory the OS reclaims at exit anyway. That
/// adds up on `recall-now`, which runs on every prompt, hit or not. Only for `cal`: the app
/// and the MCP server are long-lived and must drop theirs.
fn one_shot_embedder() -> ManuallyDrop<embed::Embedder> {
    ManuallyDrop::new(embed::Embedder::cached_only())
}

fn cmd_search(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    if o.positional.is_empty() {
        anyhow::bail!("search needs a query. e.g. `cal search \"vector index\"`");
    }
    let conn = open_db()?;
    print!("{}", search_output(&conn, &o)?);
    Ok(())
}

/// `cal search`'s stdout (the no-matches note goes to stderr).
fn search_output(conn: &Connection, o: &Opts) -> anyhow::Result<String> {
    let query = o.positional.join(" ");
    let f = filters(o);
    let mut hits = if o.hybrid {
        let embedder = cli_embedder();
        search::hybrid(conn, &embedder, &query, &f)?
    } else {
        search::search(conn, &query, &f)?
    };

    if o.json {
        if !o.raw {
            // JSON keeps the FTS match sentinels (the VS Code sidebar highlights with them),
            // but one landing inside a key splits it so the redaction pattern no longer sees
            // it. A snippet hiding a secret that way goes out plain, without highlights.
            for h in &mut hits {
                if let Cow::Owned(clean) = context::redact_secrets(&strip_marks(&h.snippet)) {
                    h.snippet = clean;
                }
            }
        }
        return Ok(format!("{}\n", o.json(&hits)?));
    }
    if hits.is_empty() {
        eprintln!("no matches for {query:?}");
        return Ok(String::new());
    }
    let mut out = String::new();
    for h in &hits {
        let title = h.title.as_deref().unwrap_or("(untitled)");
        writeln!(out, "[{}] {:<11} {}", h.thread_id, h.source, o.shown(title))?;
        // Marks out first, so one inside a key can't hide it from the redaction.
        writeln!(out, "    {}", o.shown(&strip_marks(&h.snippet)))?;
    }
    Ok(out)
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
        println!("{}", o.json(&rows)?);
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
            t.id,
            t.source,
            o.shown(title),
            t.message_count
        );
    }
    Ok(())
}

fn cmd_recent(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    let conn = open_db()?;
    print!("{}", recent_output(&conn, &o)?);
    Ok(())
}

/// `cal recent`'s stdout (the empty-index note goes to stderr).
fn recent_output(conn: &Connection, o: &Opts) -> anyhow::Result<String> {
    let rows = search::recent_threads(conn, &filters(o))?;

    if o.json {
        return Ok(format!("{}\n", o.json(&rows)?));
    }
    if rows.is_empty() {
        eprintln!("no threads indexed yet");
        return Ok(String::new());
    }
    let mut out = String::new();
    for t in &rows {
        let title = t.title.as_deref().unwrap_or("(untitled)");
        writeln!(
            out,
            "[{}] {:<11} {}  ({} msgs · {})",
            t.id,
            t.source,
            o.shown(title),
            t.message_count,
            fmt_time(t.updated_at)
        )?;
    }
    Ok(out)
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
        println!("{}", o.json(&tags)?);
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
        println!("{}", o.json(&todos)?);
        return Ok(());
    }
    if todos.is_empty() {
        eprintln!("no open todos found");
        return Ok(());
    }
    for t in &todos {
        let title = t.title.as_deref().unwrap_or("untitled");
        println!("• {}", o.shown(&t.text));
        println!(
            "    {} · {} · thread {}",
            t.source,
            o.shown(title),
            t.thread_id
        );
    }
    Ok(())
}

fn print_knowledge(k: &knowledge::ThreadKnowledge, o: &Opts) {
    if let Some(s) = &k.summary {
        println!("Summary: {}\n", o.shown(s));
    }
    if !k.decisions.is_empty() {
        println!("Decisions:");
        for f in &k.decisions {
            println!("  • {}", o.shown(&f.text));
        }
        println!();
    }
    if !k.gotchas.is_empty() {
        println!("Gotchas:");
        for f in &k.gotchas {
            println!("  • {}", o.shown(&f.text));
        }
        println!();
    }
    if !k.todos.is_empty() {
        println!("TODOs:");
        for f in &k.todos {
            println!("  • {}", o.shown(&f.text));
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
        println!("{}", o.json(&k)?);
        return Ok(());
    }
    // Judge by content, not `extracted`: a new turn clears the flag but keeps the facts, and
    // `print_knowledge` flags them as stale.
    if k.summary.is_none() && k.decisions.is_empty() && k.gotchas.is_empty() && k.todos.is_empty() {
        eprintln!("no knowledge for thread {id} yet, run `cal distill {id}`");
        return Ok(());
    }
    print_knowledge(&k, &o);
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
    print_knowledge(&knowledge::get_thread_knowledge(&conn, id)?, &o);
    Ok(())
}

/// The project key for the cwd (`knowledge::cwd_project_key`, shared with the MCP server):
/// its git root with worktrees resolved to their main repo and the walk stopping at $HOME,
/// else the cwd itself. For memory lookups and write-backs.
fn cwd_project_root() -> String {
    knowledge::cwd_project_key().unwrap_or_default()
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
    let project = project_arg(&o);
    let conn = open_db()?;
    let mem = knowledge::get_project_memory(&conn, &project, o.limit.unwrap_or(40) as usize)?;
    if o.json {
        println!("{}", o.json(&mem)?);
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
            println!("  - {}", o.shown(f.text.trim()));
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

/// Resolve the project arg (positional or cwd) to its canonical key, so it matches however
/// the threads were indexed.
fn project_arg(o: &Opts) -> String {
    if o.positional.is_empty() {
        cwd_project_root()
    } else {
        knowledge::project_key(&o.positional.join(" "))
    }
}

/// `cal hook`: the SessionStart hook target. Prints the repo's memory, size-capped, for
/// Claude Code to inject. Reads the hook payload from stdin (when there is one) to decide
/// how much to send:
/// - `resume`: nothing, the resumed transcript already holds the earlier injection;
/// - `compact`: a short refresher (`MemoryBudget::COMPACT`), facts only, since compaction
///   summarized the original injection away and the session is its own recent activity;
/// - `startup` / `clear` / no payload: `MemoryBudget::HOOK`, plus the recent-sessions
///   digest (`export::session_digest`): on its own when the repo has no decisions, gotchas
///   or pinned TODOs yet, appended when the facts leave room for it.
///
/// Also silent when the memory is already in context through a file Claude Code loads
/// holding the Callimachus managed block ([`managed_block_source`]), and when a launcher
/// that already put memory in the context file set `CALLIMACHUS_SKIP_HOOK=1`. Errors are
/// swallowed by `run_hook_silently`.
///
/// Interactive flags (errors shown, see [`is_interactive_hook`]): `--mute [path]` /
/// `--unmute [path]` toggle the digest for one repo, `--explain [path]` prints what a
/// `startup` session there would get, or why it would get nothing.
fn cmd_hook(args: &[String]) -> anyhow::Result<()> {
    let mut action = None;
    let rest: Vec<String> = args
        .iter()
        .filter(|a| {
            let interactive = HOOK_INTERACTIVE_FLAGS.contains(&a.as_str());
            if interactive {
                action = Some(a.to_string());
            }
            !interactive
        })
        .cloned()
        .collect();
    let o = parse(&rest)?;
    let agent = HookAgent::from_opts(&o)?;
    match action.as_deref() {
        Some("--mute") => return cmd_hook_mute(&o, true),
        Some("--unmute") => return cmd_hook_mute(&o, false),
        Some("--explain") => return cmd_hook_explain(&o, agent),
        _ => {}
    }
    let payload = read_hook_payload();
    let source = payload
        .as_ref()
        .and_then(|v| v.get("source"))
        .and_then(|x| x.as_str())
        .unwrap_or("startup");
    let cwd = hook_cwd(payload.as_ref());
    let project = if o.positional.is_empty() {
        knowledge::project_key(&cwd.to_string_lossy())
    } else {
        knowledge::project_key(&o.positional.join(" "))
    };
    let req = HookRequest {
        agent,
        source,
        cwd: &cwd,
        project: &project,
        limit: o.limit,
        max_chars: o.budget,
    };
    if let HookOutcome::Print(text) = hook_outcome(&req, &db::default_index_path())? {
        println!("{}", hook_stdout(agent, &text));
        if counts_as_session(source) {
            ledger::append(hook_ledger_kind(&text), &project);
        }
    }
    Ok(())
}

/// Whether an injection counts in the value ledger, whose Stats line reads "sessions that
/// started with injected memory": a new session (`startup`) or a fresh context (`clear`). A
/// `compact` refresher re-injects into a session already counted, so three compactions would
/// otherwise count one session four times.
fn counts_as_session(source: &str) -> bool {
    matches!(source, "startup" | "clear")
}

/// The agent whose hook protocol a hook command speaks (`--for`, default `claude`). The
/// memory and digest are the same for all three; what differs is the stdout envelope, the
/// context files that may already carry the managed block, whether harvested Claude Code
/// memory is left out, and the prefix a SessionEnd queues the session under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookAgent {
    Claude,
    Codex,
    Gemini,
}

impl HookAgent {
    fn from_opts(o: &Opts) -> anyhow::Result<Self> {
        match o.for_agent.as_deref() {
            None | Some("claude") => Ok(Self::Claude),
            Some("codex") => Ok(Self::Codex),
            Some("gemini") => Ok(Self::Gemini),
            Some(other) => anyhow::bail!("--for takes claude, codex or gemini, not '{other}'"),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::Gemini => "Gemini CLI",
        }
    }

    /// The `knowledge::enqueue_distill_priority` reference prefix for this agent's sessions.
    fn session_prefix(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
        }
    }
}

/// What a SessionStart hook prints for `text`, per agent. Claude Code adds plain stdout as
/// context. Codex does too (developers.openai.com/codex/hooks: plain stdout is added as
/// developer context for SessionStart), and plain is the safe choice there: Codex runs hook
/// commands under a login shell (`$SHELL -lc`, command_runner.rs `build_command`), so
/// anything the user's profile prints lands on stdout ahead of ours. With its JSON envelope
/// that noise fails Codex's `looks_like_json` check and the escaped envelope itself would be
/// injected; plain text only gains the noise in front. Gemini CLI parses stdout as exactly
/// one JSON object (plain text becomes a `systemMessage` shown to the user, not context), so
/// nothing else may ever reach its stdout; the silent paths print nothing, which Gemini
/// accepts as a success with no output (hookRunner.ts: an empty stdout skips parsing,
/// verified 2026-11-06).
fn hook_stdout(agent: HookAgent, text: &str) -> String {
    match agent {
        HookAgent::Claude | HookAgent::Codex => text.to_string(),
        HookAgent::Gemini => serde_json::json!({
            "hookSpecificOutput": { "additionalContext": text }
        })
        .to_string(),
    }
}

/// Which ledger kind an injected hook output counts as: the digest alone (a repo with no
/// memory yet) or project memory (facts, with or without a digest appended).
fn hook_ledger_kind(text: &str) -> ledger::Kind {
    if text.starts_with(export::DIGEST_HEADING) {
        ledger::Kind::Digest
    } else {
        ledger::Kind::Hook
    }
}

/// `cal hook` flags a person types, so their errors print instead of being swallowed.
const HOOK_INTERACTIVE_FLAGS: &[&str] = &["--mute", "--unmute", "--explain"];

/// `cal hook --mute|--unmute|--explain`: run as a normal command, not a silent hook.
fn is_interactive_hook(args: &[String]) -> bool {
    args.first().is_some_and(|c| c == "hook")
        && args[1..]
            .iter()
            .any(|a| HOOK_INTERACTIVE_FLAGS.contains(&a.as_str()))
}

/// What one SessionStart hook run is asked to do.
struct HookRequest<'a> {
    /// Whose hook this is: decides the context files checked and the harvested-memory rule.
    agent: HookAgent,
    source: &'a str,
    /// The session's working directory: decides which context files Claude Code loads.
    cwd: &'a std::path::Path,
    /// The canonical project key the memory and digest are scoped to.
    project: &'a str,
    /// `-n`: facts per kind.
    limit: Option<u32>,
    /// `--budget`: max chars printed.
    max_chars: Option<usize>,
}

/// The hook's result: the text it prints, or why it prints nothing.
#[derive(Debug, PartialEq, Eq)]
enum HookOutcome {
    Print(String),
    Silent(String),
}

/// The hook's output for `req` against the index at `index`. The checks that need no
/// database run first; the index is opened read-only and never created.
fn hook_outcome(req: &HookRequest, index: &std::path::Path) -> anyhow::Result<HookOutcome> {
    // A forked session starts from a copy of its parent's transcript, earlier injection
    // included, just as a resumed one does.
    if matches!(req.source, "resume" | "fork") {
        return Ok(HookOutcome::Silent(format!(
            "{} session: the transcript already holds the earlier injection",
            if req.source == "fork" {
                "forked"
            } else {
                "resumed"
            }
        )));
    }
    if req.source == "startup" && std::env::var("CALLIMACHUS_SKIP_HOOK").as_deref() == Ok("1") {
        return Ok(HookOutcome::Silent(
            "CALLIMACHUS_SKIP_HOOK=1 is set (a launcher already put the memory in context)".into(),
        ));
    }
    if let Some(file) = managed_block_for(req.agent, req.cwd) {
        return Ok(HookOutcome::Silent(format!(
            "managed block found in {} ({} already loads this memory)",
            file.display(),
            req.agent.label()
        )));
    }
    // Never create an index from inside a hook.
    if !index.exists() {
        return Ok(HookOutcome::Silent(format!(
            "no index at {} (open the Callimachus app once to build it)",
            index.display()
        )));
    }
    let conn = db::open_readonly(index)?;
    hook_output(&conn, req)
}

/// Remaining chars past the facts below which the hook appends no digest: three sessions
/// need about 900 (the header plus ~250 per session).
const HOOK_DIGEST_MIN_ROOM: usize = 900;

/// [`hook_outcome`] once the index is open: the facts, the digest, or the reason for
/// silence.
fn hook_output(conn: &Connection, req: &HookRequest) -> anyhow::Result<HookOutcome> {
    let Some(mut budget) = hook_budget(req.source) else {
        return Ok(HookOutcome::Silent("resumed session".into()));
    };
    if let Some(n) = req.limit {
        budget.max_decisions = n as usize;
        budget.max_gotchas = n as usize;
    }
    if let Some(chars) = req.max_chars {
        budget.max_chars = chars;
    }
    let per_kind = budget
        .fetch_limit()
        .max(budget.max_decisions)
        .max(budget.max_gotchas);
    // Facts only: the budgeted renderer never prints the N/M-distilled coverage counts.
    // Claude Code loads its own auto memory every session; injecting the harvested copy
    // would say the same thing twice. MCP, `cal memory` and the Projects tab keep them.
    // Filtered in SQL, so a repo full of recent auto memories still fills the window with
    // its other facts. Codex and Gemini CLI never see Claude's auto memory, so for them the
    // harvested facts are the point, labeled with where they came from.
    let mem = if req.agent == HookAgent::Claude {
        knowledge::get_project_memory_facts_excluding(
            conn,
            req.project,
            per_kind,
            knowledge::harvest::EXTRACTOR_CLAUDE_MEMORY,
        )?
    } else {
        label_harvested(knowledge::get_project_memory_facts(
            conn,
            req.project,
            per_kind,
        )?)
    };
    let pinned_todo = mem.open_todos.iter().any(|t| t.pinned);
    let has_facts = !mem.decisions.is_empty() || !mem.gotchas.is_empty() || pinned_todo;
    // A compacted session is its own recent activity: facts only.
    let wants_digest = req.source != "compact";
    let digest_off = !export::hook_settings::digest_enabled(conn)?;
    let muted = export::hook_settings::is_muted(conn, req.project)?;

    if has_facts {
        let mut out = export::agent_memory_md_budgeted(req.project, &mem, None, &budget);
        let room = budget
            .max_chars
            .saturating_sub(out.chars().count() + "\n\n".len());
        if wants_digest && !digest_off && !muted && room >= HOOK_DIGEST_MIN_ROOM {
            let opts = export::DigestOpts {
                max_sessions: 3,
                max_chars: room,
            };
            if let Some(digest) = export::session_digest(conn, req.project, &opts)? {
                out.push_str("\n\n");
                out.push_str(&digest);
            }
        }
        return Ok(HookOutcome::Print(out));
    }
    if !wants_digest {
        return Ok(HookOutcome::Silent(
            "no decisions, gotchas or pinned TODOs (after compaction the hook sends facts only)"
                .into(),
        ));
    }
    if muted {
        return Ok(HookOutcome::Silent(format!(
            "muted: no memory yet and the digest is muted for {} (`cal hook --unmute` to undo)",
            req.project
        )));
    }
    if digest_off {
        return Ok(HookOutcome::Silent(
            "digest off: no memory yet and the recent-sessions digest is off (Settings > Agents)"
                .into(),
        ));
    }
    let opts = export::DigestOpts {
        max_sessions: 5,
        max_chars: budget.max_chars.min(2500),
    };
    Ok(match export::session_digest(conn, req.project, &opts)? {
        Some(digest) => HookOutcome::Print(digest),
        None => HookOutcome::Silent(format!(
            "no threads: nothing indexed for {} yet",
            req.project
        )),
    })
}

/// Prefix on a harvested Claude Code auto-memory fact in another agent's injection, so the
/// agent knows it was written for (and by) a different tool.
const HARVESTED_ORIGIN: &str = "(from Claude Code memory) ";

/// `mem` with each harvested Claude Code memory's text marked with its origin.
fn label_harvested(mut mem: knowledge::ProjectMemory) -> knowledge::ProjectMemory {
    for f in mem
        .decisions
        .iter_mut()
        .chain(mem.gotchas.iter_mut())
        .chain(mem.open_todos.iter_mut())
        .filter(|f| f.extractor == knowledge::harvest::EXTRACTOR_CLAUDE_MEMORY)
    {
        f.text.insert_str(0, HARVESTED_ORIGIN);
    }
    mem
}

/// Claude Code's cap on a hook's injected context, past which the agent sees a preview.
const HOOK_OUTPUT_CAP: usize = 10_000;

/// `cal hook --explain [path] [--for AGENT]`: the exact text a `startup` session in `path`
/// (default: the current directory) would get, before the agent's envelope, then its size,
/// or the one reason it would get nothing.
fn cmd_hook_explain(o: &Opts, agent: HookAgent) -> anyhow::Result<()> {
    let cwd = hook_target_dir(o)?;
    let project = knowledge::project_key(&cwd.to_string_lossy());
    let req = HookRequest {
        agent,
        source: "startup",
        cwd: &cwd,
        project: &project,
        limit: o.limit,
        max_chars: o.budget,
    };
    println!(
        "{}",
        explain_text(&hook_outcome(&req, &db::default_index_path())?)
    );
    Ok(())
}

fn explain_text(outcome: &HookOutcome) -> String {
    match outcome {
        HookOutcome::Print(text) => format!(
            "{text}\n{} chars (cap {HOOK_OUTPUT_CAP})",
            text.chars().count()
        ),
        HookOutcome::Silent(reason) => format!("Silent for a new session: {reason}."),
    }
}

/// The directory a `cal hook --explain|--mute` names (its positional path), else the cwd.
fn hook_target_dir(o: &Opts) -> anyhow::Result<std::path::PathBuf> {
    if o.positional.is_empty() {
        return Ok(std::env::current_dir()?);
    }
    let raw = o.positional.join(" ");
    let p = std::path::PathBuf::from(&raw);
    Ok(std::fs::canonicalize(&p).unwrap_or(p))
}

/// `cal hook --mute|--unmute [path]`: stop or resume the recent-sessions digest for one repo
/// (stored by project key in `hook.muted_projects`). Facts still inject.
fn cmd_hook_mute(o: &Opts, mute: bool) -> anyhow::Result<()> {
    let project = knowledge::project_key(&hook_target_dir(o)?.to_string_lossy());
    let index = db::default_index_path();
    if !index.exists() {
        anyhow::bail!(
            "no index at {}: open the Callimachus app once to build it",
            index.display()
        );
    }
    let conn = open_db_write()?;
    let changed = export::hook_settings::set_muted(&conn, &project, mute)?;
    println!("{}", mute_message(&project, mute, changed));
    Ok(())
}

fn mute_message(project: &str, mute: bool, changed: bool) -> String {
    match (mute, changed) {
        (true, true) => format!(
            "Muted the recent-sessions digest for {project}. Undo with `cal hook --unmute`."
        ),
        (true, false) => format!("The digest was already muted for {project}."),
        (false, true) => format!("Unmuted the recent-sessions digest for {project}."),
        (false, false) => format!("The digest was not muted for {project}."),
    }
}

/// The memory budget for a SessionStart `source`, or None to stay silent.
fn hook_budget(source: &str) -> Option<export::MemoryBudget> {
    match source {
        // Both carry the parent transcript, with its earlier injection.
        "resume" | "fork" => None,
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

/// The context file that already carries the Callimachus managed block for a session in
/// these dirs, in which case the hook would inject the same memory a second time. Mirrors
/// what Claude Code loads: every `CLAUDE.md`, `.claude/CLAUDE.md` and `CLAUDE.local.md` in
/// the dirs (and the files they `@`-import); and only when none of those exists, the dirs'
/// `AGENTS.md`, which Claude Code (v2.1.277+) reads as project instructions only then. The
/// user-scope `~/.claude/CLAUDE.md` is checked for the block but is not a project file, so
/// it does not hide an `AGENTS.md`.
fn managed_block_source<P: AsRef<std::path::Path>>(dirs: &[P]) -> Option<std::path::PathBuf> {
    let homes: Vec<std::path::PathBuf> = dirs::home_dir()
        .into_iter()
        .flat_map(|h| [std::fs::canonicalize(&h).ok(), Some(h)])
        .flatten()
        .collect();
    let mut checked = std::collections::HashSet::new();
    let mut project_file_exists = false;
    for dir in dirs {
        let dir = dir.as_ref();
        for rel in ["CLAUDE.md", ".claude/CLAUDE.md", "CLAUDE.local.md"] {
            let path = dir.join(rel);
            if !checked.insert(path.clone()) || !path.is_file() {
                continue;
            }
            let user_scope = rel == ".claude/CLAUDE.md" && homes.iter().any(|h| h == dir);
            project_file_exists |= !user_scope;
            let Some(text) = read_small_text(&path) else {
                continue;
            };
            if text.contains(export::MANAGED_START) {
                return Some(path);
            }
            let base = path.parent().unwrap_or(dir);
            for import in claude_md_imports(&text).into_iter().take(10) {
                let target = resolve_import(base, import);
                if read_small_text(&target).is_some_and(|t| t.contains(export::MANAGED_START)) {
                    return Some(target);
                }
            }
        }
    }
    if project_file_exists {
        return None;
    }
    dirs.iter()
        .map(|d| d.as_ref().join("AGENTS.md"))
        .find(|p| read_small_text(p).is_some_and(|t| t.contains(export::MANAGED_START)))
}

/// The context file that already carries the managed block for `agent`'s session in `cwd`
/// (so the hook stays silent instead of injecting the memory twice).
fn managed_block_for(agent: HookAgent, cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    match agent {
        HookAgent::Claude => managed_block_source(&claude_md_dirs(cwd)),
        HookAgent::Codex => {
            let global = crate::indexer::codex::codex_root();
            let names = codex_context_files(global.as_deref());
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            context_block_source(
                &project_dirs(cwd),
                global.as_deref(),
                &names,
                ContextRule::FirstExisting,
            )
        }
        HookAgent::Gemini => {
            let global = dirs::home_dir().map(|h| h.join(".gemini"));
            let names = global
                .as_deref()
                .map(gemini_context_files)
                .unwrap_or_else(|| vec!["GEMINI.md".into()]);
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            context_block_source(
                &project_dirs(cwd),
                global.as_deref(),
                &names,
                ContextRule::EveryFileWithImports,
            )
        }
    }
}

/// Codex's instruction files, by precedence: in each dir (and in `CODEX_HOME`) an
/// `AGENTS.override.md` is read instead of the `AGENTS.md` beside it.
const CODEX_CONTEXT_FILES: [&str; 2] = ["AGENTS.override.md", "AGENTS.md"];

/// [`CODEX_CONTEXT_FILES`], then the names `project_doc_fallback_filenames` in
/// `<CODEX_HOME>/config.toml` adds (Codex reads one of those, CLAUDE.md say, in a dir with no
/// AGENTS.md; `cal agents -o CLAUDE.md` puts the block there).
fn codex_context_files(codex_home: Option<&std::path::Path>) -> Vec<String> {
    let mut names: Vec<String> = CODEX_CONTEXT_FILES.iter().map(|n| n.to_string()).collect();
    let fallbacks = codex_home
        .and_then(|h| read_small_text(&h.join("config.toml")))
        .and_then(|t| t.parse::<toml_edit::DocumentMut>().ok())
        .and_then(|doc| {
            let list = doc
                .get("project_doc_fallback_filenames")?
                .as_array()?
                .clone();
            Some(
                list.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect::<Vec<_>>(),
            )
        })
        .unwrap_or_default();
    for name in fallbacks {
        if !name.is_empty() && !name.contains(['/', '\\']) && !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// Gemini CLI's context file names: `context.fileName` (a string or a list) in the user's
/// `settings.json`, else `GEMINI.md`. A user who points it at `AGENTS.md` loads the block
/// `cal agents` writes there. Gemini strips comments from settings.json before parsing, so
/// a commented file is read the same way.
fn gemini_context_files(gemini_dir: &std::path::Path) -> Vec<String> {
    let configured = read_small_text(&gemini_dir.join("settings.json"))
        .and_then(|t| {
            serde_json::from_str::<serde_json::Value>(&t)
                .ok()
                .or_else(|| serde_json::from_str(&crate::integration::strip_json_comments(&t)).ok())
        })
        .and_then(|v| v.get("context")?.get("fileName").cloned());
    let names: Vec<String> = match configured {
        Some(serde_json::Value::String(s)) => vec![s],
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    let names: Vec<String> = names
        .into_iter()
        .filter(|n| !n.is_empty() && !n.contains(['/', '\\']))
        .collect();
    if names.is_empty() {
        vec!["GEMINI.md".into()]
    } else {
        names
    }
}

/// The dirs Codex and Gemini CLI read project instructions from for a session in `cwd`: cwd
/// up to its git root (both stop there), or cwd alone outside a repo. Resolved spellings,
/// since [`git_root_of`] returns one.
fn project_dirs(cwd: &std::path::Path) -> Vec<std::path::PathBuf> {
    let start = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let root = git_root_of(&start);
    let mut dirs = Vec::new();
    for dir in start.ancestors() {
        dirs.push(dir.to_path_buf());
        if dir == root {
            return dirs;
        }
    }
    // Unreachable in practice (the root is an ancestor of `start`); cwd alone is safe.
    dirs.truncate(1);
    dirs
}

/// How an agent picks among its context file names in one dir.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextRule {
    /// Codex: only the first existing name counts (`AGENTS.override.md` hides `AGENTS.md`).
    FirstExisting,
    /// Gemini CLI: every listed name that exists is loaded (`memoryDiscovery.ts`
    /// `findUpwardGeminiFiles`), with its `@file` imports expanded (`processImports`), so a
    /// GEMINI.md holding only `@AGENTS.md` loads the block `cal agents` wrote in AGENTS.md.
    EveryFileWithImports,
}

/// The context file holding the managed block among `names` in `dirs` and the agent's global
/// dir, read the way `rule` says the agent reads them.
fn context_block_source(
    dirs: &[std::path::PathBuf],
    global: Option<&std::path::Path>,
    names: &[&str],
    rule: ContextRule,
) -> Option<std::path::PathBuf> {
    for dir in dirs.iter().map(std::path::PathBuf::as_path).chain(global) {
        let existing = names.iter().map(|n| dir.join(n)).filter(|p| p.is_file());
        let files: Vec<std::path::PathBuf> = match rule {
            ContextRule::FirstExisting => existing.take(1).collect(),
            ContextRule::EveryFileWithImports => existing.collect(),
        };
        for path in files {
            let Some(text) = read_small_text(&path) else {
                continue;
            };
            if text.contains(export::MANAGED_START) {
                return Some(path);
            }
            if rule == ContextRule::EveryFileWithImports {
                // The same import walk as the CLAUDE.md check in `managed_block_source`.
                let base = path.parent().unwrap_or(dir);
                for import in claude_md_imports(&text).into_iter().take(10) {
                    let target = resolve_import(base, import);
                    if read_small_text(&target).is_some_and(|t| t.contains(export::MANAGED_START)) {
                        return Some(target);
                    }
                }
            }
        }
    }
    None
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
    // Claude Code loads its own auto memory and reads this file too: no second copy.
    let mem = knowledge::get_project_memory_excluding(
        &conn,
        &project,
        budget.fetch_limit(),
        knowledge::harvest::EXTRACTOR_CLAUDE_MEMORY,
    )?;
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
    let project = o
        .project
        .as_deref()
        .map_or_else(cwd_project_root, knowledge::project_key);
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
        let embedder = one_shot_embedder();
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
        println!("{}", o.json(&hits)?);
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
            o.shown(h.text.trim()),
            h.similarity * 100.0
        );
        if let Some(why) = h.rationale.as_deref().filter(|w| !w.is_empty()) {
            println!("    because: {}", o.shown(why.trim()));
        }
        if let Some(title) = h.title.as_deref() {
            println!("    from: {}", o.shown(title));
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
            // A changed thread keeps its old facts with `extracted: false`; `stale` says they
            // predate the latest turns rather than being absent.
            "stale": k.stale,
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
    let mut raw = false;
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
            "--raw" => raw = true,
            s if !s.starts_with('-') && repo.is_none() => repo = Some(s.to_string()),
            _ => {}
        }
        i += 1;
    }
    let repo = repo.unwrap_or_else(cwd_git_root);
    let key = knowledge::project_key(&repo);

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
    // Titles, distilled reasoning, error signatures and memory, all for another agent.
    let out = Opts {
        raw,
        ..Opts::default()
    };
    println!("{}", out.json(&bundle)?);
    Ok(())
}

fn cmd_commits(args: &[String]) -> anyhow::Result<()> {
    let o = parse(args)?;
    let repo = if o.positional.is_empty() {
        cwd_git_root()
    } else {
        o.positional.join(" ")
    };
    let key = knowledge::project_key(&repo);
    let conn = open_db_write()?;
    let n = gitlink::link_project(&conn, &repo)?;
    let rows = gitlink::commit_timeline(&conn, &key, o.limit.unwrap_or(40) as usize)?;
    if o.json {
        println!("{}", o.json(&rows)?);
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
            o.shown(r.subject.as_deref().unwrap_or(""))
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
        println!("{}", o.json(&threads)?);
        return Ok(());
    }
    if threads.is_empty() {
        eprintln!("no threads mention '{path}'");
        return Ok(());
    }
    for t in &threads {
        println!(
            "{:>6}  {}",
            t.id,
            o.shown(t.title.as_deref().unwrap_or("untitled"))
        );
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
    // The context the model saw was redacted, but it can still echo a secret from the
    // question, and the source titles come straight from the index.
    println!("{}\n", o.shown(&answer));
    println!("Sources:");
    for s in &prep.sources {
        println!(
            "  [thread {}] {}",
            s.thread_id,
            o.shown(s.title.as_deref().unwrap_or("untitled"))
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
        println!("{}", o.json(&hits)?);
        return Ok(());
    }
    if hits.is_empty() {
        eprintln!("nothing recalled — distill some threads first (`cal distill <id>`)");
        return Ok(());
    }
    for h in &hits {
        println!("• {}", o.shown(&h.text));
        println!(
            "    {} · thread {} · {:.0}% match",
            o.shown(h.title.as_deref().unwrap_or("untitled")),
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
        println!("{}", o.json(&hits)?);
        return Ok(());
    }
    if hits.is_empty() {
        eprintln!("no prior work found — distill some threads first (`cal distill <id>`)");
        return Ok(());
    }
    for h in &hits {
        println!(
            "• {} · thread {} · {:.0}% match",
            o.shown(h.title.as_deref().unwrap_or("untitled")),
            h.thread_id,
            h.similarity * 100.0
        );
        println!("    {}: {}", h.kind, o.shown(&h.snippet));
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
        println!("{}", o.json(&s)?);
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
    println!("{}", helped_line(&conn));

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

/// `cal stats`' value line from the ledger (`ledger.rs`): hook injections, recall hits and
/// MCP tool calls over the last 7 local days, rolled-up days plus lines the app has not
/// folded yet.
fn helped_line(conn: &Connection) -> String {
    let now = chrono::Utc::now().timestamp();
    let n = ledger::summary(conn, ledger::path_for_conn(conn).as_deref(), now)
        .map(|s| s.week.total())
        .unwrap_or(0);
    format!(
        "helped {n} {} in the last 7 days",
        if n == 1 { "time" } else { "times" }
    )
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
        println!("{}", o.json(&snap)?);
        return Ok(());
    }
    println!(
        "Saved snapshot #{} \"{}\" (~{} tokens). Resume with: cal resume {}",
        snap.id,
        o.shown(&snap.label),
        snap.token_estimate,
        snap.id
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
        println!("{}", o.json(&snaps)?);
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
            o.shown(&s.label)
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
/// rolling snapshot so context survives the compaction. When the payload carries the
/// `compaction_summary`, it is stored as the thread's summary fact (extractor
/// `claude-compact`), so the session's gist is in memory without a distill. Best-effort and
/// SILENT (`run_hook_silently`): a hook must never break the agent loop, and PreCompact's
/// plain stdout becomes compaction instructions, so it prints nothing.
fn cmd_snapshot_hook(args: &[String]) -> anyhow::Result<()> {
    let agent = HookAgent::from_opts(&parse(args)?)?;
    let Some(v) = read_hook_payload() else {
        return Ok(());
    };
    match agent {
        HookAgent::Claude => {}
        HookAgent::Codex => return codex_snapshot_hook(&v),
        // Not installed for Gemini CLI (its PreCompress hook is not wired).
        HookAgent::Gemini => return Ok(()),
    }
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
    // Fail fast rather than holding up the compaction while the app's writer is busy.
    let conn = open_hook_db_write(std::time::Duration::from_secs(2))?;
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
    let Some(tid) = thread_id else {
        return Ok(());
    };
    let snapshot = snapshot::create_rolling_auto(&conn, tid, event).map(|_| ());
    // Independent of the snapshot: either one failing (a busy writer) must not lose the other.
    let summary = v
        .get("compaction_summary")
        .and_then(|x| x.as_str())
        .filter(|s| !s.trim().is_empty());
    if let Some(summary) = summary {
        let session = v.get("session_id").and_then(|x| x.as_str()).unwrap_or("");
        let now = chrono::Utc::now().timestamp();
        knowledge::harvest::store_compaction_summary(&conn, tid, session, summary, now)?;
    }
    snapshot
}

/// `cal snapshot-hook --for codex`, Codex's PreCompact hook: the same rolling snapshot as
/// Claude's, of the codex thread the payload's `transcript_path` (a
/// `rollout-<timestamp>-<session id>.jsonl` file) or `session_id` names. Codex's PreCompact
/// payload carries no compaction summary (only `trigger`), so there is none to store, and its
/// plain stdout is ignored; it prints nothing anyway.
fn codex_snapshot_hook(v: &serde_json::Value) -> anyhow::Result<()> {
    let ids = codex_session_ids(v);
    if ids.is_empty() || !db::default_index_path().exists() {
        return Ok(());
    }
    let conn = open_hook_db_write(std::time::Duration::from_secs(2))?;
    for id in &ids {
        if let Some(tid) = knowledge::priority::codex_thread_for_session(&conn, id)? {
            return snapshot::create_rolling_auto(&conn, tid, "PreCompact").map(|_| ());
        }
    }
    Ok(())
}

/// The Codex session ids a hook payload names, most specific first: the uuid ending the
/// rollout file name in `transcript_path`, then `session_id`.
fn codex_session_ids(v: &serde_json::Value) -> Vec<String> {
    let from_transcript = v
        .get("transcript_path")
        .and_then(|x| x.as_str())
        .and_then(|p| {
            std::path::Path::new(p)
                .file_stem()?
                .to_str()
                .map(str::to_string)
        })
        .and_then(|stem| {
            let tail = stem.get(stem.len().checked_sub(36)?..)?;
            is_uuid(tail).then(|| tail.to_string())
        });
    let from_payload = v
        .get("session_id")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let mut ids: Vec<String> = from_transcript.into_iter().chain(from_payload).collect();
    ids.dedup();
    ids
}

/// `8-4-4-4-12` hex with dashes, the shape of a Codex session id.
fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Internal hook target for Claude Code's SessionEnd event: puts the session that just
/// ended at the front of the auto-distill queue (`knowledge::enqueue_distill_priority`), so
/// its memory lands minutes after it ends instead of after the 30-minute idle wait. The app
/// distills it (the watcher kicks a round once the transcript is indexed); this only writes
/// one `app_config` row. SessionEnd has no `transcript_path` and a 1.5 s budget shared by
/// every SessionEnd hook, so it waits at most 1 s for the lock, skips the write when memory
/// is off, and exits 0 silently on anything else.
///
/// `--for codex|gemini` is the same hook for Codex (SessionEnd: default timeout 1 s, max 3 s)
/// and Gemini CLI (SessionEnd is not awaited), queued as `codex:<session_id>` /
/// `gemini:<session_id>`.
fn cmd_session_end(args: &[String]) -> anyhow::Result<()> {
    let agent = HookAgent::from_opts(&parse(args)?)?;
    let Some(v) = read_hook_payload() else {
        return Ok(());
    };
    if v.get("session_id").and_then(|x| x.as_str()).is_none() || !db::default_index_path().exists()
    {
        return Ok(());
    }
    let conn = open_hook_db_write(std::time::Duration::from_secs(1))?;
    session_end(&conn, &v, chrono::Utc::now().timestamp(), agent)
}

/// [`cmd_session_end`] after the payload is read and the index opened.
fn session_end(
    conn: &Connection,
    payload: &serde_json::Value,
    now: i64,
    agent: HookAgent,
) -> anyhow::Result<()> {
    let Some(session) = payload
        .get("session_id")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
    else {
        return Ok(());
    };
    let cfg = knowledge::get_config(conn)?;
    if !cfg.enabled || !cfg.auto_distill {
        return Ok(());
    }
    knowledge::enqueue_distill_priority(conn, &format!("{}:{session}", agent.session_prefix()), now)
}

/// Map a Claude Code hook's absolute `transcript_path` back to the indexed thread's
/// `external_id`. Must produce byte-identical ids to the indexer (same projects root,
/// honoring `CLAUDE_CONFIG_DIR`, and '/' separators on every OS), or snapshots silently stop
/// matching their thread on Windows and for relocated configs.
fn claude_external_id(transcript_path: &str) -> Option<String> {
    let root = crate::indexer::claude::projects_root()?;
    external_id_resolving(transcript_path, &root)
}

/// [`external_id_under`], retried with symlinks resolved on both sides.
fn external_id_resolving(transcript_path: &str, root: &std::path::Path) -> Option<String> {
    external_id_under(transcript_path, root).or_else(|| {
        // The Settings override is stored canonical (symlinks resolved), but Claude Code
        // reports transcripts under the spelling it was configured with.
        let real = std::fs::canonicalize(transcript_path).ok()?;
        let real_root = std::fs::canonicalize(root).ok()?;
        external_id_under(&real.to_string_lossy(), &real_root)
    })
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
    let embedder = one_shot_embedder();
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
    ledger::append(ledger::Kind::Recall, project.as_deref().unwrap_or(""));
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
        println!("{}", o.json(&found)?);
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
        println!("    {}", o.shown(i.example.trim()));
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
        println!("{}", o.json(&s)?);
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
            o.shown(t.title.as_deref().unwrap_or(""))
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
    fn parse_reads_raw() {
        assert!(parse(&argv(&["--raw", "q"])).unwrap().raw);
        assert!(!parse(&argv(&["q"])).unwrap().raw);
    }

    /// Query commands print stored history, which agents read too (the /recall skill's CLI
    /// fallback, the VS Code extension's `--json`). A planted key comes out redacted in human
    /// and JSON output alike, `--raw` prints it as stored, and the JSON is otherwise
    /// byte-identical to what `--json` printed before.
    #[test]
    fn query_output_redacts_secrets_unless_raw() {
        const KEY: &str = "sk-ant-api03-AbCdEfGhIjKlMnOpQrStUv0123456789";
        const KEY_TAIL: &str = "AbCdEfGhIjKlMnOpQrStUv0123456789";
        let mut conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let sid = crate::indexer::source_id(&conn, "claude_code").unwrap();
        let threads = [
            (
                "leaky",
                format!("wire the gateway with {KEY}"),
                format!("the gateway rejects {KEY} on staging"),
            ),
            (
                "clean",
                "gateway timeouts".to_string(),
                "the gateway times out under load".to_string(),
            ),
        ];
        for (external_id, title, text) in threads {
            let t = crate::indexer::ParsedThread {
                external_id: external_id.into(),
                title: Some(title),
                project_path: Some("/proj/app".into()),
                messages: vec![crate::indexer::ParsedMessage {
                    role: "user".into(),
                    text,
                    tool_name: None,
                    ts: Some(1_000),
                }],
                ..Default::default()
            };
            crate::indexer::upsert_thread(&mut conn, sid, &t).unwrap();
        }
        type Cmd = fn(&Connection, &Opts) -> anyhow::Result<String>;
        let run = |cmd: Cmd, args: &[&str]| cmd(&conn, &parse(&argv(args)).unwrap()).unwrap();
        let assert_scrubbed = |out: &str| {
            assert!(out.contains("[REDACTED]"), "{out}");
            assert!(!out.contains(KEY_TAIL), "{out}");
        };

        let json = run(search_output, &["gateway", "--json"]);
        assert_scrubbed(&json);
        let hits: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
        let leaky = hits
            .iter()
            .find(|h| h["title"] == "wire the gateway with [REDACTED]");
        assert_eq!(
            leaky.expect("the leaky thread matches")["snippet"],
            "the gateway rejects [REDACTED] on staging"
        );
        assert!(
            hits.iter()
                .any(|h| h["snippet"].as_str().unwrap().contains('\u{1}')),
            "a clean snippet keeps its highlights: {json}"
        );
        // A match inside the key splits it with sentinels; the JSON still never carries it.
        assert_scrubbed(&run(search_output, &["api03", "--json"]));

        let text = run(search_output, &["gateway"]);
        assert_scrubbed(&text);
        assert!(text.contains("wire the gateway with [REDACTED]"), "{text}");
        assert!(
            text.contains("    the gateway rejects [REDACTED] on staging"),
            "{text}"
        );

        let recent = run(recent_output, &[]);
        assert_scrubbed(&recent);
        assert!(
            recent.contains("wire the gateway with [REDACTED]"),
            "{recent}"
        );

        // --raw prints the history as stored, and the redacted JSON differs from it only
        // where the key was.
        assert!(run(search_output, &["gateway", "--json", "--raw"]).contains(KEY));
        assert!(run(search_output, &["gateway", "--raw"]).contains(KEY));
        assert!(run(recent_output, &["--raw"]).contains(KEY));
        let raw_json = run(recent_output, &["--json", "--raw"]);
        assert!(raw_json.contains(KEY), "{raw_json}");
        assert_eq!(
            run(recent_output, &["--json"]),
            raw_json.replace(KEY, "[REDACTED]")
        );
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
        assert_eq!(
            hook_budget("fork"),
            None,
            "a fork holds its parent's injection"
        );
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
        assert!(managed_block_source(&[plain.as_path()]).is_none());

        let direct = temp_dir("direct");
        std::fs::create_dir_all(direct.join(".claude")).unwrap();
        std::fs::write(
            direct.join(".claude/CLAUDE.md"),
            export::upsert_managed_block("", "BODY"),
        )
        .unwrap();
        assert!(managed_block_source(&[direct.as_path()]).is_some());

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
        assert!(managed_block_source(&[imported.as_path()]).is_some());

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
        assert!(managed_block_source(&claude_md_dirs(&sibling)).is_none());
        // A nested worktree (or any subdir) does load it as an ancestor.
        assert!(managed_block_source(&claude_md_dirs(&nested)).is_some());
        assert!(claude_md_dirs(&nested).iter().all(|d| d.parent().is_some()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn agents_md_counts_only_when_no_claude_md_exists() {
        // Only AGENTS.md: Claude Code reads it, so its block means the memory is loaded.
        let only = temp_dir("agents_only");
        std::fs::write(
            only.join("AGENTS.md"),
            export::upsert_managed_block("# Agents", "BODY"),
        )
        .unwrap();
        assert_eq!(
            managed_block_source(&[only.as_path()]),
            Some(only.join("AGENTS.md"))
        );

        // A CLAUDE.md (or CLAUDE.local.md) anywhere in the dirs hides AGENTS.md from Claude
        // Code, so the hook must still inject, even from an ancestor.
        for name in ["CLAUDE.md", "CLAUDE.local.md"] {
            let root = temp_dir("agents_hidden");
            let child = root.join("pkg");
            std::fs::create_dir_all(&child).unwrap();
            std::fs::write(root.join(name), "# rules\n").unwrap();
            std::fs::write(
                child.join("AGENTS.md"),
                export::upsert_managed_block("", "BODY"),
            )
            .unwrap();
            assert_eq!(
                managed_block_source(&[child.as_path(), root.as_path()]),
                None,
                "{name}"
            );
            let _ = std::fs::remove_dir_all(&root);
        }

        // CLAUDE.local.md is loaded like CLAUDE.md, so a block in it counts.
        let local = temp_dir("local_block");
        std::fs::write(
            local.join("CLAUDE.local.md"),
            export::upsert_managed_block("", "BODY"),
        )
        .unwrap();
        assert_eq!(
            managed_block_source(&[local.as_path()]),
            Some(local.join("CLAUDE.local.md"))
        );
        for d in [only, local] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn interactive_hook_flags_bypass_the_silent_runner() {
        assert!(is_interactive_hook(&argv(&["hook", "--explain"])));
        assert!(is_interactive_hook(&argv(&["hook", "--mute", "/x"])));
        assert!(!is_interactive_hook(&argv(&["hook"])));
        assert!(!is_interactive_hook(&argv(&["memory", "--mute"])));
        // A typed command shows its error: `--mute` with a bad flag fails visibly.
        assert_eq!(run_and_exit(&argv(&["hook", "--mute", "--nope"])), 1);
    }

    // ---- SessionStart hook output ----

    use crate::export::digest_test_support as dts;

    const HOOK_PROJ: &str = "/Users/me/hook-proj";

    fn hook_req(source: &str) -> HookRequest<'_> {
        HookRequest {
            agent: HookAgent::Claude,
            source,
            cwd: std::path::Path::new("/"),
            project: HOOK_PROJ,
            limit: None,
            max_chars: None,
        }
    }

    fn hook_conn() -> (Connection, i64) {
        let mut conn = dts::temp_db("hook");
        let tid = dts::session(
            &mut conn,
            "s1",
            HOOK_PROJ,
            "Ship the release",
            3600,
            &["cut the release branch and tag it"],
        );
        (conn, tid)
    }

    fn printed(o: HookOutcome) -> String {
        match o {
            HookOutcome::Print(s) => s,
            HookOutcome::Silent(r) => panic!("silent: {r}"),
        }
    }

    fn silent(o: HookOutcome) -> String {
        match o {
            HookOutcome::Silent(r) => r,
            HookOutcome::Print(s) => panic!("printed: {s}"),
        }
    }

    #[test]
    fn hook_falls_back_to_the_digest_when_there_are_no_facts() {
        let (conn, _) = hook_conn();
        let out = printed(hook_output(&conn, &hook_req("startup")).unwrap());
        assert!(out.starts_with("## Recent sessions in this repo"), "{out}");
        assert!(out.contains("gist: cut the release branch and tag it"));
        assert!(out.chars().count() <= 2500);
        // `/clear` gets it too; a compacted session gets facts only, so nothing here.
        assert!(printed(hook_output(&conn, &hook_req("clear")).unwrap()).contains("Recent"));
        assert!(silent(hook_output(&conn, &hook_req("compact")).unwrap()).contains("facts only"));

        // An empty repo stays silent.
        let mut empty = hook_req("startup");
        empty.project = "/Users/me/nothing-here";
        assert!(silent(hook_output(&conn, &empty).unwrap()).starts_with("no threads"));
    }

    #[test]
    fn hook_appends_the_digest_after_facts_when_there_is_room() {
        let (conn, tid) = hook_conn();
        dts::fact(
            &conn,
            tid,
            "gotcha",
            "the release job needs the signing key",
            "llm",
        );
        let out = printed(hook_output(&conn, &hook_req("startup")).unwrap());
        let facts = out.find("**Watch out for:**").expect("facts first");
        let digest = out
            .find("## Recent sessions in this repo")
            .expect("digest after");
        assert!(facts < digest, "{out}");
        assert!(out.chars().count() <= export::MemoryBudget::HOOK.max_chars);

        // Not enough room past the facts: facts only.
        let mut tight = hook_req("startup");
        tight.max_chars = Some(1000);
        let out = printed(hook_output(&conn, &tight).unwrap());
        assert!(
            out.contains("signing key") && !out.contains("Recent sessions"),
            "{out}"
        );

        // After compaction: facts, never the digest.
        let out = printed(hook_output(&conn, &hook_req("compact")).unwrap());
        assert!(
            out.contains("signing key") && !out.contains("Recent sessions"),
            "{out}"
        );
    }

    #[test]
    fn hook_skips_harvested_claude_memory_facts() {
        let (conn, tid) = hook_conn();
        dts::fact(
            &conn,
            tid,
            "decision",
            "harvested from auto memory",
            "claude-memory",
        );
        // Only a harvested fact: treated as no memory, so the digest shows instead.
        let out = printed(hook_output(&conn, &hook_req("startup")).unwrap());
        assert!(!out.contains("harvested from auto memory"), "{out}");
        assert!(out.starts_with("## Recent sessions"), "{out}");
    }

    #[test]
    fn hook_digest_respects_mute_and_the_setting() {
        let (conn, tid) = hook_conn();
        export::hook_settings::set_muted(&conn, HOOK_PROJ, true).unwrap();
        assert!(silent(hook_output(&conn, &hook_req("startup")).unwrap()).starts_with("muted"));
        // Muting drops only the digest: facts still inject.
        dts::fact(&conn, tid, "gotcha", "mind the signing key", "llm");
        let out = printed(hook_output(&conn, &hook_req("startup")).unwrap());
        assert!(
            out.contains("signing key") && !out.contains("Recent sessions"),
            "{out}"
        );
        conn.execute("DELETE FROM facts", []).unwrap();

        export::hook_settings::set_muted(&conn, HOOK_PROJ, false).unwrap();
        export::hook_settings::set_digest_enabled(&conn, false).unwrap();
        assert!(silent(hook_output(&conn, &hook_req("startup")).unwrap()).starts_with("digest off"));
    }

    #[test]
    fn hook_outcome_checks_files_and_the_index_before_opening_it() {
        let dir = temp_dir("outcome");
        let missing = dir.join("no-index.db");
        let mut req = hook_req("startup");
        req.cwd = &dir;
        let reason = silent(hook_outcome(&req, &missing).unwrap());
        assert!(reason.starts_with("no index at"), "{reason}");
        assert!(!missing.exists(), "a hook never creates the index");

        std::fs::write(dir.join("CLAUDE.md"), export::upsert_managed_block("", "B")).unwrap();
        let reason = silent(hook_outcome(&req, &missing).unwrap());
        assert!(reason.starts_with("managed block found in "), "{reason}");
        assert!(reason.contains("CLAUDE.md"), "{reason}");
        assert!(silent(hook_outcome(&hook_req("resume"), &missing).unwrap()).contains("resumed"));
        assert!(silent(hook_outcome(&hook_req("fork"), &missing).unwrap()).contains("forked"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explain_prints_the_output_and_its_size_or_the_reason() {
        let text = "## Recent sessions in this repo\n\n- one";
        assert_eq!(
            explain_text(&HookOutcome::Print(text.into())),
            format!("{text}\n{} chars (cap 10000)", text.chars().count())
        );
        assert_eq!(
            explain_text(&HookOutcome::Silent("digest off: x".into())),
            "Silent for a new session: digest off: x."
        );
        assert!(mute_message("/p", true, true).starts_with("Muted the recent-sessions digest"));
        assert!(mute_message("/p", false, false).contains("was not muted"));
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

    /// A canonical stored root still matches a transcript reported through a symlink.
    #[cfg(unix)]
    #[test]
    fn external_id_matches_through_a_symlinked_config_dir() {
        let dir = temp_dir("extid_symlink");
        let real = dir.join("real");
        std::fs::create_dir_all(real.join("projects/-proj")).unwrap();
        std::fs::write(real.join("projects/-proj/abc.jsonl"), "{}\n").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let root = std::fs::canonicalize(real.join("projects")).unwrap();
        let reported = link.join("projects/-proj/abc.jsonl");
        assert!(external_id_under(reported.to_str().unwrap(), &root).is_none());
        assert_eq!(
            external_id_resolving(reported.to_str().unwrap(), &root).as_deref(),
            Some("-proj/abc.jsonl")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- docs-in-sync guards: keep `cal help` + the README/website from drifting ----

    /// Flags added for `hook` must stay documented in `cal help`.
    #[test]
    fn usage_documents_hook_flags() {
        assert!(USAGE.contains("--budget"));
        assert!(USAGE.contains("cal hook [<project>] [-n PER_KIND] [--budget CHARS]"));
    }

    /// The global `--raw` opt-out stays documented, with the commands that ignore it.
    #[test]
    fn usage_documents_raw_flag() {
        assert!(USAGE.contains("      --raw "), "{USAGE}");
        assert!(USAGE.contains("hooks always do"), "{USAGE}");
    }

    /// `cal help` lists exactly the registered source kinds (plus `in_app`) for `-s`, so a
    /// new source is documented the moment it is registered, wrapped like the lines around it.
    #[test]
    fn usage_lists_every_source_kind() {
        let help = usage();
        assert!(!help.contains(SOURCE_KINDS_SLOT), "{help}");
        let lead = "-s, --source SOURCE   filter by source kind (";
        let start = help.find(lead).unwrap();
        let end = start + help[start..].find(')').unwrap() + 1;
        let block = &help[start..end];
        let listed: Vec<&str> = block[lead.len()..block.len() - 1]
            .split([',', ' ', '\n'])
            .filter(|k| !k.is_empty())
            .collect();
        let registered: Vec<&str> = crate::indexer::SOURCES
            .iter()
            .map(|s| s.kind)
            .chain(["in_app"])
            .collect();
        assert_eq!(listed, registered);
        assert!(listed.contains(&"copilot"));
        let line_start = help[..start].rfind('\n').unwrap() + 1;
        for (i, line) in help[line_start..end].lines().enumerate() {
            assert!(line.len() <= USAGE_WIDTH, "too wide: {line:?}");
            if i > 0 {
                assert!(line.starts_with(&" ".repeat(OPTION_DESC_COL)), "{line:?}");
                assert!(!line[OPTION_DESC_COL..].starts_with(' '), "{line:?}");
            }
        }
    }

    /// SessionEnd queues the session for auto-distill, only while memory and auto-distill
    /// are on (no write otherwise), and ignores a payload without a session id.
    #[test]
    fn session_end_enqueues_the_ended_session() {
        let p = std::env::temp_dir().join(format!("calli_session_end_{}.db", std::process::id()));
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        drop(db::open(&p).unwrap());
        let conn = db::open_sidecar_current(&p).unwrap();
        let payload = serde_json::json!({
            "session_id": "abc-123", "cwd": "/x", "reason": "exit",
            "hook_event_name": "SessionEnd"
        });
        let queue = |conn: &Connection| {
            knowledge::app_config::get(conn, knowledge::priority::PRIORITY_KEY).unwrap()
        };
        session_end(&conn, &payload, 100, HookAgent::Claude).unwrap();
        assert_eq!(queue(&conn), None, "memory off: nothing written");
        knowledge::app_config::put_flag(&conn, "knowledge.enabled", true).unwrap();
        knowledge::set_auto_distill(&conn, true).unwrap();
        session_end(
            &conn,
            &serde_json::json!({ "reason": "exit" }),
            100,
            HookAgent::Claude,
        )
        .unwrap();
        assert_eq!(queue(&conn), None);
        session_end(&conn, &payload, 100, HookAgent::Claude).unwrap();
        let q = queue(&conn).unwrap();
        assert!(q.contains("\"claude:abc-123\""), "{q}");
        // Codex and Gemini CLI sessions queue under their own prefix.
        session_end(&conn, &payload, 101, HookAgent::Codex).unwrap();
        session_end(&conn, &payload, 102, HookAgent::Gemini).unwrap();
        let q = queue(&conn).unwrap();
        assert!(
            q.contains("\"codex:abc-123\"") && q.contains("\"gemini:abc-123\""),
            "{q}"
        );
    }

    /// `--for` picks the protocol; an unknown agent is an error (a hook swallows it, so
    /// nothing prints), and the default is Claude.
    #[test]
    fn hook_agent_comes_from_the_for_flag() {
        let agent = |a: &[&str]| HookAgent::from_opts(&parse(&argv(a)).unwrap());
        assert_eq!(agent(&[]).unwrap(), HookAgent::Claude);
        assert_eq!(agent(&["--for", "claude"]).unwrap(), HookAgent::Claude);
        assert_eq!(agent(&["--for", "codex"]).unwrap(), HookAgent::Codex);
        assert_eq!(agent(&["--for", "gemini"]).unwrap(), HookAgent::Gemini);
        assert!(agent(&["--for", "cursor"]).is_err());
        assert!(parse(&argv(&["--for"])).is_err());
        // Bad flags on a hook still exit 0, silently.
        assert_eq!(run_and_exit(&argv(&["hook", "--for", "nope"])), 0);
        assert_eq!(run_and_exit(&argv(&["session-end", "--for", "nope"])), 0);
    }

    /// Claude's and Codex's stdout is the text itself (Codex runs hooks under a login shell
    /// whose profile output would break a JSON envelope); Gemini's stdout is exactly one JSON
    /// object holding the text, and nothing else.
    #[test]
    fn hook_stdout_is_wrapped_per_agent() {
        let text = "## Project memory (x)\n\n- a \"quoted\" fact\n";
        assert_eq!(hook_stdout(HookAgent::Claude, text), text);
        assert_eq!(hook_stdout(HookAgent::Codex, text), text);

        let out = hook_stdout(HookAgent::Gemini, text);
        let mut stream = serde_json::Deserializer::from_str(&out).into_iter::<serde_json::Value>();
        let only = stream.next().unwrap().unwrap();
        assert!(
            stream.next().is_none(),
            "one JSON value and nothing else: {out}"
        );
        assert_eq!(out.trim(), out, "no stray whitespace or text around it");
        assert_eq!(
            only,
            serde_json::json!({ "hookSpecificOutput": { "additionalContext": text } })
        );
    }

    /// Every Gemini silent path prints nothing at all (an empty stdout is a success with
    /// no output for Gemini CLI): the only printing path is a `Print` outcome, wrapped.
    #[test]
    fn gemini_silent_paths_print_nothing() {
        let (conn, _) = hook_conn();
        let mut req = hook_req("resume");
        req.agent = HookAgent::Gemini;
        let missing = std::env::temp_dir().join("calli-no-index-here.db");
        for source in ["resume", "fork"] {
            req.source = source;
            assert!(matches!(
                hook_outcome(&req, &missing).unwrap(),
                HookOutcome::Silent(_)
            ));
        }
        req.source = "startup";
        assert!(matches!(
            hook_outcome(&req, &missing).unwrap(),
            HookOutcome::Silent(_)
        ));
        req.project = "/Users/me/nothing-here";
        assert!(matches!(
            hook_output(&conn, &req).unwrap(),
            HookOutcome::Silent(_)
        ));
        // A bad payload or flag is an error the silent runner swallows before any print.
        assert_eq!(
            run_and_exit(&argv(&["hook", "--for", "gemini", "--nope"])),
            0
        );
    }

    /// Codex and Gemini CLI get harvested Claude Code memory (they never load Claude's own
    /// auto memory), marked with its origin; Claude Code does not.
    #[test]
    fn other_agents_get_harvested_memory_with_its_origin() {
        let (conn, tid) = hook_conn();
        dts::fact(
            &conn,
            tid,
            "decision",
            "ship on fridays only",
            "claude-memory",
        );
        let claude = printed(hook_output(&conn, &hook_req("startup")).unwrap());
        assert!(!claude.contains("fridays"), "{claude}");
        for agent in [HookAgent::Codex, HookAgent::Gemini] {
            let mut req = hook_req("startup");
            req.agent = agent;
            let out = printed(hook_output(&conn, &req).unwrap());
            assert!(
                out.contains("- (from Claude Code memory) ship on fridays only"),
                "{out}"
            );
        }
    }

    /// Codex reads AGENTS.override.md over AGENTS.md per dir, from cwd up to the git root;
    /// Gemini reads GEMINI.md (or its configured `context.fileName`). A managed block in a
    /// file the agent loads silences its hook; one above the git root does not.
    #[test]
    fn other_agents_check_their_own_context_files() {
        let root = temp_dir("ctx-files");
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("sub")).unwrap();
        let block = format!("{}\nmemory\n", export::MANAGED_START);
        std::fs::write(root.join("AGENTS.md"), &block).unwrap(); // above the repo
        let dirs = project_dirs(&repo.join("sub"));
        assert_eq!(dirs.len(), 2, "{dirs:?}");
        assert_eq!(
            context_block_source(
                &dirs,
                None,
                &CODEX_CONTEXT_FILES,
                ContextRule::FirstExisting
            ),
            None
        );

        std::fs::write(repo.join("AGENTS.md"), &block).unwrap();
        assert_eq!(
            context_block_source(
                &dirs,
                None,
                &CODEX_CONTEXT_FILES,
                ContextRule::FirstExisting
            ),
            Some(dirs[1].join("AGENTS.md"))
        );
        // An override beside it wins, and it holds no block.
        std::fs::write(repo.join("AGENTS.override.md"), "local rules\n").unwrap();
        assert_eq!(
            context_block_source(
                &dirs,
                None,
                &CODEX_CONTEXT_FILES,
                ContextRule::FirstExisting
            ),
            None
        );

        // Gemini: GEMINI.md by default, else what settings.json names.
        let gem = root.join("gemini-home");
        std::fs::create_dir_all(&gem).unwrap();
        assert_eq!(gemini_context_files(&gem), ["GEMINI.md"]);
        assert_eq!(
            context_block_source(
                &dirs,
                Some(&gem),
                &["GEMINI.md"],
                ContextRule::EveryFileWithImports
            ),
            None
        );
        std::fs::write(
            gem.join("settings.json"),
            r#"{"context":{"fileName":["AGENTS.md","GEMINI.md"]}}"#,
        )
        .unwrap();
        assert_eq!(gemini_context_files(&gem), ["AGENTS.md", "GEMINI.md"]);
        std::fs::write(gem.join("GEMINI.md"), &block).unwrap();
        assert_eq!(
            context_block_source(
                &dirs,
                Some(&gem),
                &["GEMINI.md"],
                ContextRule::EveryFileWithImports
            ),
            Some(gem.join("GEMINI.md"))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Gemini loads every configured name in a dir (not just the first that exists) and
    /// expands its `@` imports; Codex keeps first-existing-wins, with the fallback names
    /// from config.toml after AGENTS.md.
    #[test]
    fn context_files_follow_each_agents_loading_rules() {
        let root = temp_dir("ctx-rules");
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let dirs = project_dirs(&repo);
        let block = format!("{}\nmemory\n", export::MANAGED_START);
        let gemini = ["AGENTS.md", "GEMINI.md"];

        // A block-less AGENTS.md beside the GEMINI.md that holds the block.
        std::fs::write(repo.join("AGENTS.md"), "rules\n").unwrap();
        std::fs::write(repo.join("GEMINI.md"), &block).unwrap();
        assert_eq!(
            context_block_source(&dirs, None, &gemini, ContextRule::EveryFileWithImports),
            Some(dirs[0].join("GEMINI.md"))
        );
        assert_eq!(
            context_block_source(&dirs, None, &gemini, ContextRule::FirstExisting),
            None,
            "Codex reads only the first existing name"
        );

        // GEMINI.md that only imports AGENTS.md, where `cal agents` wrote the block.
        std::fs::write(repo.join("AGENTS.md"), &block).unwrap();
        std::fs::write(repo.join("GEMINI.md"), "@AGENTS.md\n").unwrap();
        assert_eq!(
            context_block_source(
                &dirs,
                None,
                &["GEMINI.md"],
                ContextRule::EveryFileWithImports
            ),
            Some(dirs[0].join("AGENTS.md"))
        );
        assert_eq!(
            context_block_source(&dirs, None, &["GEMINI.md"], ContextRule::FirstExisting),
            None
        );

        // Codex fallback names come after AGENTS.md, in config.toml order.
        let codex = root.join("codex-home");
        std::fs::create_dir_all(&codex).unwrap();
        assert_eq!(codex_context_files(Some(&codex)), CODEX_CONTEXT_FILES);
        std::fs::write(
            codex.join("config.toml"),
            "project_doc_fallback_filenames = [\"CLAUDE.md\", \"AGENTS.md\", \"../x.md\"]\n",
        )
        .unwrap();
        assert_eq!(
            codex_context_files(Some(&codex)),
            ["AGENTS.override.md", "AGENTS.md", "CLAUDE.md"]
        );
        std::fs::remove_file(repo.join("AGENTS.md")).unwrap();
        std::fs::write(repo.join("CLAUDE.md"), &block).unwrap();
        let names = codex_context_files(Some(&codex));
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        assert_eq!(
            context_block_source(&dirs, None, &names, ContextRule::FirstExisting),
            Some(dirs[0].join("CLAUDE.md"))
        );

        // Gemini's settings.json may carry comments.
        let gem = root.join("gemini-home");
        std::fs::create_dir_all(&gem).unwrap();
        std::fs::write(
            gem.join("settings.json"),
            "{\n  // where memory lives\n  \"context\": {/* both */ \"fileName\": [\"AGENTS.md\", \"GEMINI.md\"]}\n}\n",
        )
        .unwrap();
        assert_eq!(gemini_context_files(&gem), ["AGENTS.md", "GEMINI.md"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The ledger counts sessions: a new one or a cleared context, never a compact refresher.
    #[test]
    fn only_new_contexts_count_in_the_ledger() {
        assert!(counts_as_session("startup"));
        assert!(counts_as_session("clear"));
        for source in ["compact", "resume", "fork", ""] {
            assert!(!counts_as_session(source), "{source}");
        }
    }

    /// Codex's PreCompact payload names the session by its rollout file and its id.
    #[test]
    fn codex_session_ids_come_from_the_rollout_name_then_the_payload() {
        let uuid = "019dc6b0-998d-7a31-a39d-1229e6b38072";
        let v = serde_json::json!({
            "session_id": uuid,
            "transcript_path": format!("/h/.codex/sessions/2026/04/25/rollout-2026-04-25T15-09-15-{uuid}.jsonl"),
        });
        assert_eq!(codex_session_ids(&v), [uuid]);
        let v = serde_json::json!({ "session_id": "other", "transcript_path": "/x/notes.jsonl" });
        assert_eq!(codex_session_ids(&v), ["other"]);
        assert!(codex_session_ids(&serde_json::json!({})).is_empty());
        assert!(is_uuid(uuid) && !is_uuid("019dc6b0-998d-7a31-a39d-1229e6b3807z"));
    }

    /// `cal help` (USAGE) must document every real command, so a new subcommand can't ship
    /// undocumented. Internal hook targets are intentionally not user-facing.
    #[test]
    fn usage_documents_every_command() {
        const INTERNAL: &[&str] = &["snapshot-hook", "recall-now", "session-end"];
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
