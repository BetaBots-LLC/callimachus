//! Cursor indexer. Three stores, one thread per agent id:
//!
//! - The editor's GLOBAL DB
//!   `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb` (table
//!   `cursorDiskKV`): `composerData:<id>` rows are threads, and
//!   `bubbleId:<composerId>:<bubbleId>` rows are messages (`type` 1=user, 2=assistant).
//! - Agent transcripts, `~/.cursor/projects/<slug>/agent-transcripts/`: one JSONL per
//!   agent, nested `<id>/<id>.jsonl` with `<id>/subagents/<sub>.jsonl`, where `<slug>` is the
//!   workspace path with every non-alphanumeric char as `-`. On a real machine all 195 were
//!   nested, the one CLI session's included; a flat `<id>.jsonl` is read too, unobserved.
//!   The editor writes one for each composer (194 of 195 ids were also `composerData` rows),
//!   so for those the transcript only supplies the project path state.vscdb lacks; it is
//!   indexed itself for agents state.vscdb doesn't have.
//! - The CLI's chat stores, `~/.cursor/chats/<md5 of cwd>/<id>/store.db`: a
//!   content-addressed `blobs` table whose root blob (named in `meta`) lists the message
//!   blobs in order. Richer than its transcript (tool results, the workspace path), so it
//!   wins over the transcript of the same id.
//!
//! Precedence per id is composer (with text) > CLI store (with messages) > transcript,
//! decided from what each store held when it was last read and stored with its file state,
//! so the same files always pick the same winner and two stores never take turns rewriting
//! one thread.
//!
//! The DB can be multiple GB and is open while Cursor runs, so we open it
//! read-only and pull only the fields we need with SQL `json_extract`, never the
//! full blobs. We upsert per-composer to keep memory bounded.
//!
//! Change detection is two-level. The whole store is one file (plus its `-wal`, where
//! Cursor's new writes land until a checkpoint), so a pass starts only when either file
//! changed. Within a pass, each composer is fingerprinted from cheap metadata
//! (`lastUpdatedAt`, bubble count, and while it is recent the bubbles' byte size) and
//! only composers whose fingerprint moved get their bubbles extracted and re-upserted.
//! Without this, every checkpoint re-extracted every bubble (gigabytes of JSON) and
//! rewrote every thread.

use super::common::{
    dir_for_project_slug, is_drive_path, normalize_slug, push_message, slug_of, truncate_title,
};
use super::{
    canonical_project, file_change_state, set_file_state, source_id, upsert_thread, IndexReport,
    ParsedMessage, ParsedThread,
};
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub const KIND: &str = "cursor";

/// Path to Cursor's global state DB. Uses the platform per-user config dir
/// (macOS `~/Library/Application Support`, Windows `%APPDATA%`, Linux `~/.config`),
/// which is where VSCode-family editors keep `User/globalStorage`.
pub fn global_db_path() -> Option<PathBuf> {
    dirs::config_dir().map(|c| c.join("Cursor/User/globalStorage/state.vscdb"))
}

struct Composer {
    id: String,
    name: Option<String>,
    created_at_ms: Option<i64>,
    /// `composerData.lastUpdatedAt` (ms); absent on older Cursor builds.
    last_updated_ms: Option<i64>,
}

/// A composer whose `lastUpdatedAt` is older than this is treated as settled: its
/// fingerprint is checked from the timestamp + bubble count alone, skipping the bubble
/// size sum. Recent composers (an assistant reply may still be streaming into an existing
/// bubble without changing the count) always get the full check.
const SETTLED_AFTER_MS: i64 = 15 * 60 * 1000;

/// `<db>-wal`: where a WAL-mode SQLite DB keeps writes until the next checkpoint.
fn wal_path(db: &Path) -> PathBuf {
    let mut s = db.as_os_str().to_owned();
    s.push("-wal");
    PathBuf::from(s)
}

/// `index_state` key for one composer's fingerprint.
fn composer_key(db: &Path, composer_id: &str) -> String {
    format!("{}#composer:{composer_id}", db.to_string_lossy())
}

/// Suffixes on a composer's fingerprint saying whether its bubbles held text, so the agent
/// stores below can tell "state.vscdb has this conversation" from "it has an empty shell of
/// it". Every fingerprint written since the agent stores were added carries one. One with
/// neither was written by an older build, which marked nothing: [`composer_covers`] rechecks
/// that composer once and rewrites it with its mark.
const EMPTY_MARK: &str = ":empty";
const TEXT_MARK: &str = ":text";

/// A fingerprint without its coverage mark (if it has one).
fn fingerprint_core(fp: &str) -> &str {
    fp.strip_suffix(EMPTY_MARK)
        .or_else(|| fp.strip_suffix(TEXT_MARK))
        .unwrap_or(fp)
}

/// Stored fingerprint `"<lastUpdatedAt>:<bubbles>:<bytes>"` for a composer, if any, with
/// [`TEXT_MARK`] or [`EMPTY_MARK`] appended.
fn stored_fingerprint(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT hash FROM index_state WHERE path = ?1", [key], |r| {
            r.get::<_, Option<String>>(0)
        })
        .optional()?
        .flatten())
}

fn store_fingerprint(conn: &Connection, key: &str, fingerprint: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO index_state (path, source_kind, mtime, size, hash, updated_at)
         VALUES (?1, ?2, 0, 0, ?3, ?4)
         ON CONFLICT (path) DO UPDATE SET hash = excluded.hash, updated_at = excluded.updated_at",
        params![key, KIND, fingerprint, chrono::Utc::now().timestamp()],
    )?;
    Ok(())
}

/// Index every Cursor store: the editor's composers, then the CLI's chat stores and the
/// agent transcripts state.vscdb doesn't already cover.
pub fn scan(conn: &mut Connection, tick: &mut dyn FnMut()) -> Result<IndexReport> {
    let stores = cursor_home()
        .map(|home| AgentStores::discover(&home))
        .unwrap_or_default();
    scan_all(conn, global_db_path().as_deref(), &stores, tick)
}

fn scan_all(
    conn: &mut Connection,
    db: Option<&Path>,
    stores: &AgentStores,
    tick: &mut dyn FnMut(),
) -> Result<IndexReport> {
    // A composer-pass failure (state.vscdb locked mid-checkpoint, a malformed row) must not
    // also drop the CLI's sessions, which live in files of their own.
    let mut report = match db {
        Some(db) => match scan_path(conn, db, &stores.placement, tick) {
            Ok(r) => r,
            Err(e) => {
                app_log!("[index] {}: {e:#}", db.display());
                IndexReport {
                    errors: 1,
                    ..Default::default()
                }
            }
        },
        None => IndexReport::default(),
    };
    if stores.chats.is_empty() && stores.transcripts.is_empty() {
        return Ok(report);
    }
    let sid = source_id(conn, KIND)?;
    if let Err(e) = enrich_composers(conn, sid, &stores.placement) {
        app_log!("[index] cursor project paths: {e:#}");
        report.errors += 1;
    }
    let mut state = StateDb::new(db);
    for chat in &stores.chats {
        tick();
        let result = index_chat(conn, sid, &mut state, chat, &stores.placement);
        report.record_at(&chat.path, result);
    }
    // A store covers its transcript only when its last read found messages: one in a layout
    // this parser doesn't know (a Cursor CLI format change) leaves the session to its
    // transcript instead of hiding it. That outcome is stored per store state, so the same
    // files always pick the same winner.
    let mut chat_ids: HashSet<&str> = HashSet::new();
    for chat in &stores.chats {
        if chat_outcome(conn, &chat.path)?.as_deref() == Some(CHAT_HAS_MESSAGES) {
            chat_ids.insert(chat.id.as_str());
        }
    }
    for t in &stores.transcripts {
        tick();
        let result = index_transcript(conn, sid, &mut state, t, &chat_ids);
        report.record_at(&t.path, result);
    }
    Ok(report)
}

