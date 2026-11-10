//! `real_index_bench`: the reads the app, `cal`, the hooks and the MCP server lean on, timed
//! on a copy of a real index, with the query plan of every statement each one runs. Numbers
//! from a synthetic index say little: the plans and the I/O that matter (FTS segments, the
//! vec0 chunks, the covering indexes) only show at real sizes. How to run it is in
//! CONTRIBUTING.md ("Measuring on a real index").
//!
//! Read-only: the index is opened with [`db::open_readonly`], and only from `CALLIMACHUS_DB`,
//! never a file in the app's data folder (the live index, or the dev build's): measure an
//! APFS clone (`cp -c`) or a plain copy. "cold" is a case's first run in this process, so
//! SQLite's page cache is empty; the OS cache is cold only on a copy nothing has read yet
//! (a fresh clone has its own, empty, cache on macOS). "warm" is the best of [`WARM_RUNS`]
//! more runs. Each line gives wall time and this process's CPU time.

use crate::{db, embed, export, issues, jobclock, search, today};
use rusqlite::trace::{TraceEvent, TraceEventCodes};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// Warm runs per case after the cold one; the best is reported.
const WARM_RUNS: usize = 3;

/// The statements the running case executed (their SQL as written, `?` placeholders and
/// all), with the time each took. `trace_v2` takes a plain fn, hence a global.
static SEEN: Mutex<Vec<(String, Duration)>> = Mutex::new(Vec::new());

fn record(event: TraceEvent<'_>) {
    if let TraceEvent::Profile(stmt, took) = event {
        if let Ok(mut seen) = SEEN.lock() {
            seen.push((stmt.sql().into_owned(), took));
        }
    }
}

/// A case's work, returning how many rows or bytes it produced.
type Run = Box<dyn Fn(&Connection) -> anyhow::Result<usize>>;

/// One timed read: what it is, and the work (its output size is printed, so a case that
/// silently finds nothing stands out).
struct Case {
    name: String,
    run: Run,
}

fn case(
    name: impl Into<String>,
    run: impl Fn(&Connection) -> anyhow::Result<usize> + 'static,
) -> Case {
    Case {
        name: name.into(),
        run: Box::new(run),
    }
}

/// Wall and process CPU time of one call.
struct Timing {
    wall: Duration,
    cpu: Option<Duration>,
    out: usize,
}

fn time(conn: &Connection, run: &dyn Fn(&Connection) -> anyhow::Result<usize>) -> Timing {
    let clock = jobclock::JobClock::start();
    let out = run(conn).unwrap_or_else(|e| panic!("{e:#}"));
    Timing {
        wall: clock.wall(),
        cpu: clock.process_cpu(),
        out,
    }
}

fn ms(d: Duration) -> String {
    format!("{:.1} ms", d.as_secs_f64() * 1000.0)
}

fn cpu(t: &Timing) -> String {
    t.cpu.map(ms).unwrap_or_else(|| "?".into())
}

/// The index to measure: `CALLIMACHUS_DB`, refused when it names a file in the app's data
/// folder. None when it is unset (a plain `--ignored` run skips the bench).
fn bench_index() -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os("CALLIMACHUS_DB")?);
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let file = canonical(&path);
    let app_dir = canonical(&db::app_data_dir());
    assert!(
        file.parent() != Some(app_dir.as_path()),
        "{} is in the app's data folder; copy it elsewhere first (cp -c on APFS)",
        path.display()
    );
    Some(path)
}

/// Query vectors for the KNN cases: stored chunks spread over the table, so no model is
/// needed and each one has close neighbors, as a real query would.
fn stored_vectors(conn: &Connection, n: usize) -> anyhow::Result<Vec<Vec<f32>>> {
    let total: i64 = conn.query_row("SELECT COUNT(*) FROM vec_chunk_owner", [], |r| r.get(0))?;
    let mut out = Vec::new();
    for i in 0..n as i64 {
        let offset = total * (2 * i + 1) / (2 * n as i64);
        let blob: Option<Vec<u8>> = conn
            .query_row(
                "SELECT v.embedding FROM vec_chunks v
                 WHERE v.rowid = (SELECT chunk_rowid FROM vec_chunk_owner
                                  ORDER BY chunk_rowid LIMIT 1 OFFSET ?1)",
                [offset],
                |r| r.get(0),
            )
            .ok();
        if let Some(blob) = blob {
            out.push(
                blob.chunks_exact(4)
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    .collect(),
            );
        }
    }
    Ok(out)
}

/// The fixed set: keyword search, the semantic KNN with and without scope, what the
/// SessionStart hook reads for the biggest projects, the digest, issue mining, Today and
/// the embed progress counts.
fn cases(conn: &Connection) -> anyhow::Result<Vec<Case>> {
    let mut cases = Vec::new();
    for query in [
        "error",
        "migration",
        "test",
        "index",
        "\"database is locked\"",
    ] {
        cases.push(case(format!("fts {query}"), move |c| {
            Ok(search::search(c, query, &search::SearchFilters::default())?.len())
        }));
    }

    // Projects by main-thread sessions, labelled by rank, not by path: the output is meant
    // to be pasted into an issue or a PR.
    let projects: Vec<(String, i64)> = conn
        .prepare(
            "SELECT project_key, COUNT(*) FROM threads
             WHERE project_key IS NOT NULL AND is_subagent = 0
             GROUP BY project_key ORDER BY 2 DESC LIMIT 5",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;

    let vectors = std::sync::Arc::new(stored_vectors(conn, 3)?);
    if vectors.is_empty() {
        println!("(no vectors in this index: the KNN cases are skipped)");
    } else {
        let knn = |name: String, project: Option<String>, sources: Vec<String>| {
            let vectors = std::sync::Arc::clone(&vectors);
            case(name, move |c| {
                let mut hits = 0;
                for qv in vectors.iter() {
                    hits +=
                        embed::semantic_search_vec(c, qv, false, &sources, project.as_deref(), 20)?
                            .len();
                }
                Ok(hits)
            })
        };
        cases.push(knn("knn default scope x3".into(), None, Vec::new()));
        if let Some((project, _)) = projects.first() {
            cases.push(knn(
                "knn project #1 x3".into(),
                Some(project.clone()),
                Vec::new(),
            ));
        }
        // The source with the fewest vectors (at least 100): the narrowest filter, where
        // the post-filtered KNN path loses most.
        let source: Option<String> = conn
            .query_row(
                "SELECT s.kind FROM vec_chunk_owner o JOIN messages m ON m.id = o.message_id
                 JOIN threads t ON t.id = m.thread_id JOIN sources s ON s.id = t.source_id
                 GROUP BY s.kind HAVING COUNT(*) >= 100 ORDER BY COUNT(*) ASC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .ok();
        if let Some(source) = source {
            cases.push(knn(format!("knn source {source} x3"), None, vec![source]));
        }
    }

    for (rank, (project, threads)) in projects.iter().enumerate() {
        let project = project.clone();
        cases.push(case(
            format!("hook preview project #{} ({threads} sessions)", rank + 1),
            move |c| {
                Ok(match crate::cli_core::hook_preview(c, &project)? {
                    crate::cli_core::HookOutcome::Print(text) => text.len(),
                    _ => 0,
                })
            },
        ));
    }
    if let Some((project, _)) = projects.first() {
        let project = project.clone();
        cases.push(case("session digest project #1", move |c| {
            let opts = export::DigestOpts {
                max_sessions: export::DIGEST_MAX_SESSIONS,
                max_chars: 2500,
            };
            Ok(export::session_digest(c, &project, &opts)?.map_or(0, |d| d.len()))
        }));
    }
    let now = chrono::Utc::now().timestamp();
    // Out is 1 when it mined any cluster (the count is private to issues).
    cases.push(case("issue mining (refresh's read half)", move |c| {
        Ok(usize::from(!issues::mine_for_refresh(c, now)?.is_empty()))
    }));
    cases.push(case("today (last 24 h)", move |c| {
        Ok(today::build(c, now - 86_400, now)?.groups.len())
    }));
    cases.push(case("embed progress", |c| {
        let (done, total) = embed::embed_progress(c)?;
        Ok((done + total) as usize)
    }));
    Ok(cases)
}

/// Print `sql`'s query plan, indented, or why it has none. Its parameters are bound to
/// NULL: the plan depends on which constraints exist, not on their values.
fn print_plan(conn: &Connection, sql: &str) {
    let plan = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .and_then(|mut stmt| {
            let nulls = vec![rusqlite::types::Null; stmt.parameter_count()];
            stmt.query_map(rusqlite::params_from_iter(nulls), |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
        });
    match plan {
        Ok(rows) => {
            // EXPLAIN QUERY PLAN rows are (id, parent, _, detail): indent by depth.
            let mut depth = std::collections::HashMap::new();
            for (id, parent, detail) in rows {
                let d = depth.get(&parent).map_or(0, |d| d + 1);
                depth.insert(id, d);
                println!("        {}{detail}", "  ".repeat(d));
            }
        }
        Err(e) => println!("        (no plan: {e})"),
    }
}

/// One statement's SQL on a line, whitespace collapsed and cut, for the listing.
fn one_line(sql: &str) -> String {
    let flat = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    let cut: String = flat.chars().take(160).collect();
    if cut.len() < flat.len() {
        format!("{cut}...")
    } else {
        cut
    }
}

#[test]
#[ignore = "reads a copy of a real index from CALLIMACHUS_DB (CONTRIBUTING.md)"]
fn real_index_bench() {
    let Some(path) = bench_index() else {
        println!("skipped: set CALLIMACHUS_DB to a copy of an index (CONTRIBUTING.md)");
        return;
    };
    let conn = db::open_readonly(&path).unwrap();
    let cases = cases(&conn).unwrap();
    println!(
        "{} ({:.2} GB)\n",
        path.display(),
        std::fs::metadata(&path).map_or(0.0, |m| m.len() as f64 / 1e9)
    );
    println!(
        "{:<44} {:>9} | {:>10} {:>10} | {:>10} {:>10}",
        "case", "out", "cold wall", "cold cpu", "warm wall", "warm cpu"
    );
    let mut plans = Vec::new();
    for case in &cases {
        let cold = time(&conn, &*case.run);
        let warm = (0..WARM_RUNS)
            .map(|_| time(&conn, &*case.run))
            .min_by_key(|t| t.wall)
            .unwrap();
        // One more run, traced, for the statement list: a callback per statement (FTS5
        // runs one per matching row for bm25's sizes) would weigh on the timed runs.
        SEEN.lock().unwrap().clear();
        conn.trace_v2(TraceEventCodes::SQLITE_TRACE_PROFILE, Some(record));
        time(&conn, &*case.run);
        conn.trace_v2(TraceEventCodes::empty(), None);
        let statements = std::mem::take(&mut *SEEN.lock().unwrap());
        println!(
            "{:<44} {:>9} | {:>10} {:>10} | {:>10} {:>10}",
            case.name,
            cold.out,
            ms(cold.wall),
            cpu(&cold),
            ms(warm.wall),
            cpu(&warm)
        );
        plans.push((case.name.clone(), statements));
    }
    // The plans after the table. Statements run inside another (FTS5's and vec0's own
    // lookups) are listed too, so their times overlap the outer statement's.
    for (name, statements) in plans {
        println!("\n{name} (a warm, traced run):");
        // Each distinct statement once, in first-run order, with its total time.
        let mut order: Vec<String> = Vec::new();
        let mut total: std::collections::HashMap<String, (Duration, usize)> =
            std::collections::HashMap::new();
        for (sql, took) in statements {
            let entry = total.entry(sql.clone()).or_insert_with(|| {
                order.push(sql.clone());
                (Duration::ZERO, 0)
            });
            entry.0 += took;
            entry.1 += 1;
        }
        for sql in order {
            let (took, runs) = total[&sql];
            println!("    [{} in {runs} run(s)] {}", ms(took), one_line(&sql));
            print_plan(&conn, &sql);
        }
    }
}