/// Index Cursor threads from a specific state.vscdb. Skips entirely if the file is
/// unchanged since the last pass (the whole store is one file). `placement` is where each
/// agent's transcript sits (its project, and whether it is a subagent), which the
/// composer rows themselves don't record.
fn scan_path(
    conn: &mut Connection,
    db: &Path,
    placement: &HashMap<String, Placement>,
    tick: &mut dyn FnMut(),
) -> Result<IndexReport> {
    let mut report = IndexReport::default();
    if !db.exists() {
        return Ok(report);
    }
    let sid = source_id(conn, KIND)?;

    // Change key covers the main file AND its WAL: new chats land in the -wal first, and
    // the main file only changes at checkpoint time, so watching it alone lagged.
    let wal = wal_path(db);
    let (main_unchanged, mtime, size) = file_change_state(conn, db)?;
    let wal_state = if wal.exists() {
        Some(file_change_state(conn, &wal)?)
    } else {
        None
    };
    if main_unchanged
        && wal_state
            .as_ref()
            .is_none_or(|(unchanged, _, _)| *unchanged)
    {
        return Ok(report); // nothing changed
    }

    let ro = super::open_external_readonly(db)?;
    let composers = list_composers(&ro)?;
    let now_ms = chrono::Utc::now().timestamp_millis();

    // Index-only count of a composer's bubbles (key range on the key index), plus, when
    // asked, their total byte size: octet_length() reads only each record's header.
    let mut count_stmt =
        ro.prepare("SELECT COUNT(*) FROM cursorDiskKV WHERE key >= ?1 AND key < ?2")?;
    let mut bytes_stmt = ro.prepare(
        "SELECT COALESCE(SUM(octet_length(value)), 0) FROM cursorDiskKV
         WHERE key >= ?1 AND key < ?2",
    )?;

    // CAST each extracted field so rusqlite always sees a predictable column type
    // (Cursor stores some numbers as REAL, which would otherwise fail i64 decoding).
    // Use a key RANGE (>=/<), not LIKE: SQLite's LIKE is case-insensitive by default
    // and would NOT use the PRIMARY KEY index — a full 4.4GB scan per composer.
    let mut bubble_stmt = ro.prepare(
        "SELECT CAST(json_extract(value, '$.type') AS INTEGER),
                CAST(json_extract(value, '$.text') AS TEXT),
                CAST(json_extract(value, '$.timingInfo.clientStartTime') AS INTEGER)
         FROM cursorDiskKV
         WHERE key >= ?1 AND key < ?2
         ORDER BY 3",
    )?;

    for c in composers {
        tick();
        let lo = format!("bubbleId:{}:", c.id);
        let hi = format!("bubbleId:{}:\u{10FFFF}", c.id);

        // Deleted by the user: never re-import.
        if crate::cleanup::is_tombstoned(conn, sid, &c.id)? {
            report.threads_skipped += 1;
            continue;
        }

        // Skip composers whose fingerprint hasn't moved since they were last indexed.
        let key = composer_key(db, &c.id);
        let stored = stored_fingerprint(conn, &key)?;
        let updated = c.last_updated_ms.unwrap_or(-1);
        let bubbles: i64 = count_stmt.query_row(params![lo, hi], |r| r.get(0))?;
        let settled = c
            .last_updated_ms
            .is_some_and(|u| now_ms - u > SETTLED_AFTER_MS);
        let cheap_prefix = format!("{updated}:{bubbles}:");
        if settled
            && stored
                .as_deref()
                .is_some_and(|f| f.starts_with(&cheap_prefix))
        {
            report.threads_skipped += 1;
            continue;
        }
        let bytes: i64 = bytes_stmt.query_row(params![lo, hi], |r| r.get(0))?;
        let fingerprint = format!("{cheap_prefix}{bytes}");
        if stored.as_deref().map(fingerprint_core) == Some(fingerprint.as_str()) {
            report.threads_skipped += 1;
            continue;
        }

        let mut messages: Vec<ParsedMessage> = Vec::new();
        let mut first_user: Option<String> = None;
        let mut max_ts: Option<i64> = None;

        let rows = bubble_stmt.query_map(params![lo, hi], |r| {
            Ok((
                r.get::<_, Option<i64>>(0)?,    // type
                r.get::<_, Option<String>>(1)?, // text
                r.get::<_, Option<i64>>(2)?,    // clientStartTime (ms)
            ))
        })?;

        for row in rows {
            let (kind, text, ts_ms) = row?;
            let Some(text) = text.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) else {
                continue;
            };
            let role = if kind == Some(2) { "assistant" } else { "user" };
            let ts = ts_ms.map(|ms| ms / 1000);
            if role == "user" && first_user.is_none() {
                first_user = Some(text.clone());
            }
            if let Some(ts) = ts {
                max_ts = Some(max_ts.map_or(ts, |m| m.max(ts)));
            }
            messages.push(ParsedMessage {
                role: role.to_string(),
                text,
                tool_name: None,
                ts,
            });
        }

        if messages.is_empty() {
            // Nothing to index yet, but remember the fingerprint so the next pass doesn't
            // re-extract it again, marked so a transcript of this id is indexed instead.
            store_fingerprint(conn, &key, &format!("{fingerprint}{EMPTY_MARK}"))?;
            report.threads_skipped += 1;
            continue;
        }

        let created_at = c.created_at_ms.map(|ms| ms / 1000);
        let title = c
            .name
            .filter(|n| !n.trim().is_empty())
            .or_else(|| first_user.as_deref().map(truncate_title));
        // state.vscdb records no workspace per composer; the agent transcript's directory
        // does (see `enrich_composers` for composers this pass skips as unchanged).
        let placed = placement.get(&c.id);
        let mut thread = ParsedThread {
            project_path: placed.and_then(|p| p.project.clone()),
            is_subagent: placed.is_some_and(|p| p.is_subagent),
            external_id: c.id,
            title,
            git_branch: None,
            created_at,
            updated_at: max_ts.or(created_at),
            usage: Vec::new(),
            messages,
        };
        keep_known_project(conn, sid, &mut thread)?;
        let n = upsert_thread(conn, sid, &thread)?;
        // Only after a successful upsert, so a failed write is retried next pass.
        store_fingerprint(conn, &key, &format!("{fingerprint}{TEXT_MARK}"))?;
        report.threads_indexed += 1;
        report.messages_indexed += n;
    }

    // Record the DB file as indexed ONLY after every composer upserted without error: a
    // mid-pass failure returns above via `?`, leaving the file "changed" so the watcher's
    // retry re-indexes it instead of skipping the dropped threads.
    set_file_state(conn, &db.to_string_lossy(), KIND, mtime, size)?;
    if let Some((_, wal_mtime, wal_size)) = wal_state {
        set_file_state(conn, &wal.to_string_lossy(), KIND, wal_mtime, wal_size)?;
    }
    Ok(report)
}

fn list_composers(ro: &Connection) -> Result<Vec<Composer>> {
    // substr(key, 14) strips the "composerData:" prefix (13 chars).
    // Range scan (not LIKE) so the PRIMARY KEY index is used. ';' (0x3b) is the
    // byte just after ':' (0x3a), bounding the 'composerData:' prefix.
    let mut stmt = ro.prepare(
        "SELECT substr(key, 14),
                CAST(json_extract(value, '$.name') AS TEXT),
                CAST(json_extract(value, '$.createdAt') AS INTEGER),
                CAST(json_extract(value, '$.lastUpdatedAt') AS INTEGER)
         FROM cursorDiskKV
         WHERE key >= 'composerData:' AND key < 'composerData;'",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Composer {
            id: r.get(0)?,
            name: r.get::<_, Option<String>>(1)?,
            created_at_ms: r.get::<_, Option<i64>>(2)?,
            last_updated_ms: r.get::<_, Option<i64>>(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// ---------------------------------------------------------------------------------------
// Agent transcripts and the CLI's chat stores (`~/.cursor`)
// ---------------------------------------------------------------------------------------

/// Cursor's per-user dir, `~/.cursor` on every platform: agent transcripts under
/// `projects/`, the CLI's chat stores under `chats/`.
pub fn cursor_home() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".cursor"))
}

/// Where an agent's transcript sits: the project its directory names, and whether it is a
/// subagent (`<parent>/subagents/<id>.jsonl`).
#[derive(Debug, Clone, Default, PartialEq)]
struct Placement {
    project: Option<String>,
    is_subagent: bool,
}

/// One agent transcript file.
#[derive(Debug)]
struct Transcript {
    id: String,
    path: PathBuf,
    /// The `projects/` directory name, for recovering the project from the transcript's
    /// own tool paths when the directory it names no longer exists.
    slug: String,
    placement: Placement,
}

/// One CLI chat store.
#[derive(Debug)]
struct Chat {
    id: String,
    path: PathBuf,
}

#[derive(Debug, Default)]
struct AgentStores {
    transcripts: Vec<Transcript>,
    chats: Vec<Chat>,
    placement: HashMap<String, Placement>,
}

impl AgentStores {
    /// Walk `<home>/projects/*/agent-transcripts` and `<home>/chats/*/*/store.db`. Only
    /// directory listings: no file is opened here, so an unchanged tree costs a few hundred
    /// stats per pass.
    fn discover(home: &Path) -> Self {
        let mut out = AgentStores::default();
        for project_dir in read_dirs(&home.join("projects")) {
            let root = project_dir.join("agent-transcripts");
            if !root.is_dir() {
                continue;
            }
            let slug = file_name(&project_dir);
            // Resolved once per project directory, against the directories on disk.
            let project = project_for_slug(&slug);
            let add = |out: &mut AgentStores, path: PathBuf, is_subagent: bool| {
                let Some(id) = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(str::to_string)
                else {
                    return;
                };
                let placement = Placement {
                    project: project.clone(),
                    is_subagent,
                };
                out.placement.insert(id.clone(), placement.clone());
                out.transcripts.push(Transcript {
                    id,
                    path,
                    slug: slug.clone(),
                    placement,
                });
            };
            for entry in std::fs::read_dir(&root).into_iter().flatten().flatten() {
                let path = entry.path();
                if is_jsonl(&path) {
                    add(&mut out, path, false); // flat: <id>.jsonl
                } else if path.is_dir() {
                    let id = file_name(&path);
                    let main = path.join(format!("{id}.jsonl"));
                    if main.is_file() {
                        add(&mut out, main, false); // nested: <id>/<id>.jsonl
                    }
                    for sub in read_files(&path.join("subagents")) {
                        if is_jsonl(&sub) {
                            add(&mut out, sub, true);
                        }
                    }
                }
            }
        }
        for workspace in read_dirs(&home.join("chats")) {
            for chat_dir in read_dirs(&workspace) {
                let path = chat_dir.join("store.db");
                if path.is_file() {
                    out.chats.push(Chat {
                        id: file_name(&chat_dir),
                        path,
                    });
                }
            }
        }
        out
    }
}

fn read_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    v.sort();
    v
}

fn read_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    v.sort();
    v
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn is_jsonl(p: &Path) -> bool {
    p.is_file() && p.extension().is_some_and(|e| e == "jsonl")
}

/// The project a `projects/` directory name stands for: the existing directory whose path
/// encodes to it (decoding is lossy, `betabots-callimachus` could be one directory or two,
/// so this walks the disk; on Windows from the drive the name starts with). None for a
/// deleted project and for the numeric names Cursor gives windows with no folder open.
fn project_for_slug(slug: &str) -> Option<String> {
    dir_for_project_slug(&format!("-{slug}")).map(|p| p.to_string_lossy().into_owned())
}

/// The project a transcript's own tool paths show, for a directory that no longer exists:
/// the ancestor of an absolute path whose encoding is the directory name. Unix paths and
/// Windows drive paths (either separator) both count, and the walk is over the string, so
/// a Windows path read on any host splits at its backslashes.
fn project_from_paths<'a>(slug: &str, paths: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let want = normalize_slug(slug);
    for p in paths {
        let p = p.strip_prefix("file://").unwrap_or(p);
        // `file:///c:/x` leaves `/c:/x`.
        let p = p
            .strip_prefix('/')
            .filter(|q| is_drive_path(q))
            .unwrap_or(p);
        if !p.starts_with('/') && !is_drive_path(p) {
            continue;
        }
        let p = p.trim_end_matches(['/', '\\']);
        let mut end = p.len();
        loop {
            let dir = &p[..end];
            if normalize_slug(&slug_of(dir)) == want {
                return Some(dir.to_string());
            }
            match dir.rfind(['/', '\\']) {
                Some(i) if i > 0 => end = i,
                _ => break,
            }
        }
    }
    None
}

/// The editor's state.vscdb, opened read-only the first time a pass needs to look inside it
/// for one composer (see [`composer_covers`]), then reused for the rest of the pass.
struct StateDb<'a> {
    path: Option<&'a Path>,
    ro: Option<Option<Connection>>,
}

impl<'a> StateDb<'a> {
    fn new(path: Option<&'a Path>) -> Self {
        Self { path, ro: None }
    }

    /// Whether any bubble of `composer_id` holds text (what the composer pass would index),
    /// or None when state.vscdb can't be read right now.
    fn has_text(&mut self, composer_id: &str) -> Option<bool> {
        let path = self.path?;
        let ro = self
            .ro
            .get_or_insert_with(|| super::open_external_readonly(path).ok())
            .as_ref()?;
        composer_has_text(ro, composer_id).ok()
    }
}

/// One key-range read of the composer's bubbles, stopping at the first with text. Trims the
/// same whitespace the composer pass's `str::trim` does for ASCII.
fn composer_has_text(ro: &Connection, composer_id: &str) -> Result<bool> {
    let lo = format!("bubbleId:{composer_id}:");
    let hi = format!("bubbleId:{composer_id}:\u{10FFFF}");
    Ok(ro
        .query_row(
            "SELECT 1 FROM cursorDiskKV WHERE key >= ?1 AND key < ?2
               AND length(trim(CAST(json_extract(value, '$.text') AS TEXT),
                               ' ' || char(9, 10, 11, 12, 13))) > 0
             LIMIT 1",
            params![lo, hi],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// What a composer's stored fingerprint says about state.vscdb holding its conversation.
enum Coverage {
    Covered,
    NotCovered,
    /// An older build's fingerprint (no mark) for a composer with bubbles: whether they hold
    /// text is unknown until state.vscdb is asked. Carries the stored fingerprint.
    Unknown(String),
}

/// [`Coverage`] from our own index only (the composer pass writes these fingerprints).
fn stored_coverage(conn: &Connection, db: Option<&Path>, id: &str) -> Result<Coverage> {
    let Some(db) = db else {
        return Ok(Coverage::NotCovered);
    };
    let Some(fp) = stored_fingerprint(conn, &composer_key(db, id))? else {
        return Ok(Coverage::NotCovered);
    };
    let bubbles = fp
        .split(':')
        .nth(1)
        .and_then(|b| b.parse::<i64>().ok())
        .unwrap_or(0);
    Ok(if bubbles == 0 || fp.ends_with(EMPTY_MARK) {
        Coverage::NotCovered
    } else if fp.ends_with(TEXT_MARK) {
        Coverage::Covered
    } else {
        Coverage::Unknown(fp)
    })
}

/// Whether state.vscdb holds this agent's conversation: its composer fingerprint counted
/// bubbles with text. A fingerprint from before the marks existed is settled by asking
/// state.vscdb once and storing the answer as its mark, so an index upgraded from an older
/// build neither hides a transcript behind an empty composer nor re-asks every pass. If
/// state.vscdb can't be read just then, it counts as covered (what older builds assumed)
/// and is asked again next pass.
fn composer_covers(conn: &Connection, state: &mut StateDb, id: &str) -> Result<bool> {
    match stored_coverage(conn, state.path, id)? {
        Coverage::Covered => Ok(true),
        Coverage::NotCovered => Ok(false),
        Coverage::Unknown(fp) => {
            let Some(has_text) = state.has_text(id) else {
                return Ok(true);
            };
            let Some(db) = state.path else {
                return Ok(has_text);
            };
            let mark = if has_text { TEXT_MARK } else { EMPTY_MARK };
            store_fingerprint(conn, &composer_key(db, id), &format!("{fp}{mark}"))?;
            Ok(has_text)
        }
    }
}

/// Whether an indexed Cursor thread came from the CLI's chat store or an agent transcript
/// rather than an editor composer, so `cursor-agent --resume <id>` can pick it up. Read-only:
/// an unmarked fingerprint is checked against state.vscdb without being rewritten.
pub fn is_cli_thread(conn: &Connection, external_id: &str) -> Result<bool> {
    cli_thread_in(conn, global_db_path().as_deref(), external_id)
}

fn cli_thread_in(conn: &Connection, db: Option<&Path>, external_id: &str) -> Result<bool> {
    let mut state = StateDb::new(db);
    Ok(match stored_coverage(conn, db, external_id)? {
        Coverage::Covered => false,
        Coverage::NotCovered => true,
        Coverage::Unknown(_) => state.has_text(external_id) == Some(false),
    })
}

/// A re-extracted thread whose placement no longer resolves (its project folder renamed, a
/// volume unmounted) keeps the project it was stored with: `upsert_thread` writes
/// `project_path` as given, so None would erase what an earlier pass or
/// [`enrich_composers`] found.
fn keep_known_project(conn: &Connection, sid: i64, thread: &mut ParsedThread) -> Result<()> {
    if thread.project_path.is_some() {
        return Ok(());
    }
    thread.project_path = conn
        .query_row(
            "SELECT project_path FROM threads
             WHERE source_id = ?1 AND external_id = ?2 AND project_path != ''",
            params![sid, thread.external_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    Ok(())
}

/// Give composer threads the project (and subagent flag) their transcripts show. The
/// composer pass sets both on threads it re-extracts; this covers the settled ones it
/// skips, which would otherwise keep a null project until their next change. One read of
/// the source's threads, and a write only for threads that change.
fn enrich_composers(
    conn: &mut Connection,
    sid: i64,
    placement: &HashMap<String, Placement>,
) -> Result<()> {
    let stale: Vec<(String, Option<String>, bool)> = {
        let mut stmt = conn.prepare(
            "SELECT external_id, COALESCE(project_path, ''), is_subagent FROM threads
             WHERE source_id = ?1",
        )?;
        let rows = stmt.query_map([sid], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, bool>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, project, sub) = row?;
            let Some(p) = placement.get(&id) else {
                continue;
            };
            let new_project = p.project.clone().filter(|_| project.is_empty());
            if new_project.is_some() || (p.is_subagent && !sub) {
                out.push((id, new_project, p.is_subagent || sub));
            }
        }
        out
    };
    if stale.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    for (id, project, sub) in &stale {
        let key = project.as_deref().and_then(canonical_project);
        tx.execute(
            "UPDATE threads SET project_path = COALESCE(?3, project_path),
                project_key = COALESCE(?4, project_key), is_subagent = ?5
             WHERE source_id = ?1 AND external_id = ?2",
            params![sid, id, project, key, sub],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Context blocks Cursor injects into a user turn around (or instead of) what was typed.
const CONTEXT_TAGS: &[&str] = &[
    "user_info",
    "agent_transcripts",
    "agent_skills",
    "attached_files",
    "image_files",
    "external_links",
    "git_status",
    "git_diff_from_branch_to_main",
    "timestamp",
    "system_reminder",
    "rules",
    "mcp_file_system",
    "mcp_file_system_servers",
    "open_and_recently_viewed_files",
];

/// The inner text of every `<tag>...</tag>` block in `s`, in order.
fn tag_blocks<'a>(s: &'a str, tag: &str) -> Vec<&'a str> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        let Some(j) = after.find(&close) else {
            break;
        };
        out.push(&after[..j]);
        rest = &after[j + close.len()..];
    }
    out
}

/// `s` without the `<tag>...</tag>` blocks of `tags` (an unclosed block is kept).
fn strip_tag_blocks(s: &str, tags: &[&str]) -> String {
    let mut out = s.to_string();
    for tag in tags {
        let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
        while let Some(i) = out.find(&open) {
            let Some(j) = out[i..].find(&close) else {
                break;
            };
            out.replace_range(i..i + j + close.len(), "");
        }
    }
    out
}

/// What the user typed in a Cursor user turn, plus the turn's `<timestamp>` when it has
/// one: the `<user_query>` blocks when present (attachments and environment blocks around
/// them are context, not the prompt), otherwise the text minus the injected blocks.
fn user_turn(text: &str) -> (String, Option<i64>) {
    let ts = tag_blocks(text, "timestamp")
        .first()
        .and_then(|t| parse_cursor_timestamp(t));
    let queries = tag_blocks(text, "user_query");
    let typed = if queries.is_empty() {
        strip_tag_blocks(text, CONTEXT_TAGS)
    } else {
        queries
            .iter()
            .map(|q| q.trim())
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    (typed.trim().to_string(), ts)
}

/// `Wednesday, May 6, 2026, 9:38 PM (UTC-7)` (also `(UTC+5:30)` and `(UTC)`) as epoch
/// seconds. The only clock an agent transcript carries.
fn parse_cursor_timestamp(s: &str) -> Option<i64> {
    let (date, zone) = s.trim().split_once(" (UTC")?;
    let local =
        chrono::NaiveDateTime::parse_from_str(date.trim(), "%A, %b %d, %Y, %I:%M %p").ok()?;
    let zone = zone.trim_end_matches(')');
    let secs = if zone.is_empty() {
        0
    } else {
        let sign = if zone.starts_with('-') { -1 } else { 1 };
        let (h, m) = zone[1..].split_once(':').unwrap_or((&zone[1..], "0"));
        sign * (h.parse::<i32>().ok()? * 3600 + m.parse::<i32>().ok()? * 60)
    };
    let offset = chrono::FixedOffset::east_opt(secs)?;
    Some(local.and_local_timezone(offset).single()?.timestamp())
}

/// `Name: {input json}` for a tool call, the shape every source stores calls in.
fn push_tool_call(
    out: &mut Vec<ParsedMessage>,
    name: &str,
    input: Option<&Value>,
    ts: Option<i64>,
) {
    let args = input.map(|v| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    });
    let text = match args {
        Some(a) if !a.is_empty() => format!("{name}: {a}"),
        _ => name.to_string(),
    };
    push_message(out, "assistant", &text, Some(name.to_string()), ts);
}

/// A tool result's text through the shared index-time cap (`claude::cap_tool_text`).
fn push_tool_result(out: &mut Vec<ParsedMessage>, result: Option<&Value>, ts: Option<i64>) {
    let text = match result {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(_)) => super::common::join_text_parts(result),
        Some(Value::Null) | None => return,
        Some(other) => other.to_string(),
    };
    push_message(
        out,
        "tool",
        &super::claude::cap_tool_text(text.trim()),
        None,
        ts,
    );
}

/// Absolute paths a tool call names (`path`, `target_directory`, ...), for
/// [`project_from_paths`].
fn tool_paths(input: &Value, out: &mut Vec<String>) {
    if let Some(obj) = input.as_object() {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                if (k.contains("path") || k.contains("directory"))
                    && (s.starts_with('/') || is_drive_path(s))
                {
                    out.push(s.to_string());
                }
            }
        }
    }
}

/// Parse an agent transcript: one `{"role", "message": {"content": [...]}}` per line, with
/// `text` and `tool_use` parts (no tool results and no per-line clock; a user turn's
/// `<timestamp>` block dates it and the turns after it).
fn parse_transcript(path: &Path) -> Result<(Vec<ParsedMessage>, Vec<String>)> {
    let content = std::fs::read_to_string(path)?;
    let mut out = Vec::new();
    let mut paths = Vec::new();
    let mut ts: Option<i64> = None;
    for line in content.lines() {
        let Ok(obj) = serde_json::from_str::<Value>(line) else {
            continue; // a half-written last line, or noise
        };
        let role = obj.get("role").and_then(Value::as_str).unwrap_or_default();
        let parts = obj.get("message").and_then(|m| m.get("content"));
        let parts: Vec<&Value> = match parts {
            Some(Value::Array(a)) => a.iter().collect(),
            Some(v @ Value::String(_)) => vec![v],
            _ => continue,
        };
        for part in parts {
            let (kind, text) = match part {
                Value::String(s) => ("text", Some(s.as_str())),
                p => (
                    p.get("type").and_then(Value::as_str).unwrap_or_default(),
                    p.get("text").and_then(Value::as_str),
                ),
            };
            match (role, kind) {
                ("user", "text") => {
                    let (typed, at) = user_turn(text.unwrap_or_default());
                    ts = at.or(ts);
                    push_message(&mut out, "user", &typed, None, ts);
                }
                ("assistant", "text") => {
                    push_message(&mut out, "assistant", text.unwrap_or_default(), None, ts)
                }
                (_, "tool_use") => {
                    let name = part.get("name").and_then(Value::as_str).unwrap_or("tool");
                    let input = part.get("input");
                    if let Some(i) = input {
                        tool_paths(i, &mut paths);
                    }
                    push_tool_call(&mut out, name, input, ts);
                }
                (_, "tool_result") => push_tool_result(&mut out, part.get("content"), ts),
                _ => {} // reasoning, images, unknown parts
            }
        }
    }
    Ok((out, paths))
}

/// A file's (birth, modification) time in epoch seconds, where the platform reports them.
fn file_times(path: &Path) -> (Option<i64>, Option<i64>) {
    let secs = |t: std::io::Result<std::time::SystemTime>| {
        t.ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
    };
    match std::fs::metadata(path) {
        Ok(m) => (secs(m.created()), secs(m.modified())),
        Err(_) => (None, None),
    }
}

fn index_transcript(
    conn: &mut Connection,
    sid: i64,
    state: &mut StateDb,
    t: &Transcript,
    chat_ids: &HashSet<&str>,
) -> Result<Option<usize>> {
    // Covered by a richer store: skip without recording the file, so the transcript is
    // picked up if that store ever stops covering it.
    if chat_ids.contains(t.id.as_str()) || composer_covers(conn, state, &t.id)? {
        return Ok(None);
    }
    let (unchanged, mtime, size) = file_change_state(conn, &t.path)?;
    if unchanged {
        return Ok(None);
    }
    let key = t.path.to_string_lossy();
    if crate::cleanup::is_tombstoned(conn, sid, &t.id)? {
        set_file_state(conn, &key, KIND, mtime, size)?;
        return Ok(None);
    }
    let (messages, paths) = parse_transcript(&t.path)?;
    let project = t
        .placement
        .project
        .clone()
        .or_else(|| project_from_paths(&t.slug, paths.iter().map(String::as_str)));
    let user_texts = messages
        .iter()
        .filter(|m| m.role == "user")
        .take(3)
        .map(|m| m.text.as_str());
    if messages.is_empty() || crate::agent::is_app_cli_session(project.as_deref(), user_texts) {
        set_file_state(conn, &key, KIND, mtime, size)?;
        return Ok(None);
    }
    let (born, modified) = file_times(&t.path);
    let first_ts = messages.iter().find_map(|m| m.ts);
    let mut thread = ParsedThread {
        external_id: t.id.clone(),
        title: messages
            .iter()
            .find(|m| m.role == "user")
            .map(|m| truncate_title(m.text.lines().next().unwrap_or_default())),
        project_path: project,
        created_at: first_ts.or(born).or(modified),
        updated_at: modified,
        is_subagent: t.placement.is_subagent,
        messages,
        ..Default::default()
    };
    keep_known_project(conn, sid, &mut thread)?;
    let n = upsert_thread(conn, sid, &thread)?;
    set_file_state(conn, &key, KIND, mtime, size)?;
    Ok(Some(n))
}

/// What a CLI chat store holds.
#[derive(Debug, Default)]
struct ChatParse {
    title: Option<String>,
    created_ms: Option<i64>,
    workspace: Option<String>,
    messages: Vec<ParsedMessage>,
}

/// A protobuf base-128 varint at `*i`, advancing past it.
fn pb_varint(b: &[u8], i: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *b.get(*i)?;
        *i += 1;
        v |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Some(v);
        }
    }
    None
}

/// The length-delimited fields of a protobuf message, in order, as `(field, bytes)`;
/// varint and fixed-width fields are stepped over. Stops at the first malformed or
/// unsupported (group) field, keeping what came before.
fn pb_bytes_fields(b: &[u8]) -> Vec<(u64, &[u8])> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let Some(key) = pb_varint(b, &mut i) else {
            break;
        };
        let (field, wire) = (key >> 3, key & 7);
        match wire {
            0 => {
                if pb_varint(b, &mut i).is_none() {
                    break;
                }
            }
            1 => i += 8,
            5 => i += 4,
            2 => {
                let Some(len) = pb_varint(b, &mut i).and_then(|l| usize::try_from(l).ok()) else {
                    break;
                };
                let Some(bytes) = i.checked_add(len).and_then(|end| b.get(i..end)) else {
                    break;
                };
                out.push((field, bytes));
                i += len;
            }
            _ => break,
        }
    }
    out
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

fn hex_encode(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Read a CLI chat store. `meta` holds one hex-encoded JSON value naming the root blob
/// (`latestRootBlobId`); the root blob is a protobuf whose field 1 repeats the 32-byte ids
/// of the message blobs in order, field 4 holds the turn still being written as inline
/// JSON, and field 9 is the workspace URI. Each message blob is a JSON
/// `{role, content}` with `content` a string or `text` / `tool-call` / `tool-result` /
/// `reasoning` parts. The format is undocumented (read from Cursor CLI 2026.06 stores), so
/// anything unexpected is skipped rather than failing the chat.
fn parse_chat(ro: &Connection) -> Result<ChatParse> {
    let mut out = ChatParse::default();
    // No `meta` / `blobs` table: a layout this parser doesn't know, or a WAL-only store read
    // without its WAL. Nothing to read, which is not an error worth one per pass.
    let tables: i64 = ro.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN ('meta', 'blobs')",
        [],
        |r| r.get(0),
    )?;
    if tables < 2 {
        return Ok(out);
    }
    let raw: Option<String> = ro
        .query_row(
            "SELECT value FROM meta ORDER BY key = '0' DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let Some(meta) = raw
        .and_then(|h| hex_decode(&h))
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    else {
        return Ok(out);
    };
    out.title = meta
        .get("name")
        .and_then(Value::as_str)
        .filter(|n| !n.trim().is_empty())
        .map(truncate_title);
    out.created_ms = meta.get("createdAt").and_then(Value::as_i64);
    let Some(root_id) = meta.get("latestRootBlobId").and_then(Value::as_str) else {
        return Ok(out);
    };
    let mut blob = ro.prepare("SELECT data FROM blobs WHERE id = ?1")?;
    let mut load = |id: &str| -> Option<Vec<u8>> {
        blob.query_row([id], |r| r.get::<_, Vec<u8>>(0))
            .optional()
            .ok()
            .flatten()
    };
    let Some(root) = load(root_id) else {
        return Ok(out);
    };
    for (field, bytes) in pb_bytes_fields(&root) {
        let json = match field {
            1 if bytes.len() == 32 => load(&hex_encode(bytes)),
            4 => Some(bytes.to_vec()),
            9 => {
                out.workspace = std::str::from_utf8(bytes)
                    .ok()
                    .and_then(super::copilot::uri_to_path);
                None
            }
            _ => None,
        };
        if let Some(msg) = json.and_then(|j| serde_json::from_slice::<Value>(&j).ok()) {
            chat_message(&mut out.messages, &msg);
        }
    }
    Ok(out)
}

fn chat_message(out: &mut Vec<ParsedMessage>, msg: &Value) {
    let role = msg.get("role").and_then(Value::as_str).unwrap_or_default();
    let parts: Vec<&Value> = match msg.get("content") {
        Some(Value::Array(a)) => a.iter().collect(),
        Some(v @ Value::String(_)) => vec![v],
        _ => return,
    };
    for part in parts {
        let (kind, text) = match part {
            Value::String(s) => ("text", Some(s.as_str())),
            p => (
                p.get("type").and_then(Value::as_str).unwrap_or_default(),
                p.get("text").and_then(Value::as_str),
            ),
        };
        match (role, kind) {
            // The CLI's system prompt is the agent's, not the conversation.
            ("system", _) => {}
            ("user", "text") => {
                let (typed, ts) = user_turn(text.unwrap_or_default());
                push_message(out, "user", &typed, None, ts);
            }
            ("assistant", "text") => {
                push_message(out, "assistant", text.unwrap_or_default(), None, None)
            }
            ("assistant", "tool-call") => {
                let name = part
                    .get("toolName")
                    .and_then(Value::as_str)
                    .unwrap_or("tool");
                push_tool_call(out, name, part.get("args"), None);
            }
            ("tool", "tool-result") => push_tool_result(out, part.get("result"), None),
            _ => {} // reasoning (often redacted), images, unknown parts
        }
    }
}

fn index_chat(
    conn: &mut Connection,
    sid: i64,
    state: &mut StateDb,
    chat: &Chat,
    placement: &HashMap<String, Placement>,
) -> Result<Option<usize>> {
    if composer_covers(conn, state, &chat.id)? {
        return Ok(None);
    }
    // WAL-mode store: new turns land in the -wal until a checkpoint, so it is part of the
    // change key (as for state.vscdb).
    let wal = wal_path(&chat.path);
    let (main_unchanged, mtime, size) = file_change_state(conn, &chat.path)?;
    let wal_state = if wal.exists() {
        Some(file_change_state(conn, &wal)?)
    } else {
        None
    };
    if main_unchanged && wal_state.as_ref().is_none_or(|(u, _, _)| *u) {
        return Ok(None);
    }
    let record = |conn: &Connection, outcome: Option<&str>| -> Result<()> {
        if let Some(outcome) = outcome {
            set_chat_outcome(conn, &chat.path, outcome)?;
        }
        set_file_state(conn, &chat.path.to_string_lossy(), KIND, mtime, size)?;
        if let Some((_, m, s)) = wal_state {
            set_file_state(conn, &wal.to_string_lossy(), KIND, m, s)?;
        }
        Ok(())
    };
    if crate::cleanup::is_tombstoned(conn, sid, &chat.id)? {
        record(conn, None)?;
        return Ok(None);
    }
    let parsed = read_chat(&chat.path)?;
    let outcome = if parsed.messages.is_empty() {
        CHAT_EMPTY
    } else {
        CHAT_HAS_MESSAGES
    };
    // meta.json beside the store carries the title and both clocks; it is rewritten with
    // the store, and its title follows renames the store's meta row may lag.
    let side = super::common::read_json(&chat.path.with_file_name("meta.json"));
    let side_ms = |k: &str| side.as_ref().and_then(|v| v.get(k)).and_then(Value::as_i64);
    let project = parsed
        .workspace
        .clone()
        .or_else(|| placement.get(&chat.id).and_then(|p| p.project.clone()));
    let user_texts = parsed
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .take(3)
        .map(|m| m.text.as_str());
    if parsed.messages.is_empty()
        || crate::agent::is_app_cli_session(project.as_deref(), user_texts)
    {
        record(conn, Some(outcome))?;
        return Ok(None);
    }
    let modified = file_times(
        &wal_state
            .as_ref()
            .map_or(chat.path.clone(), |_| wal.clone()),
    )
    .1;
    let created_at = side_ms("createdAtMs")
        .or(parsed.created_ms)
        .map(|ms| ms / 1000);
    let title = side
        .as_ref()
        .and_then(|v| v.get("title"))
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
        .map(truncate_title)
        .or(parsed.title)
        .or_else(|| {
            parsed
                .messages
                .iter()
                .find(|m| m.role == "user")
                .map(|m| truncate_title(m.text.lines().next().unwrap_or_default()))
        });
    let mut thread = ParsedThread {
        external_id: chat.id.clone(),
        title,
        project_path: project,
        created_at,
        updated_at: side_ms("updatedAtMs")
            .map(|ms| ms / 1000)
            .or(modified)
            .or(created_at),
        is_subagent: placement.get(&chat.id).is_some_and(|p| p.is_subagent),
        messages: parsed.messages,
        ..Default::default()
    };
    keep_known_project(conn, sid, &mut thread)?;
    let n = upsert_thread(conn, sid, &thread)?;
    record(conn, Some(outcome))?;
    Ok(Some(n))
}

/// What the last read of a CLI chat store found, stored beside its change key: the
/// transcript of the same id defers to the store only while this says it had messages.
const CHAT_HAS_MESSAGES: &str = "messages";
const CHAT_EMPTY: &str = "empty";

fn chat_outcome_key(store: &Path) -> String {
    format!("{}#parsed", store.to_string_lossy())
}

fn chat_outcome(conn: &Connection, store: &Path) -> Result<Option<String>> {
    stored_fingerprint(conn, &chat_outcome_key(store))
}

fn set_chat_outcome(conn: &Connection, store: &Path, outcome: &str) -> Result<()> {
    store_fingerprint(conn, &chat_outcome_key(store), outcome)
}

/// Read a CLI chat store. A store whose tables all sit in its `-wal` (a real one here: a
/// 4 KB store.db beside a 290 KB -wal) reads as empty when the open had to fall back to
/// `immutable=1`, which ignores the WAL. Then the store and its -wal are copied to a temp
/// dir and the copy is opened read-write, so SQLite replays the WAL into it without
/// touching Cursor's files. The -shm is left behind on purpose: without one SQLite rebuilds
/// the WAL index from the -wal itself, while a copy taken mid-write could disagree with it.
fn read_chat(store: &Path) -> Result<ChatParse> {
    let direct = super::open_external_readonly(store).and_then(|ro| parse_chat(&ro));
    if direct.as_ref().is_ok_and(|p| !p.messages.is_empty()) || !wal_path(store).exists() {
        return direct;
    }
    // A failed copy is an error even after an empty direct read: recording the store as
    // empty would hand its session to the transcript until the store next changes.
    read_chat_copy(store).map_err(|e| match direct {
        Err(first) => first.context(format!("reading a copy also failed: {e:#}")),
        Ok(_) => e,
    })
}

fn read_chat_copy(store: &Path) -> Result<ChatParse> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "callimachus-cursor-chat-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let result = (|| -> Result<ChatParse> {
        let copy = dir.join("store.db");
        std::fs::copy(store, &copy)?;
        std::fs::copy(wal_path(store), wal_path(&copy))?;
        let conn = Connection::open(&copy)?;
        parse_chat(&conn)
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a tiny Cursor-shaped DB and verify we extract a thread + messages.
    #[test]
    fn extracts_from_cursor_shaped_db() {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "callimachus_cursorsrc_{}.vscdb",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        {
            let src = Connection::open(&path).unwrap();
            src.execute_batch(
                "CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value BLOB);
                 INSERT INTO cursorDiskKV VALUES
                   ('composerData:abc', json('{\"composerId\":\"abc\",\"name\":\"Venv help\",\"createdAt\":1733732099602}')),
                   ('bubbleId:abc:b1', json('{\"type\":1,\"text\":\"how do i activate venv\",\"timingInfo\":{\"clientStartTime\":1733732100000}}')),
                   ('bubbleId:abc:b2', json('{\"type\":2,\"text\":\"run source .venv/bin/activate\",\"timingInfo\":{\"clientStartTime\":1733732105000}}'));",
            )
            .unwrap();
        }

        // list_composers
        let ro = super::super::open_external_readonly(&path).unwrap();
        let composers = list_composers(&ro).unwrap();
        assert_eq!(composers.len(), 1);
        assert_eq!(composers[0].id, "abc");
        assert_eq!(composers[0].name.as_deref(), Some("Venv help"));
        drop(ro);

        // Full scan_path: thread + ordered messages land in the canonical store.
        let mut db = std::env::temp_dir();
        db.push(format!("callimachus_cursordst_{}.db", std::process::id()));
        let mut conn = crate::db::open(&db).unwrap();
        let report = scan_path(&mut conn, &path, &HashMap::new(), &mut || {}).unwrap();
        assert_eq!(report.threads_indexed, 1);
        assert_eq!(report.messages_indexed, 2);

        let detail = crate::search::thread_detail(
            &conn,
            conn.query_row("SELECT id FROM threads", [], |r| r.get(0))
                .unwrap(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(detail.title.as_deref(), Some("Venv help"));
        assert_eq!(detail.messages.len(), 2);
        assert_eq!(detail.messages[0].role, "user"); // ordered by clientStartTime
        assert_eq!(detail.messages[1].role, "assistant");
    }

    /// A change to the store re-extracts only the composers that changed, catches a bubble
    /// growing in place (streaming) and never re-imports a deleted composer.
    #[test]
    fn rescans_touch_only_changed_composers() {
        let dir = std::env::temp_dir().join(format!("calli_cursor_fp_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src_path = dir.join("state.vscdb");
        let src = Connection::open(&src_path).unwrap();
        src.execute_batch(
            "CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value BLOB);
             INSERT INTO cursorDiskKV VALUES
               ('composerData:a', json('{\"name\":\"A\",\"createdAt\":1733732099602}')),
               ('bubbleId:a:1', json('{\"type\":1,\"text\":\"question a\"}')),
               ('composerData:b', json('{\"name\":\"B\",\"createdAt\":1733732099602}')),
               ('bubbleId:b:1', json('{\"type\":1,\"text\":\"question b\"}'));",
        )
        .unwrap();

        let mut conn = crate::db::open(&dir.join("index.db")).unwrap();
        let first = scan_path(&mut conn, &src_path, &HashMap::new(), &mut || {}).unwrap();
        assert_eq!(first.threads_indexed, 2);

        // Composer a's bubble grows in place (same bubble count): only a is re-extracted.
        src.execute(
            "UPDATE cursorDiskKV SET value = json('{\"type\":1,\"text\":\"question a, now longer\"}')
             WHERE key = 'bubbleId:a:1'",
            [],
        )
        .unwrap();
        // Force the file-level change key to differ (mtime has 1s resolution).
        conn.execute(
            "DELETE FROM index_state WHERE path NOT LIKE '%#composer:%'",
            [],
        )
        .unwrap();
        let second = scan_path(&mut conn, &src_path, &HashMap::new(), &mut || {}).unwrap();
        assert_eq!(second.threads_indexed, 1, "unchanged composer b is skipped");

        // Deleting thread b tombstones it; a later change to b doesn't bring it back.
        let b: i64 = conn
            .query_row("SELECT id FROM threads WHERE external_id = 'b'", [], |r| {
                r.get(0)
            })
            .unwrap();
        crate::cleanup::delete_threads(&mut conn, &[b]).unwrap();
        src.execute(
            "INSERT INTO cursorDiskKV VALUES ('bubbleId:b:2', json('{\"type\":2,\"text\":\"answer\"}'))",
            [],
        )
        .unwrap();
        conn.execute(
            "DELETE FROM index_state WHERE path NOT LIKE '%#composer:%'",
            [],
        )
        .unwrap();
        let third = scan_path(&mut conn, &src_path, &HashMap::new(), &mut || {}).unwrap();
        assert_eq!(third.threads_indexed, 0);
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM threads WHERE external_id = 'b'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "tombstoned composer stays deleted");
    }

    fn temp_dir(name: &str) -> PathBuf {
        // Canonical, so the slug walk from `/` can follow it (macOS temp is under the
        // `/var` symlink, which a directory walk does not enter).
        let base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let dir = base.join(format!("calli_cursor_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn jsonl(lines: &[serde_json::Value]) -> String {
        lines.iter().map(|l| format!("{l}\n")).collect()
    }

    fn user_line(text: &str) -> serde_json::Value {
        serde_json::json!({"role":"user","message":{"content":[{"type":"text","text": text}]}})
    }

    fn assistant_line(text: &str) -> serde_json::Value {
        serde_json::json!({"role":"assistant","message":{"content":[
            {"type":"text","text": text},
            {"type":"tool_use","name":"Read","input":{"path":"/gone/my-app/src/main.rs"}}
        ]}})
    }

    /// A Cursor-shaped home: a project directory named after an existing (dashed) folder
    /// with a nested transcript and a subagent, plus a flat (unobserved layout) transcript under
    /// a directory whose project is gone.
    fn agent_home(root: &Path) -> (PathBuf, PathBuf) {
        let project = root.join("work").join("my-app");
        std::fs::create_dir_all(&project).unwrap();
        let slug = slug_of(&project.to_string_lossy())
            .trim_start_matches('-')
            .to_string();
        let home = root.join(".cursor");
        let nested = home
            .join("projects")
            .join(&slug)
            .join("agent-transcripts")
            .join("n1");
        std::fs::create_dir_all(nested.join("subagents")).unwrap();
        std::fs::write(
            nested.join("n1.jsonl"),
            jsonl(&[
                user_line("<timestamp>Wednesday, May 6, 2026, 9:38 PM (UTC-7)</timestamp>\n<user_query>\nfix the login redirect\n</user_query>\n<attached_files>\nnoise\n</attached_files>"),
                assistant_line("Looking at the router."),
            ]),
        )
        .unwrap();
        std::fs::write(
            nested.join("subagents").join("s1.jsonl"),
            jsonl(&[user_line(
                "<user_query>explore the auth module</user_query>",
            )]),
        )
        .unwrap();
        let flat = home
            .join("projects")
            .join("gone-my-app")
            .join("agent-transcripts");
        std::fs::create_dir_all(&flat).unwrap();
        std::fs::write(
            flat.join("f1.jsonl"),
            jsonl(&[
                user_line("<user_query>cli question</user_query>"),
                assistant_line("cli answer"),
            ]),
        )
        .unwrap();
        (home, project)
    }

    #[test]
    fn user_turns_keep_what_was_typed() {
        let (t, ts) = user_turn(
            "<timestamp>Friday, Apr 24, 2026, 7:24 PM (UTC-7)</timestamp>\n<user_query>\n  is my rpc optimized\n</user_query>\n<attached_files>x</attached_files>",
        );
        assert_eq!(t, "is my rpc optimized");
        assert_eq!(ts, Some(1777083840), "2026-04-25T02:24:00Z");
        let (t, _) = user_turn("<agent_transcripts>\nhistory lives here\n</agent_transcripts>\nYour previous response was interrupted.");
        assert_eq!(t, "Your previous response was interrupted.");
        assert_eq!(user_turn("<user_info>\nOS: darwin\n</user_info>").0, "");
        assert_eq!(
            parse_cursor_timestamp("Monday, Jan 5, 2026, 12:05 AM (UTC+5:30)"),
            Some(1767551700)
        );
        assert_eq!(
            parse_cursor_timestamp("Monday, Jan 5, 2026, 12:05 AM (UTC)"),
            Some(1767571500)
        );
        assert_eq!(parse_cursor_timestamp("yesterday"), None);
    }

    #[test]
    fn discovers_nested_flat_and_subagent_transcripts() {
        let root = temp_dir("discover");
        let (home, project) = agent_home(&root);
        let stores = AgentStores::discover(&home);
        let mut ids: Vec<&str> = stores.transcripts.iter().map(|t| t.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["f1", "n1", "s1"]);
        let project = Some(project.to_string_lossy().into_owned());
        assert_eq!(
            stores.placement["n1"],
            Placement {
                project: project.clone(),
                is_subagent: false
            }
        );
        assert_eq!(
            stores.placement["s1"],
            Placement {
                project,
                is_subagent: true
            }
        );
        assert_eq!(
            stores.placement["f1"].project, None,
            "its directory no longer exists"
        );
        assert_eq!(
            project_from_paths("gone-my-app", ["relative", "/gone/my-app/src/main.rs"]),
            Some("/gone/my-app".into()),
            "a deleted project still shows in the transcript's tool paths"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Transcripts become threads (subagents flagged, projects recovered, tool calls kept),
    /// unless state.vscdb already holds the same agent, in which case the transcript only
    /// lends the composer its project; an empty composer leaves the transcript in charge.
    #[test]
    fn transcripts_fill_in_what_state_vscdb_lacks() {
        let root = temp_dir("transcripts");
        let (home, project) = agent_home(&root);
        let vscdb = root.join("state.vscdb");
        let src = Connection::open(&vscdb).unwrap();
        src.execute_batch(
            "CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value BLOB);
             INSERT INTO cursorDiskKV VALUES
               ('composerData:n1', json('{\"name\":\"From the editor\",\"createdAt\":1733732099602}')),
               ('bubbleId:n1:1', json('{\"type\":1,\"text\":\"editor question\"}')),
               ('composerData:f1', json('{\"name\":\"Empty shell\",\"createdAt\":1733732099602}')),
               ('bubbleId:f1:1', json('{\"type\":1,\"text\":\"\"}'));",
        )
        .unwrap();
        let mut conn = crate::db::open(&root.join("index.db")).unwrap();
        let stores = AgentStores::discover(&home);
        let report = scan_all(&mut conn, Some(&vscdb), &stores, &mut || {}).unwrap();
        assert_eq!(report.errors, 0);
        let row = |id: &str| -> (String, Option<String>, bool, i64) {
            conn.query_row(
                "SELECT t.title, t.project_path, t.is_subagent, COUNT(m.id) FROM threads t
                 LEFT JOIN messages m ON m.thread_id = t.id WHERE t.external_id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap()
        };
        let project = project.to_string_lossy().into_owned();
        let n1 = row("n1");
        assert_eq!(n1.0, "From the editor", "the composer wins");
        assert_eq!(
            n1.1.as_deref(),
            Some(project.as_str()),
            "with the transcript's project"
        );
        assert_eq!(n1.3, 1);
        let s1 = row("s1");
        assert!(s1.2, "subagent transcript is flagged");
        assert_eq!(s1.1.as_deref(), Some(project.as_str()));
        let f1 = row("f1");
        assert_eq!(
            f1.0, "cli question",
            "an empty composer leaves the transcript in charge"
        );
        assert_eq!(f1.1.as_deref(), Some("/gone/my-app"));
        assert_eq!(f1.3, 3, "user, assistant and the tool call");
        let call: String = conn
            .query_row(
                "SELECT text FROM messages WHERE tool_name = 'Read'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(call.starts_with("Read: {"));

        // Nothing changed: nothing is re-indexed.
        let again = scan_all(&mut conn, Some(&vscdb), &stores, &mut || {}).unwrap();
        assert_eq!(again.threads_indexed, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A composer indexed before its transcript existed gets the project without being
    /// re-extracted (settled composers are skipped by fingerprint).
    #[test]
    fn settled_composers_get_their_project_from_a_later_transcript() {
        let root = temp_dir("enrich");
        let vscdb = root.join("state.vscdb");
        let src = Connection::open(&vscdb).unwrap();
        src.execute_batch(
            "CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value BLOB);
             INSERT INTO cursorDiskKV VALUES
               ('composerData:n1', json('{\"name\":\"Old\",\"createdAt\":1,\"lastUpdatedAt\":1}')),
               ('bubbleId:n1:1', json('{\"type\":1,\"text\":\"old question\"}'));",
        )
        .unwrap();
        let mut conn = crate::db::open(&root.join("index.db")).unwrap();
        scan_all(&mut conn, Some(&vscdb), &AgentStores::default(), &mut || {}).unwrap();
        let (home, project) = agent_home(&root);
        let stores = AgentStores::discover(&home);
        conn.execute(
            "DELETE FROM index_state WHERE path NOT LIKE '%#composer:%'",
            [],
        )
        .unwrap();
        let report = scan_all(&mut conn, Some(&vscdb), &stores, &mut || {}).unwrap();
        let path: Option<String> = conn
            .query_row(
                "SELECT project_path FROM threads WHERE external_id = 'n1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(path, Some(project.to_string_lossy().into_owned()));
        // The composer was skipped as settled; only the two transcripts it lacks indexed.
        assert_eq!(report.threads_indexed, 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    fn pb_field(out: &mut Vec<u8>, field: u8, bytes: &[u8]) {
        out.push(field << 3 | 2);
        let mut len = bytes.len();
        loop {
            let b = (len & 0x7f) as u8;
            len >>= 7;
            if len == 0 {
                out.push(b);
                break;
            }
            out.push(b | 0x80);
        }
        out.extend_from_slice(bytes);
    }

    /// A Cursor CLI chat store: messages in root-blob order (plus the in-flight turn),
    /// tool results capped, the system prompt and environment block dropped, and the
    /// workspace from the root blob. It wins over the transcript of the same agent.
    #[test]
    fn indexes_cli_chat_stores() {
        let root = temp_dir("chat");
        let home = root.join(".cursor");
        let chat_dir = home.join("chats").join("ab12").join("c1");
        std::fs::create_dir_all(&chat_dir).unwrap();
        let store = Connection::open(chat_dir.join("store.db")).unwrap();
        store
            .execute_batch(
                "CREATE TABLE blobs (id TEXT PRIMARY KEY, data BLOB);
                 CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);",
            )
            .unwrap();
        let big = "y".repeat(40_000);
        let msgs = [
            serde_json::json!({"role":"system","content":"You are an AI coding assistant"}),
            serde_json::json!({"role":"user","content":"<user_info>\nWorkspace Path: /w\n</user_info>"}),
            serde_json::json!({"role":"user","content":[{"type":"text","text":"<user_query>\nwhy is the build red\n</user_query>"}]}),
            serde_json::json!({"role":"assistant","content":[
                {"type":"redacted-reasoning","data":"opaque"},
                {"type":"text","text":"Checking the log."},
                {"type":"tool-call","toolCallId":"t1","toolName":"Shell","args":{"command":"cargo test"}}]}),
            serde_json::json!({"role":"tool","content":[{"type":"tool-result","toolCallId":"t1","toolName":"Shell","result": big}]}),
        ];
        let mut root_blob = Vec::new();
        for (i, m) in msgs.iter().enumerate() {
            let id = [i as u8 + 1; 32];
            store
                .execute(
                    "INSERT INTO blobs VALUES (?1, ?2)",
                    params![hex_encode(&id), m.to_string().into_bytes()],
                )
                .unwrap();
            pb_field(&mut root_blob, 1, &id);
        }
        let pending = serde_json::json!({"role":"assistant","content":[{"type":"text","text":"The test fails on a missing env var."}]});
        pb_field(&mut root_blob, 4, pending.to_string().as_bytes());
        pb_field(&mut root_blob, 9, b"file:///Users/me/my%20app");
        store
            .execute("INSERT INTO blobs VALUES ('root', ?1)", [root_blob])
            .unwrap();
        let meta = serde_json::json!({"agentId":"c1","latestRootBlobId":"root","name":"Red build","createdAt":1781729743860_i64});
        store
            .execute(
                "INSERT INTO meta VALUES ('0', ?1)",
                [hex_encode(meta.to_string().as_bytes())],
            )
            .unwrap();
        drop(store);
        std::fs::write(
            chat_dir.join("meta.json"),
            r#"{"schemaVersion":1,"createdAtMs":1781729743860,"title":"Red build, renamed","updatedAtMs":1781729749877}"#,
        )
        .unwrap();
        // The CLI also writes a (poorer) transcript of the same agent.
        let t = home
            .join("projects")
            .join("Users-me-my-app")
            .join("agent-transcripts");
        std::fs::create_dir_all(&t).unwrap();
        std::fs::write(
            t.join("c1.jsonl"),
            jsonl(&[user_line("<user_query>why is the build red</user_query>")]),
        )
        .unwrap();

        let mut conn = crate::db::open(&root.join("index.db")).unwrap();
        let stores = AgentStores::discover(&home);
        let report = scan_all(&mut conn, None, &stores, &mut || {}).unwrap();
        assert_eq!((report.threads_indexed, report.errors), (1, 0));
        let (title, project, created, updated): (String, String, i64, i64) = conn
            .query_row(
                "SELECT title, project_path, created_at, updated_at FROM threads",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(title, "Red build, renamed");
        assert_eq!(project, "/Users/me/my app");
        assert_eq!((created, updated), (1781729743, 1781729749));
        let rows: Vec<(String, String)> = conn
            .prepare("SELECT role, text FROM messages ORDER BY seq")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let roles: Vec<&str> = rows.iter().map(|(r, _)| r.as_str()).collect();
        assert_eq!(
            roles,
            vec!["user", "assistant", "assistant", "tool", "assistant"]
        );
        assert_eq!(rows[0].1, "why is the build red");
        assert_eq!(rows[2].1, r#"Shell: {"command":"cargo test"}"#);
        assert!(rows[3].1.chars().count() <= super::super::claude::TOOL_TEXT_CAP_CHARS);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An index written before fingerprints carried a coverage mark: a settled composer the
    /// pass skips keeps its unmarked fingerprint, so coverage is unknown until state.vscdb
    /// is asked. The empty composer hands its id to the transcript, the one with text keeps
    /// it, and both fingerprints get their mark so the next pass asks nothing.
    #[test]
    fn unmarked_fingerprints_from_an_older_build_are_rechecked_once() {
        let root = temp_dir("oldfp");
        let (home, _) = agent_home(&root);
        let vscdb = root.join("state.vscdb");
        Connection::open(&vscdb)
            .unwrap()
            .execute_batch(
                "CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value BLOB);
                 INSERT INTO cursorDiskKV VALUES
                   ('composerData:n1', json('{\"name\":\"From the editor\",\"createdAt\":1,\"lastUpdatedAt\":1}')),
                   ('bubbleId:n1:1', json('{\"type\":1,\"text\":\"editor question\"}')),
                   ('composerData:f1', json('{\"name\":\"Empty shell\",\"createdAt\":1,\"lastUpdatedAt\":1}')),
                   ('bubbleId:f1:1', json('{\"type\":1,\"text\":\"  \"}'));",
            )
            .unwrap();
        let mut conn = crate::db::open(&root.join("index.db")).unwrap();
        scan_all(&mut conn, Some(&vscdb), &AgentStores::default(), &mut || {}).unwrap();
        // What the previous build stored: the same fingerprints with no mark.
        conn.execute(
            "UPDATE index_state SET hash = replace(replace(hash, ':text', ''), ':empty', '')
             WHERE path LIKE '%#composer:%'",
            [],
        )
        .unwrap();
        let fp = |conn: &Connection, id: &str| {
            stored_fingerprint(conn, &composer_key(&vscdb, id))
                .unwrap()
                .unwrap()
        };
        assert!(!fp(&conn, "f1").ends_with(EMPTY_MARK));
        assert!(cli_thread_in(&conn, Some(&vscdb), "f1").unwrap());
        assert!(!cli_thread_in(&conn, Some(&vscdb), "n1").unwrap());

        let stores = AgentStores::discover(&home);
        let report = scan_all(&mut conn, Some(&vscdb), &stores, &mut || {}).unwrap();
        assert_eq!(report.errors, 0);
        let title = |id: &str| -> String {
            conn.query_row(
                "SELECT title FROM threads WHERE external_id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(title("f1"), "cli question", "the transcript is indexed");
        assert_eq!(
            title("n1"),
            "From the editor",
            "the composer keeps its thread"
        );
        assert!(fp(&conn, "f1").ends_with(EMPTY_MARK));
        assert!(fp(&conn, "n1").ends_with(TEXT_MARK));
        let again = scan_all(&mut conn, Some(&vscdb), &stores, &mut || {}).unwrap();
        assert_eq!(again.threads_indexed, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A composer re-extracted after its transcript went away (the project folder renamed)
    /// keeps the project an earlier pass found instead of losing it to None.
    #[test]
    fn a_reextracted_composer_keeps_its_known_project() {
        let root = temp_dir("keepproj");
        let (home, project) = agent_home(&root);
        let vscdb = root.join("state.vscdb");
        let src = Connection::open(&vscdb).unwrap();
        src.execute_batch(
            "CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value BLOB);
             INSERT INTO cursorDiskKV VALUES
               ('composerData:n1', json('{\"name\":\"Editor\",\"createdAt\":1}')),
               ('bubbleId:n1:1', json('{\"type\":1,\"text\":\"first\"}'));",
        )
        .unwrap();
        let mut conn = crate::db::open(&root.join("index.db")).unwrap();
        scan_all(
            &mut conn,
            Some(&vscdb),
            &AgentStores::discover(&home),
            &mut || {},
        )
        .unwrap();
        src.execute(
            "INSERT INTO cursorDiskKV VALUES ('bubbleId:n1:2', json('{\"type\":2,\"text\":\"second\"}'))",
            [],
        )
        .unwrap();
        // The file's change key is (mtime in seconds, size): forget it, so a write within the
        // same second still counts as a change.
        conn.execute(
            "DELETE FROM index_state WHERE path NOT LIKE '%#composer:%'",
            [],
        )
        .unwrap();
        let report =
            scan_all(&mut conn, Some(&vscdb), &AgentStores::default(), &mut || {}).unwrap();
        assert_eq!(report.threads_indexed, 1, "re-extracted with no placement");
        let (path, n): (Option<String>, i64) = conn
            .query_row(
                "SELECT project_path, message_count FROM threads WHERE external_id = 'n1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(n, 2);
        assert_eq!(path, Some(project.to_string_lossy().into_owned()));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Windows tool paths (either separator, or a `file:///c:/` URI) recover the project of
    /// a Cursor directory name, with the drive written as one dash or two.
    #[test]
    fn windows_tool_paths_recover_the_project() {
        assert_eq!(
            project_from_paths("c-Users-me-my-app", [r"C:\Users\me\my-app\src\main.rs"]),
            Some(r"C:\Users\me\my-app".into())
        );
        assert_eq!(
            project_from_paths("C--Users-me-my-app", ["file:///c:/Users/me/my-app/x.rs"]),
            Some("c:/Users/me/my-app".into())
        );
        assert_eq!(
            project_from_paths("c-Users-me-other", [r"C:\Users\me\my-app\x.rs"]),
            None
        );
        let mut paths = Vec::new();
        tool_paths(
            &serde_json::json!({"path": r"D:\work\x.rs", "target_directory": "/u/w", "query": "C:\\no"}),
            &mut paths,
        );
        paths.sort();
        assert_eq!(paths, vec!["/u/w".to_string(), r"D:\work\x.rs".to_string()]);
    }

    /// Fill `store` as a one-message CLI chat store.
    fn write_chat_store(store: &Connection, text: &str) {
        store
            .execute_batch(
                "CREATE TABLE blobs (id TEXT PRIMARY KEY, data BLOB);
                 CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);",
            )
            .unwrap();
        let id = [7u8; 32];
        let msg = serde_json::json!({"role":"user","content": text});
        store
            .execute(
                "INSERT INTO blobs VALUES (?1, ?2)",
                params![hex_encode(&id), msg.to_string().into_bytes()],
            )
            .unwrap();
        let mut root_blob = Vec::new();
        pb_field(&mut root_blob, 1, &id);
        store
            .execute("INSERT INTO blobs VALUES ('root', ?1)", [root_blob])
            .unwrap();
        let meta = serde_json::json!({"latestRootBlobId":"root","name":"From the store"});
        store
            .execute(
                "INSERT INTO meta VALUES ('0', ?1)",
                [hex_encode(meta.to_string().as_bytes())],
            )
            .unwrap();
    }

    /// A store in a layout the parser doesn't know reads as empty, not as an error, and
    /// leaves the session to its transcript; the same files pick the same winner next pass.
    #[test]
    fn a_store_that_yields_nothing_leaves_its_transcript_in_charge() {
        let root = temp_dir("emptystore");
        let home = root.join(".cursor");
        let chat_dir = home.join("chats").join("ab12").join("c1");
        std::fs::create_dir_all(&chat_dir).unwrap();
        Connection::open(chat_dir.join("store.db"))
            .unwrap()
            .execute_batch("CREATE TABLE records (k TEXT, v BLOB)")
            .unwrap();
        let t = home
            .join("projects")
            .join("Users-me-my-app")
            .join("agent-transcripts");
        std::fs::create_dir_all(&t).unwrap();
        std::fs::write(
            t.join("c1.jsonl"),
            jsonl(&[user_line("<user_query>from the transcript</user_query>")]),
        )
        .unwrap();
        let mut conn = crate::db::open(&root.join("index.db")).unwrap();
        let stores = AgentStores::discover(&home);
        let report = scan_all(&mut conn, None, &stores, &mut || {}).unwrap();
        assert_eq!((report.threads_indexed, report.errors), (1, 0));
        let title: String = conn
            .query_row("SELECT title FROM threads", [], |r| r.get(0))
            .unwrap();
        assert_eq!(title, "from the transcript");
        let again = scan_all(&mut conn, None, &stores, &mut || {}).unwrap();
        assert_eq!((again.threads_indexed, again.errors), (0, 0));

        // The store learns to parse (a fixed layout): it takes the session over.
        std::fs::remove_file(chat_dir.join("store.db")).unwrap();
        write_chat_store(
            &Connection::open(chat_dir.join("store.db")).unwrap(),
            "from the store",
        );
        let report = scan_all(&mut conn, None, &stores, &mut || {}).unwrap();
        assert_eq!((report.threads_indexed, report.errors), (1, 0));
        let title: String = conn
            .query_row("SELECT title FROM threads", [], |r| r.get(0))
            .unwrap();
        assert_eq!(title, "From the store");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A store whose tables are all still in its `-wal` reads as empty through an
    /// `immutable=1` open (which skips the WAL) without failing, and in full from a copy.
    #[test]
    fn a_wal_only_store_is_read_from_a_copy() {
        let root = temp_dir("walstore");
        let live = root.join("live");
        std::fs::create_dir_all(&live).unwrap();
        let writer = Connection::open(live.join("store.db")).unwrap();
        writer
            .execute_batch("PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0;")
            .unwrap();
        write_chat_store(&writer, "only in the wal");
        // Snapshot the files while the writer still holds them, so nothing is checkpointed.
        let store = root.join("store.db");
        std::fs::copy(live.join("store.db"), &store).unwrap();
        std::fs::copy(live.join("store.db-wal"), wal_path(&store)).unwrap();
        drop(writer);

        let imm = Connection::open_with_flags(
            format!("file:{}?immutable=1", store.display()),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .unwrap();
        assert!(parse_chat(&imm).unwrap().messages.is_empty());
        let copied = read_chat_copy(&store).unwrap();
        assert_eq!(copied.messages.len(), 1);
        assert_eq!(copied.messages[0].text, "only in the wal");
        assert!(
            !root.join("store.db-shm").exists(),
            "the copy is opened, never the store"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Real-data smoke test of the agent transcripts and CLI chat stores alone (skips the
    /// multi-GB state.vscdb). Run with
    /// `cargo test -p callimachus-core -- --ignored real_cursor_agents_index --nocapture`.
    #[test]
    #[ignore]
    fn real_cursor_agents_index() {
        let Some(home) = cursor_home() else {
            return;
        };
        let p = std::env::temp_dir().join(format!(
            "callimachus_cursor_agents_{}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&p);
        let mut conn = crate::db::open(&p).unwrap();
        let t0 = std::time::Instant::now();
        let stores = AgentStores::discover(&home);
        let found = (stores.transcripts.len(), stores.chats.len());
        let report = scan_all(&mut conn, None, &stores, &mut || {}).unwrap();
        eprintln!(
            "{found:?} transcripts/chats: {report:?} in {:?}",
            t0.elapsed()
        );
        let (threads, with_project, subagents): (i64, i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), COUNT(project_path), SUM(is_subagent) FROM threads",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        eprintln!("threads {threads}, with a project {with_project}, subagents {subagents}");
        let t1 = std::time::Instant::now();
        let again = scan_all(&mut conn, None, &AgentStores::discover(&home), &mut || {}).unwrap();
        eprintln!("unchanged rescan: {again:?} in {:?}", t1.elapsed());
        assert_eq!(again.threads_indexed, 0);
        let _ = std::fs::remove_file(&p);
    }

    /// Real-data smoke test. Run with `cargo test -- --ignored real_cursor_index --nocapture`.
    #[test]
    #[ignore]
    fn real_cursor_index() {
        let mut p = std::env::temp_dir();
        p.push(format!("callimachus_cursor_real_{}.db", std::process::id()));
        let mut conn = crate::db::open(&p).unwrap();
        let report = scan(&mut conn, &mut || {}).unwrap();
        eprintln!("{report:?}");
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        eprintln!("cursor messages indexed: {n}");
        let (threads, with_project, subagents): (i64, i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), COUNT(project_path), SUM(is_subagent) FROM threads",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        eprintln!("threads {threads}, with a project {with_project}, subagents {subagents}");
    }
}
