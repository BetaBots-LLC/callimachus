//! Agent Trace interop (https://agent-trace.dev, version 0.1, an RFC from January 2026):
//! read the trace records a repository carries so `cal why` and `threads_for_file` can name
//! the agent and conversation another tool recorded for a line, and write records for the
//! commits our sessions produced (`cal trace export`).
//!
//! The format, from the spec and its published schema
//! (https://agent-trace.dev/schemas/v1/trace-record.json): a record is `{version, id,
//! timestamp, vcs: {type, revision}, tool: {name, version}, files: [{path, conversations:
//! [{url?, contributor: {type, model_id?}, ranges: [{start_line, end_line}], related?}]}],
//! metadata?}`. Paths are relative to the repository root; ranges are 1-based, inclusive and
//! "reference positions at the recorded revision". The spec leaves storage to implementations;
//! its reference implementation appends JSON Lines to `.agent-trace/traces.jsonl` at the
//! workspace root, so that is where we read (every `*.jsonl` there) and write.
//!
//! Two readings of `vcs.revision` exist in the wild, and both are accepted: the spec's (the
//! commit the ranges describe), and the reference hook's, which records at edit time with
//! `git rev-parse HEAD`, the commit *before* the edit lands. A record whose revision is the
//! blamed commit's first parent is therefore attributed to it too, flagged
//! `recordedBeforeCommit`. Nothing here is stored: the records are read at query time, so no
//! migration and nothing to keep in sync.

use anyhow::Result;
use rusqlite::Connection;
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::cli_contract::CAL_SCHEMA_VERSION;
use crate::why::{BlameCommit, Picked, Target};

/// The spec version we write. The live schema's pattern is `^[0-9]+\.[0-9]+$`; the spec's
/// own examples say "0.1.0" and the reference writes "1.0", so readers accept any string.
pub const SPEC_VERSION: &str = "0.1";
/// Where records live, relative to the repository root (the reference implementation's
/// `TRACE_PATH`).
pub const TRACE_DIR: &str = ".agent-trace";
pub const TRACE_FILE: &str = "traces.jsonl";
/// What `from` says on every attribution, so a reader can tell it from a linked session.
pub const FROM_AGENT_TRACE: &str = "agent-trace";
/// Attributions shown per commit. The reference hook writes one record per edit, so a busy
/// session yields dozens; they are merged per conversation first.
pub const MAX_ATTRIBUTIONS_PER_COMMIT: usize = 5;
/// How much of `.agent-trace/` a query reads. `cal why` runs on every editor save through
/// the extension; a trace log past this is skipped rather than parsed on each keystroke.
const MAX_TRACE_BYTES: u64 = 64 << 20;
/// The most `git show` output an export reads for one commit. A commit that vendors a
/// lockfile or a data dump prints all of it; past this its files are named without ranges.
const MAX_DIFF_BYTES: usize = 8 << 20;
/// The most a `--name-only` file list may print before the commit is left out altogether.
const MAX_NAMES_BYTES: usize = 1 << 20;

/// One conversation a trace record attributes lines of a blamed commit to.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TraceAttribution {
    /// Always `agent-trace`: read from a trace record in the repository, not inferred from
    /// a transcript in the index.
    pub from: String,
    /// The agent the record names (`metadata.agent`, else `tool.name`), e.g. `cursor`.
    pub agent: Option<String>,
    /// `human`, `ai`, `mixed` or `unknown`, as the record says.
    pub contributor: String,
    /// The model, `provider/model` per models.dev, when the record names one.
    pub model: Option<String>,
    /// The conversation id the record carries (`metadata.conversation_id`, else
    /// `metadata.session_id`).
    pub conversation_id: Option<String>,
    /// The record's link to the conversation, often a local `file://` transcript path.
    pub conversation_url: Option<String>,
    /// The indexed session with that conversation id, when there is one.
    pub thread_id: Option<i64>,
    /// The lines the record attributes in this file, inclusive `[start, end]` runs at the
    /// recorded revision (not today's line numbers); empty when the record names the file
    /// without ranges.
    pub ranges: Vec<[u32; 2]>,
    /// The record's `vcs.revision`.
    pub revision: String,
    /// The revision is the blamed commit's parent: the record was written while the change
    /// was being made (the reference hook's habit), before it was committed.
    pub recorded_before_commit: bool,
    /// The newest `timestamp` among the merged records (RFC 3339).
    pub recorded_at: Option<String>,
    /// The file the record came from, relative to the repository root.
    pub trace_file: String,
}

/// A repository's trace records for one file, ready to match against blamed commits.
#[derive(Debug, Default)]
pub struct FileTrace {
    /// The file, relative to the repo root: blame names it so at each commit it owns lines of
    /// unless it was renamed since.
    rel: String,
    entries: Vec<TraceAttribution>,
    /// Blamed commit -> its first parent, for records written before the commit.
    parents: HashMap<String, String>,
    /// Blamed commit -> `(position at that commit, path there)` of each line it owns, from
    /// [`FileTrace::with_blame`]. Without it, ranges are compared with today's line
    /// numbers, which only agree until a later commit moves the lines.
    origins: Option<HashMap<String, Vec<(u32, String)>>>,
}

/// The records under `<repo>/.agent-trace/` that name `target`'s file, or None when the repo
/// has none (the common case: one `read_dir` that fails). Never an error: a trace log is a
/// bonus, and a malformed one must not break `cal why`.
pub fn load_for_file(
    target: &Target,
    blamed: &[BlameCommit],
    deadline: Instant,
) -> Option<FileTrace> {
    let repo = Path::new(&target.repo);
    let mut entries = Vec::new();
    for (text, rel_name) in trace_logs(repo, deadline) {
        entries.extend(entries_in(&text, &target.repo, &target.rel, &rel_name));
    }
    if entries.is_empty() {
        return None;
    }
    let shas: Vec<&str> = blamed
        .iter()
        .map(|c| c.sha.as_str())
        .filter(|s| !crate::why::is_uncommitted(s))
        .collect();
    // Always asked, even when every revision is itself blamed: a record written at edit time
    // names the parent, which a whole-file blame often lists too (it owns other lines).
    let parents = if shas.is_empty() {
        HashMap::new()
    } else {
        first_parents(repo, &shas, deadline)
    };
    Some(FileTrace {
        rel: target.rel.clone(),
        entries,
        parents,
        origins: None,
    })
}

/// The text of each `*.jsonl` log under `<repo>/.agent-trace/`, sorted by name, with its
/// repo-relative name, [`MAX_TRACE_BYTES`] in all and read before `deadline`. The directory
/// comes with whatever repository was cloned, so nothing in it is followed: a symlinked
/// `.agent-trace`, a symlinked log (to `/dev/zero`, a FIFO or a huge file elsewhere) and
/// anything but a regular file are skipped, and a log that grows past what is left of the
/// budget while it is read is dropped.
fn trace_logs(repo: &Path, deadline: Instant) -> Vec<(String, String)> {
    let dir = repo.join(TRACE_DIR);
    if !std::fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_dir()) {
        return Vec::new();
    }
    let Ok(listing) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = listing
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    files.sort();
    let mut budget = MAX_TRACE_BYTES;
    let mut out = Vec::new();
    for path in files {
        if Instant::now() >= deadline {
            break;
        }
        let Ok(bytes) = read_regular(&path, budget, deadline) else {
            continue;
        };
        budget -= bytes.len() as u64;
        let name = path
            .file_name()
            .map(|n| format!("{TRACE_DIR}/{}", n.to_string_lossy()))
            .unwrap_or_default();
        out.push((String::from_utf8_lossy(&bytes).into_owned(), name));
    }
    out
}

/// `O_NOFOLLOW` from `<fcntl.h>`, where its value is known (it differs by architecture on
/// Linux); elsewhere [`open_regular`]'s before-and-after check stands alone.
#[cfg(target_os = "macos")]
const O_NOFOLLOW: Option<i32> = Some(0x0100);
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const O_NOFOLLOW: Option<i32> = Some(0o400_000);
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const O_NOFOLLOW: Option<i32> = Some(0o100_000);
#[cfg(all(
    unix,
    not(target_os = "macos"),
    not(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))
))]
const O_NOFOLLOW: Option<i32> = None;

/// Open `path` only when it is a regular file and not a symlink: checked before opening,
/// opened with `O_NOFOLLOW`, then checked again on the open handle (same device and inode),
/// so a link swapped in between is refused too.
fn open_regular(path: &Path, opts: &mut std::fs::OpenOptions) -> std::io::Result<std::fs::File> {
    let refuse = |why: &str| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} {why}", path.display()),
        )
    };
    let before = std::fs::symlink_metadata(path)?;
    if before.file_type().is_symlink() {
        return Err(refuse("is a symlink; refusing to follow it"));
    }
    if !before.file_type().is_file() {
        return Err(refuse("is not a regular file"));
    }
    #[cfg(unix)]
    if let Some(flag) = O_NOFOLLOW {
        std::os::unix::fs::OpenOptionsExt::custom_flags(opts, flag);
    }
    let file = opts.open(path)?;
    let after = file.metadata()?;
    if !after.is_file() || !same_file(&before, &after) {
        return Err(refuse("changed while it was opened"));
    }
    Ok(file)
}

#[cfg(unix)]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

#[cfg(not(unix))]
fn same_file(_: &std::fs::Metadata, _: &std::fs::Metadata) -> bool {
    true
}

/// A regular file's bytes, refused when it holds more than `cap` (read through
/// `take(cap + 1)`, so a file that grows while it is read cannot run past the cap) or when
/// `deadline` passes first.
fn read_regular(path: &Path, cap: u64, deadline: Instant) -> std::io::Result<Vec<u8>> {
    let file = open_regular(path, std::fs::OpenOptions::new().read(true))?;
    read_capped(file, cap, deadline)
}

fn read_capped(file: std::fs::File, cap: u64, deadline: Instant) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let too_big = || std::io::Error::new(std::io::ErrorKind::InvalidData, "past the size cap");
    if file.metadata()?.len() > cap {
        return Err(too_big());
    }
    let mut bytes = Vec::new();
    let mut reader = file.take(cap.saturating_add(1));
    let mut chunk = vec![0u8; 1 << 20];
    loop {
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "out of time",
            ));
        }
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..n]);
        if bytes.len() as u64 > cap {
            return Err(too_big());
        }
    }
    Ok(bytes)
}

/// The attributions in a JSON Lines trace log for the file `rel` of `repo`. A line that does
/// not even contain the file's name is skipped unparsed: a log covers the whole repository.
pub(crate) fn entries_in(
    text: &str,
    repo: &str,
    rel: &str,
    trace_file: &str,
) -> Vec<TraceAttribution> {
    let base = rel.rsplit('/').next().unwrap_or(rel);
    let mut out = Vec::new();
    for line in text.lines() {
        if base.is_empty() || !line.contains(base) {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        out.extend(record_entries(&record, repo, rel, trace_file));
    }
    out
}

/// One record's attributions for `rel`. Only git revisions are matched: blame names git
/// commits, and a jj change id or hg changeset cannot be compared with one.
fn record_entries(
    record: &Value,
    repo: &str,
    rel: &str,
    trace_file: &str,
) -> Vec<TraceAttribution> {
    let vcs = &record["vcs"];
    if vcs["type"].as_str() != Some("git") {
        return Vec::new();
    }
    let Some(revision) = vcs["revision"]
        .as_str()
        .map(|r| r.trim().to_ascii_lowercase())
        .filter(|r| r.len() >= 7 && r.bytes().all(|b| b.is_ascii_hexdigit()))
    else {
        return Vec::new();
    };
    let meta = &record["metadata"];
    let text = |v: &Value| {
        v.as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    let agent = text(&meta["agent"]).or_else(|| text(&record["tool"]["name"]));
    let conversation_id = text(&meta["conversation_id"]).or_else(|| text(&meta["session_id"]));
    let recorded_at = text(&record["timestamp"]);
    let mut out = Vec::new();
    for file in record["files"].as_array().into_iter().flatten() {
        if file["path"]
            .as_str()
            .map(|p| repo_relative(p, repo))
            .as_deref()
            != Some(rel)
        {
            continue;
        }
        for conv in file["conversations"].as_array().into_iter().flatten() {
            let ranges_json = conv["ranges"].as_array();
            let ranges = ranges_json
                .into_iter()
                .flatten()
                .filter_map(|r| {
                    let a = u32::try_from(r["start_line"].as_u64()?).ok()?;
                    let b = u32::try_from(r["end_line"].as_u64()?).ok()?;
                    (a >= 1 && b >= 1).then(|| [a.min(b), a.max(b)])
                })
                .collect::<Vec<_>>();
            // A per-range contributor overrides the conversation's (an agent handoff); the
            // first one stands for the conversation when it names none.
            let range_contributor = ranges_json
                .into_iter()
                .flatten()
                .find(|r| r["contributor"].is_object())
                .map(|r| &r["contributor"]);
            let contributor = if conv["contributor"].is_object() {
                &conv["contributor"]
            } else {
                range_contributor.unwrap_or(&Value::Null)
            };
            let model = text(&contributor["model_id"])
                .or_else(|| range_contributor.and_then(|c| text(&c["model_id"])));
            out.push(TraceAttribution {
                from: FROM_AGENT_TRACE.into(),
                agent: agent.clone(),
                contributor: text(&contributor["type"]).unwrap_or_else(|| "unknown".into()),
                model,
                conversation_id: conversation_id.clone(),
                conversation_url: text(&conv["url"]),
                thread_id: None,
                ranges: coalesce(ranges),
                revision: revision.clone(),
                recorded_before_commit: false,
                recorded_at: recorded_at.clone(),
                trace_file: trace_file.to_string(),
            });
        }
    }
    out
}

/// A record's `path` as `/`-separated and relative to the repo root. The reference keeps a
/// path outside its root absolute, and Windows writers use backslashes.
fn repo_relative(path: &str, repo: &str) -> String {
    let p = path.trim().replace('\\', "/");
    let repo = repo.replace('\\', "/");
    let repo = repo.trim_end_matches('/');
    let p = match p.strip_prefix(repo).and_then(|r| r.strip_prefix('/')) {
        Some(inner) if !repo.is_empty() => inner.to_string(),
        _ => p,
    };
    p.trim_start_matches("./").to_string()
}

/// Sorted, merged inclusive runs.
fn coalesce(mut ranges: Vec<[u32; 2]>) -> Vec<[u32; 2]> {
    ranges.sort_unstable();
    let mut out: Vec<[u32; 2]> = Vec::with_capacity(ranges.len());
    for r in ranges {
        match out.last_mut() {
            Some(last) if r[0] <= last[1].saturating_add(1) => last[1] = last[1].max(r[1]),
            _ => out.push(r),
        }
    }
    out
}

/// A full sha against a record's revision, which may be abbreviated.
fn revision_is(sha: &str, revision: &str) -> bool {
    revision.len() >= 7
        && sha.len() >= revision.len()
        && sha[..revision.len()].eq_ignore_ascii_case(revision)
}

/// Each commit's first parent, in one `git rev-list` over stdin (a whole-file blame can name
/// hundreds of commits, more than a Windows command line holds).
fn first_parents(repo: &Path, shas: &[&str], deadline: Instant) -> HashMap<String, String> {
    let input = shas.join("\n") + "\n";
    let Ok(out) = crate::why::run_git_input(
        repo,
        &["rev-list", "--no-walk=unsorted", "--parents", "--stdin"],
        Some(input.as_bytes()),
        deadline,
    ) else {
        return HashMap::new();
    };
    out.lines()
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            Some((parts.next()?.to_string(), parts.next()?.to_string()))
        })
        .collect()
}

/// What makes two attributions one: the same agent, model, reading of the revision and
/// contributor, and the same conversation (its id, else its url).
type MergeKey = (
    Option<String>,
    Option<String>,
    bool,
    String,
    Result<String, Option<String>>,
);

impl FileTrace {
    /// The trace of one log's text, as [`load_for_file`] builds it (for tests elsewhere in
    /// the crate, which have no repository to read).
    #[cfg(test)]
    pub(crate) fn from_log_for_tests(log: &str, target: &Target) -> Self {
        Self {
            rel: target.rel.clone(),
            entries: entries_in(log, &target.repo, &target.rel, "t"),
            ..Default::default()
        }
    }

    /// Match records against blame's own positions: `porcelain` is the `git blame
    /// --porcelain` output the blamed commits came from.
    pub fn with_blame(mut self, porcelain: &str) -> Self {
        self.origins = Some(crate::why::blame_origins(porcelain));
        self
    }

    /// The attributions for blamed commit `c`: records at `c` (or written just before it, at
    /// its parent) whose ranges hold the asked line, or for a whole file any of the lines
    /// `c` owns. A record without ranges names the whole file. Ranges are positions at the
    /// recorded revision, so with [`Self::with_blame`] they are compared with each line's
    /// position at `c` (blame's `<orig>`); a record at the parent was written at edit time,
    /// when its positions were already the commit's. Merged per agent, model and
    /// conversation, all of them: [`link_threads`] drops duplicates of linked sessions
    /// before the list is cut.
    pub fn for_commit(&self, c: &BlameCommit, line: Option<u32>) -> Vec<TraceAttribution> {
        let parent = self.parents.get(&c.sha);
        // Where `c`'s lines were when it was committed, under the path the records name.
        let at_commit: Option<Vec<u32>> = self.origins.as_ref().map(|o| {
            let mut v: Vec<u32> = o
                .get(&c.sha)
                .into_iter()
                .flatten()
                .filter(|(_, path)| *path == self.rel)
                .map(|(n, _)| *n)
                .collect();
            v.sort_unstable();
            v.dedup();
            v
        });
        let mut merged: Vec<TraceAttribution> = Vec::new();
        let mut at: HashMap<MergeKey, usize> = HashMap::new();
        for e in &self.entries {
            let direct = revision_is(&c.sha, &e.revision);
            let before = !direct && parent.is_some_and(|p| revision_is(p, &e.revision));
            if !direct && !before {
                continue;
            }
            let hit = match &at_commit {
                // A line from a path the record does not name (the file was renamed since)
                // is not the record's.
                Some(lines) => {
                    !lines.is_empty()
                        && (e.ranges.is_empty() || e.ranges.iter().any(|r| owns_any(lines, *r)))
                }
                None => {
                    e.ranges.is_empty()
                        || match line {
                            Some(n) => e.ranges.iter().any(|r| r[0] <= n && n <= r[1]),
                            None => e.ranges.iter().any(|r| owns_any(&c.line_numbers, *r)),
                        }
                }
            };
            if !hit {
                continue;
            }
            let key: MergeKey = (
                e.agent.clone(),
                e.model.clone(),
                before,
                e.contributor.clone(),
                e.conversation_id
                    .clone()
                    .ok_or_else(|| e.conversation_url.clone()),
            );
            match at.get(&key) {
                Some(&i) => {
                    let m = &mut merged[i];
                    // Coalesced once at the end: per entry it would sort the run each time.
                    m.ranges.extend_from_slice(&e.ranges);
                    if e.recorded_at > m.recorded_at {
                        m.recorded_at = e.recorded_at.clone();
                    }
                    if m.conversation_url.is_none() {
                        m.conversation_url = e.conversation_url.clone();
                    }
                }
                None => {
                    at.insert(key, merged.len());
                    merged.push(TraceAttribution {
                        recorded_before_commit: before,
                        ..e.clone()
                    });
                }
            }
        }
        for m in &mut merged {
            m.ranges = coalesce(std::mem::take(&mut m.ranges));
        }
        // The spec's reading first, then the newest record.
        merged.sort_by(|a, b| {
            a.recorded_before_commit
                .cmp(&b.recorded_before_commit)
                .then(b.recorded_at.cmp(&a.recorded_at))
        });
        merged
    }
}

/// Whether the sorted `lines` hold any line of `r`.
fn owns_any(lines: &[u32], r: [u32; 2]) -> bool {
    let i = lines.partition_point(|&n| n < r[0]);
    lines.get(i).is_some_and(|&n| n <= r[1])
}

/// Threads whose external id is a conversation id, or ends in `/<id>.jsonl`, `/<id>.json` or
/// `/<id>` (`?3` long). Top-level sessions first.
pub(crate) const LINK_THREADS_SQL: &str = "SELECT id, external_id FROM threads
     WHERE external_id IN (SELECT value FROM json_each(?1))
        OR substr(external_id, -?3) IN (SELECT value FROM json_each(?2))
     ORDER BY is_subagent, id";

/// The external-id endings a conversation id can stand for: the agent's session id is the
/// last segment of the stored id (a Claude Code `<slug>/<uuid>.jsonl`, a Gemini
/// `<hash>/chats/<id>.jsonl`, a Cline `<host>/<task>`). The bare `/<id>` form only for an id
/// long enough not to end some unrelated id by chance.
fn id_suffixes(id: &str) -> Vec<String> {
    let mut out = vec![format!("/{id}.jsonl"), format!("/{id}.json")];
    if id.len() >= 8 {
        out.push(format!("/{id}"));
    }
    out
}

/// Map each attribution's conversation id to the indexed session that has it, drop an
/// attribution whose session is linked to the commit at any rank (our own exported records
/// read back, or another tool tracing a session we indexed), then keep the first
/// [`MAX_ATTRIBUTIONS_PER_COMMIT`] per commit. One pass over `threads` per suffix length.
pub(crate) fn link_threads(conn: &Connection, picked: &mut [Picked]) -> Result<()> {
    let ids: Vec<String> = picked
        .iter()
        .flat_map(|p| p.2.iter().filter_map(|a| a.conversation_id.clone()))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let mut found: HashMap<String, i64> = HashMap::new();
    if !ids.is_empty() {
        let by_stem: Vec<String> = ids.iter().flat_map(|id| id_suffixes(id)).collect();
        let mut stmt = conn.prepare(LINK_THREADS_SQL)?;
        // One pass per suffix length (uuids share one): `substr(x, -n)` is x's last n chars.
        let mut lengths: Vec<usize> = by_stem.iter().map(String::len).collect();
        lengths.sort_unstable();
        lengths.dedup();
        let (ids_json, stems_json) = (
            serde_json::to_string(&ids)?,
            serde_json::to_string(&by_stem)?,
        );
        for len in lengths {
            let rows = stmt
                .query_map(rusqlite::params![ids_json, stems_json, len as i64], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                })?;
            for row in rows {
                let (tid, ext) = row?;
                let key = ids.iter().find(|id| {
                    ext == **id || id_suffixes(id).iter().any(|s| ext.ends_with(s.as_str()))
                });
                if let Some(key) = key {
                    found.entry(key.clone()).or_insert(tid);
                }
            }
        }
    }
    for (_, threads, trace) in picked.iter_mut() {
        for a in trace.iter_mut() {
            a.thread_id = a
                .conversation_id
                .as_ref()
                .and_then(|id| found.get(id).copied());
        }
        trace.retain(|a| {
            !a.thread_id
                .is_some_and(|t| threads.iter().any(|w| w.id == t))
        });
        trace.truncate(MAX_ATTRIBUTIONS_PER_COMMIT);
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Export: `cal trace export`.
// ---------------------------------------------------------------------------------------

/// `cal trace export --json`: what was (or, with `--dry-run`, would be) written.
#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TraceExport {
    /// Major version of this payload's shape. A reader refuses a version newer than it knows.
    #[serde(rename = "schema_version")]
    pub schema_version: u32,
    /// Always `agent-trace`: the record format written.
    pub standard: String,
    /// The Agent Trace version the records carry.
    pub spec_version: String,
    /// The repository the commits were read from.
    pub repo: String,
    /// The project key whose sessions were exported.
    pub project: String,
    /// Where the records go: a file, or `-` for stdout.
    pub out: String,
    pub dry_run: bool,
    /// Only commits from this epoch second on (`--since DAYS`); null for all time.
    pub since: Option<i64>,
    /// Records built: one per session and linked commit whose files the session edited.
    pub records: usize,
    /// Records appended to `out` (0 on a dry run or to stdout).
    pub written: usize,
    /// Records `out` already had (same id), left as they were.
    pub already_present: usize,
    pub sessions: usize,
    pub commits: usize,
    /// File entries across the records.
    pub files: usize,
    /// Session-commit links skipped because the session edited none of the commit's files
    /// (linked through mentions alone, or indexed before file edits were recorded).
    pub skipped_links: usize,
    /// Linked commits the repository no longer has (rebased away, or another clone's).
    pub missing_commits: usize,
    /// Commits whose diff was past the 8 MiB bound (a vendored lockfile or data dump): their
    /// records name the files without line ranges.
    pub oversized_commits: usize,
}

/// A file a commit changed, with the lines it added or rewrote there.
type FileLines = (String, Vec<[u32; 2]>);

/// Records built for a project, ready to write.
#[derive(Debug, Default)]
pub struct Built {
    /// `(record id, one JSON line)`, oldest commit first.
    pub records: Vec<(String, String)>,
    pub sessions: usize,
    pub commits: usize,
    pub files: usize,
    pub skipped_links: usize,
    /// Distinct linked commits the repository does not have.
    pub missing_commits: usize,
    /// Distinct commits whose files were named without ranges, their diff being too large.
    pub oversized_commits: usize,
}

/// The session-commit links of a project, oldest commit first. `COALESCE(project_key,
/// project_path)` is the project scope `gitlink::commit_timeline` uses.
pub(crate) const EXPORT_LINKS_SQL: &str =
    "SELECT tc.thread_id, tc.sha, tc.committed_at, tc.overlap, t.external_id, s.kind
     FROM thread_commits tc
     JOIN threads t ON t.id = tc.thread_id
     JOIN sources s ON s.id = t.source_id
     WHERE COALESCE(t.project_key, t.project_path) = ?1 AND tc.committed_at >= ?2
     ORDER BY tc.committed_at, tc.sha, tc.thread_id";

/// The files a thread changed, every spelling as stored.
pub(crate) const EDITED_PATHS_SQL: &str =
    "SELECT DISTINCT path FROM file_edits WHERE thread_id = ?1 AND op IN ('edit', 'write')";

/// A thread's most used model. A probe of one thread's rows (an index led by thread_id), not
/// a walk of the partial `idx_messages_model` over every model in the index: the unary `+`
/// keeps the planner off it.
pub(crate) const THREAD_MODEL_SQL: &str = "SELECT model FROM messages
     WHERE thread_id = ?1 AND +model IS NOT NULL AND model NOT LIKE '<%'
     GROUP BY model ORDER BY COUNT(*) DESC, model LIMIT 1";

/// Build one Agent Trace record per (session, linked commit) in `project` whose commit
/// changed files the session edited or wrote, read from `repo`. Read-only on the index.
pub fn build_records(
    conn: &Connection,
    repo: &str,
    project: &str,
    since: Option<i64>,
    deadline: Instant,
) -> Result<Built> {
    struct Link {
        thread_id: i64,
        sha: String,
        committed_at: i64,
        overlap: i64,
        external_id: String,
        kind: String,
    }
    let mut stmt = conn.prepare(EXPORT_LINKS_SQL)?;
    let links = stmt
        .query_map(rusqlite::params![project, since.unwrap_or(i64::MIN)], |r| {
            Ok(Link {
                thread_id: r.get(0)?,
                sha: r.get(1)?,
                committed_at: r.get(2)?,
                overlap: r.get(3)?,
                external_id: r.get(4)?,
                kind: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let checkouts = crate::why::project_checkouts(
        conn,
        &Target {
            repo: repo.to_string(),
            rel: String::new(),
        },
    );
    let version = env!("CALLIMACHUS_VERSION");
    let mut changes: HashMap<String, Option<Changes>> = HashMap::new();
    let mut edited: HashMap<i64, HashSet<String>> = HashMap::new();
    let mut models: HashMap<i64, Option<String>> = HashMap::new();
    let mut built = Built::default();
    let mut sessions: HashSet<i64> = HashSet::new();
    let mut commits: HashSet<String> = HashSet::new();
    let mut missing: HashSet<String> = HashSet::new();
    for link in links {
        if Instant::now() >= deadline {
            anyhow::bail!("git did not finish in time; try a shorter --since");
        }
        let files = changes
            .entry(link.sha.clone())
            .or_insert_with(|| commit_changes(Path::new(repo), &link.sha, deadline).ok());
        let Some(Changes { files, .. }) = files else {
            missing.insert(link.sha.clone());
            continue;
        };
        let paths = match edited.entry(link.thread_id) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(edited_paths(conn, link.thread_id)?)
            }
        };
        let mut entries = Vec::new();
        for (rel, ranges) in files.iter() {
            let forms = crate::why::path_forms(
                &Target {
                    repo: repo.to_string(),
                    rel: rel.clone(),
                },
                &checkouts,
            );
            if forms.iter().any(|f| paths.contains(f)) {
                entries.push((rel.clone(), ranges.clone()));
            }
        }
        if entries.is_empty() {
            built.skipped_links += 1;
            continue;
        }
        let model = match models.entry(link.thread_id) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(thread_model(conn, link.thread_id)?)
            }
        }
        .as_deref()
        .map(normalize_model_id);
        let id = record_id(&link.kind, &link.external_id, &link.sha);
        let record = record_json(&RecordInput {
            id: &id,
            sha: &link.sha,
            committed_at: link.committed_at,
            version,
            files: &entries,
            model: model.as_deref(),
            kind: &link.kind,
            conversation_id: &conversation_id(&link.external_id),
            overlap: link.overlap,
        });
        built.files += entries.len();
        sessions.insert(link.thread_id);
        commits.insert(link.sha.clone());
        built
            .records
            .push((id, crate::context::to_redacted_json(&record)?));
    }
    built.sessions = sessions.len();
    built.commits = commits.len();
    built.missing_commits = missing.len();
    built.oversized_commits = changes
        .values()
        .filter(|c| c.as_ref().is_some_and(|c| c.oversized))
        .count();
    Ok(built)
}

/// Every path the thread (with its subagents) edited or wrote, as stored.
fn edited_paths(conn: &Connection, thread_id: i64) -> Result<HashSet<String>> {
    let mut out = HashSet::new();
    let mut stmt = conn.prepare_cached(EDITED_PATHS_SQL)?;
    for tid in crate::indexer::session_thread_ids(conn, thread_id)? {
        for p in stmt.query_map([tid], |r| r.get::<_, String>(0))? {
            out.insert(p?);
        }
    }
    Ok(out)
}

fn thread_model(conn: &Connection, thread_id: i64) -> Result<Option<String>> {
    use rusqlite::OptionalExtension;
    Ok(conn
        .prepare_cached(THREAD_MODEL_SQL)?
        .query_row([thread_id], |r| r.get(0))
        .optional()?)
}

struct RecordInput<'a> {
    id: &'a str,
    sha: &'a str,
    committed_at: i64,
    version: &'a str,
    files: &'a [(String, Vec<[u32; 2]>)],
    model: Option<&'a str>,
    kind: &'a str,
    conversation_id: &'a str,
    overlap: i64,
}

/// One record, valid against the published schema: `tool` is required with a version there,
/// so it names us, the generator; the agent goes in `metadata.agent`. Nothing that names a
/// path on this machine, since the file is meant to be committed: no conversation `url`
/// (the only one we have is a local transcript path), and no stored external id (Claude
/// Code's encodes the project's absolute path in its directory name). The conversation id,
/// the agent's own session id, goes where the reference hook puts it.
fn record_json(r: &RecordInput<'_>) -> Value {
    let timestamp = chrono::DateTime::from_timestamp(r.committed_at, 0)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut contributor = json!({ "type": "ai" });
    if let Some(m) = r.model {
        contributor["model_id"] = m.chars().take(250).collect::<String>().into();
    }
    let files: Vec<Value> = r
        .files
        .iter()
        .map(|(path, ranges)| {
            let ranges: Vec<Value> = ranges
                .iter()
                .map(|[a, b]| json!({ "start_line": a, "end_line": b }))
                .collect();
            json!({
                "path": path,
                "conversations": [{ "contributor": contributor, "ranges": ranges }],
            })
        })
        .collect();
    json!({
        "version": SPEC_VERSION,
        "id": r.id,
        "timestamp": timestamp,
        "vcs": { "type": "git", "revision": r.sha },
        "tool": { "name": "callimachus", "version": r.version },
        "files": files,
        "metadata": {
            "conversation_id": r.conversation_id,
            "agent": agent_name(r.kind),
            "dev.callimachus": {
                "source": r.kind,
                // The session-commit link is inferred from the files they share, not
                // recorded at edit time; `overlap` is how many.
                "inferred": true,
                "overlap": r.overlap,
            },
        },
    })
}

/// The agent's own id for the session: the last segment of the stored external id, without
/// a transcript extension. Several agents store a transcript's path there (Claude Code's
/// `<slug>/<uuid>.jsonl`, whose slug is the project's absolute path; Gemini's
/// `<hash>/chats/<id>.jsonl`; an absolute path for a file outside the agent's root), and
/// only the last segment is the session id. [`link_threads`] maps it back.
fn conversation_id(external_id: &str) -> String {
    let last = external_id
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(external_id);
    last.strip_suffix(".jsonl")
        .or_else(|| last.strip_suffix(".json"))
        .filter(|s| !s.is_empty())
        .unwrap_or(last)
        .to_string()
}

/// The agent as the reference names tools (`claude-code`, `cursor`).
fn agent_name(kind: &str) -> String {
    kind.replace('_', "-")
}

/// `provider/model` per models.dev, the reference's `normalizeModelId`: a name that already
/// has a provider stays, the common families get theirs, anything else is kept as is.
pub(crate) fn normalize_model_id(model: &str) -> String {
    if model.contains('/') {
        return model.to_string();
    }
    let provider = [
        ("claude-", "anthropic"),
        ("gpt-", "openai"),
        ("o1", "openai"),
        ("o3", "openai"),
        ("o4", "openai"),
        ("codex-", "openai"),
        ("gemini-", "google"),
    ]
    .iter()
    .find(|(p, _)| model.starts_with(p))
    .map(|(_, v)| *v);
    match provider {
        Some(p) => format!("{p}/{model}"),
        None => model.to_string(),
    }
}

/// A stable UUID for (source, session, commit), so a second export finds its own records
/// and appends nothing. Version 8 (RFC 9562, custom) over two FNV-1a 64 hashes: std's
/// hasher is not stable across Rust releases, and the uuid crate's v5 needs another crate.
pub(crate) fn record_id(kind: &str, external_id: &str, sha: &str) -> String {
    let key = format!("callimachus-agent-trace\0{kind}\0{external_id}\0{sha}");
    let fnv = |basis: u64| {
        key.bytes().fold(basis, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
        })
    };
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&fnv(0xcbf2_9ce4_8422_2325).to_be_bytes());
    bytes[8..].copy_from_slice(&fnv(0x6c62_272e_07bb_0142).to_be_bytes());
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}

/// What a commit changed, for export: each file with the lines it added or rewrote.
struct Changes {
    files: Vec<FileLines>,
    /// The diff was past [`MAX_DIFF_BYTES`]: the files are named without ranges.
    oversized: bool,
}

/// The files a commit changed (against its first parent for a merge) with the lines it
/// added or rewrote, at that commit, as `git show -U0` hunks give them. A deleted file has
/// no lines at the commit and is left out; a binary change, a pure rename or a
/// deletion-only change keeps the file with no ranges (the spec's file-level form). Past
/// [`MAX_DIFF_BYTES`] of diff, git is stopped and the files come from `--name-only`, each
/// without ranges. `--no-textconv`: a textconv filter's ranges would describe its output,
/// not the file.
fn commit_changes(repo: &Path, sha: &str, deadline: Instant) -> Result<Changes> {
    commit_changes_within(repo, sha, deadline, MAX_DIFF_BYTES)
}

/// [`commit_changes`] with the diff bounded at `max_diff` bytes.
fn commit_changes_within(
    repo: &Path,
    sha: &str,
    deadline: Instant,
    max_diff: usize,
) -> Result<Changes> {
    let show = |extra: &[&'static str], cap: usize| {
        let mut args = vec![
            "-c",
            "core.quotepath=off",
            "show",
            "-m",
            "--first-parent",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--format=",
        ];
        args.extend_from_slice(extra);
        args.extend([sha, "--"]);
        crate::why::run_git_bounded(repo, &args, None, deadline, cap)
    };
    match show(&["--unified=0"], max_diff) {
        Ok(out) => Ok(Changes {
            files: parse_diff(&out),
            oversized: false,
        }),
        Err(e) if e.downcast_ref::<crate::why::GitOutputTooLarge>().is_some() => {
            // `-z`: one NUL-terminated name each, never quoted. Deleted files have no lines
            // at the commit, as above.
            let names = show(&["--name-only", "-z", "--diff-filter=d"], MAX_NAMES_BYTES)?;
            let mut files: Vec<FileLines> = names
                .split('\0')
                .filter(|n| !n.is_empty())
                .map(|n| (n.to_string(), Vec::new()))
                .collect();
            files.sort();
            files.dedup();
            Ok(Changes {
                files,
                oversized: true,
            })
        }
        Err(e) => Err(e),
    }
}

/// [`commit_changes`]'s parser over `git show -U0` output.
pub(crate) fn parse_diff(diff: &str) -> Vec<(String, Vec<[u32; 2]>)> {
    let mut files: BTreeMap<String, Vec<[u32; 2]>> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in diff.lines() {
        if let Some(header) = line.strip_prefix("diff --git ") {
            // `a/<old> b/<new>`; the `+++` line below, when there is one, is authoritative.
            current = header
                .rfind(" b/")
                .map(|i| unquote(&header[i + 3..]))
                .filter(|p| !p.is_empty());
            if let Some(p) = &current {
                files.entry(p.clone()).or_default();
            }
        } else if let Some(path) = line.strip_prefix("+++ ") {
            if path == "/dev/null" {
                if let Some(p) = current.take() {
                    files.remove(&p);
                }
            } else if let Some(p) = path.strip_prefix("b/").map(unquote) {
                if let Some(old) = current.replace(p.clone()) {
                    if old != p && files.get(&old).is_some_and(Vec::is_empty) {
                        files.remove(&old);
                    }
                }
                files.entry(p).or_default();
            }
        } else if let Some(hunk) = line.strip_prefix("@@ ") {
            let Some(p) = &current else { continue };
            // `-a[,b] +c[,d] @@`: lines c..c+d-1 at the commit; d = 0 is a deletion.
            let Some(new) = hunk.split(' ').find_map(|t| t.strip_prefix('+')) else {
                continue;
            };
            let (start, count) = match new.split_once(',') {
                Some((s, n)) => (s.parse::<u32>().ok(), n.parse::<u32>().ok()),
                None => (new.parse::<u32>().ok(), Some(1)),
            };
            if let (Some(s), Some(n)) = (start, count) {
                if n > 0 && s > 0 {
                    files.entry(p.clone()).or_default().push([s, s + n - 1]);
                }
            }
        }
    }
    files.into_iter().map(|(p, r)| (p, coalesce(r))).collect()
}

/// A path as git prints it, without the quotes it adds around unusual names (with
/// `core.quotepath=off` only control characters, quotes and backslashes are escaped).
fn unquote(p: &str) -> String {
    crate::why::unquote_path(p)
}

/// The default output: `<repo>/.agent-trace/traces.jsonl`.
pub fn default_out(repo: &str) -> PathBuf {
    Path::new(repo).join(TRACE_DIR).join(TRACE_FILE)
}

/// Append the records `path` does not already have (by `id`); with `dry_run`, only count.
/// Returns (appended, already there).
///
/// The default `path` is inside a repository someone else may have written, so nothing on
/// the way is followed: a symlinked `.agent-trace` directory or a symlinked (or otherwise
/// not regular) output file is refused rather than written through, the directory is made
/// with `create_dir` (one level, under a parent that must already be a real directory), a
/// new file is made with `create_new` (which fails on any existing name, links included)
/// and an existing one is opened without following links and checked again once open.
pub fn append_new(
    path: &Path,
    records: &[(String, String)],
    dry_run: bool,
) -> Result<(usize, usize)> {
    use std::io::{Seek, Write};
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let dir_exists = check_out_dir(dir)?;
    let existing = if dir_exists {
        match std::fs::symlink_metadata(path) {
            Ok(_) => Some(
                open_regular(path, std::fs::OpenOptions::new().read(true).append(true))
                    .map_err(|e| anyhow::anyhow!("not writing the trace: {e}"))?,
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        }
    } else {
        None
    };
    let mut file = existing;
    let text = match &file {
        Some(f) => {
            let no_deadline = Instant::now() + std::time::Duration::from_secs(3600);
            let bytes = read_capped(f.try_clone()?, MAX_TRACE_BYTES, no_deadline).map_err(|e| {
                anyhow::anyhow!(
                    "{} is too large to check for records it already has ({e}); \
                     write to another file with -o",
                    path.display()
                )
            })?;
            String::from_utf8_lossy(&bytes).into_owned()
        }
        None => String::new(),
    };
    let have: HashSet<String> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| v["id"].as_str().map(String::from))
        .collect();
    let fresh: Vec<&String> = records
        .iter()
        .filter(|(id, _)| !have.contains(id))
        .map(|(_, line)| line)
        .collect();
    let present = records.len() - fresh.len();
    if dry_run || fresh.is_empty() {
        return Ok((if dry_run { 0 } else { fresh.len() }, present));
    }
    if !dir_exists {
        std::fs::create_dir(dir)?;
    }
    let mut out = String::new();
    if !text.is_empty() && !text.ends_with('\n') {
        out.push('\n');
    }
    for line in &fresh {
        out.push_str(line);
        out.push('\n');
    }
    let mut f = match file.take() {
        Some(f) => f,
        None => std::fs::OpenOptions::new()
            .append(true)
            .create_new(true)
            .open(path)?,
    };
    f.seek(std::io::SeekFrom::End(0))?;
    f.write_all(out.as_bytes())?;
    Ok((fresh.len(), present))
}

/// Whether the output's directory exists. A `.agent-trace` that is a symlink is refused (a
/// cloned repository chooses where it points), as is anything there that is not a
/// directory; a missing one is made by the caller with `create_dir`, one level, so its
/// parent must already be a directory. Links elsewhere on the way are the user's own layout
/// (a repository under a linked folder, `-o /tmp/...` on macOS) and are followed.
fn check_out_dir(dir: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(dir) {
        Ok(m) if m.file_type().is_symlink() && dir.file_name().is_some_and(|n| n == TRACE_DIR) => {
            anyhow::bail!(
                "not writing the trace: {} is a symlink; refusing to follow it",
                dir.display()
            )
        }
        Ok(_) if dir.is_dir() => Ok(true),
        Ok(_) => anyhow::bail!(
            "not writing the trace: {} is not a directory",
            dir.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let parent = dir
                .parent()
                .filter(|d| !d.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            if !parent.is_dir() {
                anyhow::bail!("not writing the trace: {} does not exist", parent.display());
            }
            Ok(false)
        }
        Err(e) => Err(e.into()),
    }
}

impl TraceExport {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        repo: &str,
        project: &str,
        out: &str,
        dry_run: bool,
        since: Option<i64>,
        built: &Built,
        written: usize,
        already_present: usize,
    ) -> Self {
        Self {
            schema_version: CAL_SCHEMA_VERSION,
            standard: FROM_AGENT_TRACE.into(),
            spec_version: SPEC_VERSION.into(),
            repo: repo.to_string(),
            project: project.to_string(),
            out: out.to_string(),
            dry_run,
            since,
            records: built.records.len(),
            written,
            already_present,
            sessions: built.sessions,
            commits: built.commits,
            files: built.files,
            skipped_links: built.skipped_links,
            missing_commits: built.missing_commits,
            oversized_commits: built.oversized_commits,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::{source_id, upsert_thread, ParsedMessage, ParsedThread};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    static N: AtomicU32 = AtomicU32::new(0);

    fn temp_db() -> Connection {
        let p = std::env::temp_dir().join(format!(
            "calli_trace_{}_{}.db",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        crate::db::open(&p).unwrap()
    }

    const C: &str = "cccccccccccccccccccccccccccccccccccccccc";
    const P: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

    fn blamed(sha: &str, lines: &[u32]) -> BlameCommit {
        BlameCommit {
            sha: sha.into(),
            committed_at: 1,
            subject: "s".into(),
            lines: lines.len(),
            line_numbers: lines.to_vec(),
        }
    }

    /// The reference hook's record (Cursor `afterFileEdit`): revision = HEAD at edit time,
    /// a `file://` transcript url, the conversation id in metadata, and its pseudo-files.
    fn reference_record(revision: &str, path: &str, start: u32, end: u32) -> String {
        json!({
            "version": "1.0",
            "id": "550e8400-e29b-41d4-a716-446655440000",
            "timestamp": "2026-01-23T14:30:00Z",
            "vcs": { "type": "git", "revision": revision },
            "tool": { "name": "cursor", "version": "2.4.0" },
            "files": [
                { "path": path, "conversations": [{
                    "url": "file:///home/u/.cursor/t.jsonl",
                    "contributor": { "type": "ai", "model_id": "anthropic/claude-opus-4-5-20251101" },
                    "ranges": [{ "start_line": start, "end_line": end }]
                }]},
                { "path": ".shell-history", "conversations": [{ "ranges": [{ "start_line": 1, "end_line": 1 }] }] }
            ],
            "metadata": { "conversation_id": "conv-1", "generation_id": "g" }
        })
        .to_string()
    }

    #[test]
    fn records_are_read_for_the_file_only() {
        let log = [
            reference_record(C, "src/auth.rs", 10, 20),
            // Absolute (outside the reference's root logic) and Windows spellings.
            reference_record(C, "/repo/src/auth.rs", 30, 31),
            reference_record(C, "src\\auth.rs", 40, 40),
            reference_record(C, "src/other.rs", 1, 5),
            // A jj change id cannot be compared with a blamed git commit.
            reference_record(C, "src/auth.rs", 1, 1).replace("\"git\"", "\"jj\""),
            "not json, but mentions auth.rs".to_string(),
        ]
        .join("\n");
        let got = entries_in(&log, "/repo", "src/auth.rs", ".agent-trace/traces.jsonl");
        assert_eq!(got.len(), 3, "{got:?}");
        assert_eq!(got[0].ranges, vec![[10, 20]]);
        assert_eq!(got[1].ranges, vec![[30, 31]]);
        assert_eq!(got[2].ranges, vec![[40, 40]]);
        let a = &got[0];
        assert_eq!(a.from, FROM_AGENT_TRACE);
        assert_eq!(a.agent.as_deref(), Some("cursor"));
        assert_eq!(a.contributor, "ai");
        assert_eq!(
            a.model.as_deref(),
            Some("anthropic/claude-opus-4-5-20251101")
        );
        assert_eq!(a.conversation_id.as_deref(), Some("conv-1"));
        assert_eq!(
            a.conversation_url.as_deref(),
            Some("file:///home/u/.cursor/t.jsonl")
        );
        assert_eq!(a.revision, C);
        // Claude Code's hook puts the session id under `session_id`; a range contributor
        // stands in for a conversation without one.
        let claude = json!({
            "version": "1.0", "id": "x", "timestamp": "t",
            "vcs": { "type": "git", "revision": &C[..12] },
            "tool": { "name": "claude-code" },
            "files": [{ "path": "src/auth.rs", "conversations": [{
                "ranges": [{ "start_line": 5, "end_line": 2,
                             "contributor": { "type": "mixed", "model_id": "m" } }]
            }]}],
            "metadata": { "session_id": "sess-9" }
        })
        .to_string();
        let got = entries_in(&claude, "/repo", "src/auth.rs", "t");
        assert_eq!(got[0].conversation_id.as_deref(), Some("sess-9"));
        assert_eq!(got[0].contributor, "mixed");
        assert_eq!(got[0].model.as_deref(), Some("m"));
        assert_eq!(
            got[0].ranges,
            vec![[2, 5]],
            "a reversed range is still those lines"
        );
    }

    #[test]
    fn a_commit_matches_its_own_records_and_those_written_before_it() {
        let log = [
            reference_record(C, "src/auth.rs", 10, 20),
            reference_record(C, "src/auth.rs", 21, 25),
            reference_record(P, "src/auth.rs", 50, 60).replace("conv-1", "conv-2"),
            reference_record(P, "src/auth.rs", 90, 99).replace("conv-1", "conv-3"),
        ]
        .join("\n");
        let mut trace = FileTrace {
            rel: "src/auth.rs".into(),
            entries: entries_in(&log, "/repo", "src/auth.rs", "f"),
            ..Default::default()
        };
        trace.parents.insert(C.into(), P.into());
        let c = blamed(C, &[12, 13, 22, 55]);
        // One line: only the record whose range holds it.
        let at = trace.for_commit(&c, Some(12));
        assert_eq!(at.len(), 1);
        assert_eq!(at[0].conversation_id.as_deref(), Some("conv-1"));
        assert!(!at[0].recorded_before_commit);
        let at = trace.for_commit(&c, Some(55));
        assert_eq!(at[0].conversation_id.as_deref(), Some("conv-2"));
        assert!(at[0].recorded_before_commit);
        assert!(trace.for_commit(&c, Some(70)).is_empty());
        // The whole file: records over lines the commit owns, merged per conversation.
        let all = trace.for_commit(&c, None);
        let ids: Vec<_> = all
            .iter()
            .map(|a| a.conversation_id.clone().unwrap())
            .collect();
        assert_eq!(
            ids,
            vec!["conv-1", "conv-2"],
            "conv-3 covers no line of the commit"
        );
        assert_eq!(all[0].ranges, vec![[10, 25]]);
        // Another commit: nothing.
        assert!(trace
            .for_commit(&blamed(&"d".repeat(40), &[12]), Some(12))
            .is_empty());
    }

    #[test]
    fn diffs_give_each_files_lines_at_the_commit() {
        let diff = "diff --git a/src/a.rs b/src/a.rs\nindex 1..2 100644\n--- a/src/a.rs\n+++ b/src/a.rs\n\
                    @@ -3,0 +4,2 @@ fn x\n+a\n+b\n@@ -10 +12 @@\n-x\n+y\n@@ -20,3 +22,0 @@\n-gone\n\
                    diff --git a/old.rs b/new.rs\nsimilarity index 100%\nrename from old.rs\nrename to new.rs\n\
                    diff --git a/del.rs b/del.rs\ndeleted file mode 100644\n--- a/del.rs\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-a\n-b\n\
                    diff --git a/img.png b/img.png\nBinary files a/img.png and b/img.png differ\n\
                    diff --git a/my file.rs b/my file.rs\n--- a/my file.rs\n+++ b/my file.rs\n@@ -0,0 +1 @@\n+z\n";
        let got = parse_diff(diff);
        assert_eq!(
            got,
            vec![
                ("img.png".to_string(), vec![]),
                ("my file.rs".to_string(), vec![[1, 1]]),
                ("new.rs".to_string(), vec![]),
                ("src/a.rs".to_string(), vec![[4, 5], [12, 12]]),
            ]
        );
    }

    #[test]
    fn ids_are_stable_uuids_and_models_get_providers() {
        let a = record_id("claude_code", "slug/u.jsonl", C);
        assert_eq!(a, record_id("claude_code", "slug/u.jsonl", C));
        assert_ne!(a, record_id("claude_code", "slug/u.jsonl", P));
        let u = uuid::Uuid::parse_str(&a).unwrap();
        assert_eq!(u.get_version_num(), 8);
        assert_eq!(u.get_variant(), uuid::Variant::RFC4122);
        assert_eq!(
            normalize_model_id("claude-opus-4-5"),
            "anthropic/claude-opus-4-5"
        );
        assert_eq!(normalize_model_id("gpt-5-codex"), "openai/gpt-5-codex");
        assert_eq!(
            normalize_model_id("gemini-2.5-pro"),
            "google/gemini-2.5-pro"
        );
        assert_eq!(normalize_model_id("x-ai/grok"), "x-ai/grok");
        assert_eq!(normalize_model_id("composer-1"), "composer-1");
        assert_eq!(conversation_id("-Users-a-repo/abc-123.jsonl"), "abc-123");
        assert_eq!(conversation_id("hash/chats/session-x.jsonl"), "session-x");
        assert_eq!(conversation_id("Code/1780000000000"), "1780000000000");
        assert_eq!(conversation_id(r"C:\Users\a\t.json"), "t");
        assert_eq!(conversation_id("composer-abc"), "composer-abc");
    }

    fn thread(conn: &mut Connection, kind: &str, ext: &str) -> i64 {
        let sid = source_id(conn, kind).unwrap();
        upsert_thread(
            conn,
            sid,
            &ParsedThread {
                external_id: ext.into(),
                title: Some("t".into()),
                project_path: Some("/repo".into()),
                messages: vec![ParsedMessage {
                    role: "user".into(),
                    text: "work".into(),
                    tool_name: None,
                    ts: Some(1),
                }],
                ..Default::default()
            },
        )
        .unwrap();
        conn.query_row(
            "SELECT id FROM threads WHERE external_id = ?1",
            [ext],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn attribution(conv: &str) -> TraceAttribution {
        TraceAttribution {
            from: FROM_AGENT_TRACE.into(),
            agent: Some("claude-code".into()),
            contributor: "ai".into(),
            model: None,
            conversation_id: Some(conv.into()),
            conversation_url: None,
            thread_id: None,
            ranges: vec![],
            revision: C.into(),
            recorded_before_commit: false,
            recorded_at: None,
            trace_file: "f".into(),
        }
    }

    /// A Claude Code session id names the transcript `<slug>/<uuid>.jsonl`; an attribution
    /// to a session the commit already lists is dropped as a duplicate.
    #[test]
    fn conversations_map_to_indexed_sessions() {
        let mut conn = temp_db();
        let uuid = "0f6c1d9e-1111-4222-8333-444455556666";
        let tid = thread(&mut conn, "claude_code", &format!("-repo/{uuid}.jsonl"));
        let cursor = thread(&mut conn, "cursor", "composer-abc");
        let linked = thread(&mut conn, "codex", "rollout-1");
        let why_thread = |id| crate::why::WhyThread {
            id,
            title: None,
            source: "codex".into(),
            last_active: None,
            edits: 0,
            touch: None,
            overlap: 1,
            facts: vec![],
        };
        let mut picked: Vec<Picked> = vec![(
            blamed(C, &[1]),
            vec![why_thread(linked)],
            vec![
                attribution(uuid),
                attribution("composer-abc"),
                attribution("rollout-1"),
                attribution("never-indexed"),
            ],
        )];
        link_threads(&conn, &mut picked).unwrap();
        let got: Vec<(Option<&str>, Option<i64>)> = picked[0]
            .2
            .iter()
            .map(|a| (a.conversation_id.as_deref(), a.thread_id))
            .collect();
        assert_eq!(
            got,
            vec![
                (Some(uuid), Some(tid)),
                (Some("composer-abc"), Some(cursor)),
                (Some("never-indexed"), None),
            ]
        );
    }

    fn plan(conn: &Connection, sql: &str) -> String {
        let n = sql.matches('?').count();
        let params: Vec<String> = (0..n).map(|_| "1".to_string()).collect();
        conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map(rusqlite::params_from_iter(params.iter()), |r| {
                r.get::<_, String>(3)
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join(" | ")
    }

    /// The export's per-thread lookups are point probes on the big tables.
    #[test]
    fn export_lookups_use_their_indexes() {
        let conn = temp_db();
        let p = plan(&conn, THREAD_MODEL_SQL);
        // Either (thread_id, ...) index serves it: the plan probes one thread's rows.
        assert!(
            p.contains("SEARCH messages USING") && p.contains("(thread_id=?)"),
            "{p}"
        );
        assert!(
            !p.contains("SCAN messages") && !p.contains("idx_messages_model"),
            "{p}"
        );
        let p = plan(&conn, EDITED_PATHS_SQL);
        assert!(p.contains("idx_file_edits_thread (thread_id=?)"), "{p}");
        let p = plan(&conn, EXPORT_LINKS_SQL);
        assert!(
            !p.contains("SCAN messages") && !p.contains("SCAN file_edits"),
            "{p}"
        );
    }

    /// The trace record schema exactly as published at
    /// https://agent-trace.dev/schemas/v1/trace-record.json (fetched 2026-11-08).
    const PUBLISHED_SCHEMA: &str = r##"{
  "$ref": "#/definitions/TraceRecord",
  "definitions": {
    "TraceRecord": {
      "type": "object",
      "properties": {
        "version": {
          "type": "string",
          "pattern": "^[0-9]+\\.[0-9]+$",
          "description": "Agent Trace specification version (e.g., '1.0')"
        },
        "id": {
          "type": "string",
          "format": "uuid",
          "description": "Unique identifier for this trace record"
        },
        "timestamp": {
          "type": "string",
          "format": "date-time",
          "description": "RFC 3339 timestamp when trace was recorded"
        },
        "vcs": {
          "type": "object",
          "properties": {
            "type": {
              "type": "string",
              "enum": [
                "git",
                "jj",
                "hg",
                "svn"
              ],
              "description": "Version control system type (e.g., 'git', 'jj', 'hg')"
            },
            "revision": {
              "type": "string",
              "description": "Revision identifier (e.g., git commit SHA, jj change ID, hg changeset)"
            }
          },
          "required": [
            "type",
            "revision"
          ],
          "additionalProperties": false,
          "description": "Version control system information for this trace"
        },
        "tool": {
          "type": "object",
          "properties": {
            "name": {
              "type": "string",
              "description": "Name of the tool that produced the code"
            },
            "version": {
              "type": "string",
              "description": "Version of the tool"
            }
          },
          "required": [
            "name",
            "version"
          ],
          "additionalProperties": false,
          "description": "The tool that generated this trace"
        },
        "files": {
          "type": "array",
          "items": {
            "type": "object",
            "properties": {
              "path": {
                "type": "string",
                "description": "Relative file path from repository root"
              },
              "conversations": {
                "type": "array",
                "items": {
                  "type": "object",
                  "properties": {
                    "url": {
                      "type": "string",
                      "format": "uri",
                      "description": "URL to look up the conversation that produced this code"
                    },
                    "contributor": {
                      "type": "object",
                      "properties": {
                        "type": {
                          "type": "string",
                          "enum": [
                            "human",
                            "ai",
                            "mixed",
                            "unknown"
                          ],
                          "description": "The type of contributor"
                        },
                        "model_id": {
                          "type": "string",
                          "maxLength": 250,
                          "description": "The model's unique identifier following models.dev convention (e.g., 'anthropic/claude-opus-4-5-20251101')"
                        }
                      },
                      "required": [
                        "type"
                      ],
                      "additionalProperties": false,
                      "description": "The contributor for ranges in this conversation (can be overridden per-range)"
                    },
                    "ranges": {
                      "type": "array",
                      "items": {
                        "type": "object",
                        "properties": {
                          "start_line": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "1-indexed start line number"
                          },
                          "end_line": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "1-indexed end line number"
                          },
                          "content_hash": {
                            "type": "string",
                            "description": "Hash of attributed content for position-independent tracking"
                          },
                          "contributor": {
                            "type": "object",
                            "properties": {
                              "type": {
                                "type": "string",
                                "enum": [
                                  "human",
                                  "ai",
                                  "mixed",
                                  "unknown"
                                ],
                                "description": "The type of contributor"
                              },
                              "model_id": {
                                "type": "string",
                                "maxLength": 250,
                                "description": "The model's unique identifier following models.dev convention (e.g., 'anthropic/claude-opus-4-5-20251101')"
                              }
                            },
                            "required": [
                              "type"
                            ],
                            "additionalProperties": false,
                            "description": "Override contributor for this specific range (e.g., for agent handoffs)"
                          }
                        },
                        "required": [
                          "start_line",
                          "end_line"
                        ],
                        "additionalProperties": false
                      },
                      "description": "Array of line ranges produced by this conversation"
                    },
                    "related": {
                      "type": "array",
                      "items": {
                        "type": "object",
                        "properties": {
                          "type": {
                            "type": "string",
                            "description": "Type of related resource"
                          },
                          "url": {
                            "type": "string",
                            "format": "uri",
                            "description": "URL to the related resource"
                          }
                        },
                        "required": [
                          "type",
                          "url"
                        ],
                        "additionalProperties": false
                      },
                      "description": "Other related resources"
                    }
                  },
                  "required": [
                    "ranges"
                  ],
                  "additionalProperties": false
                },
                "description": "Array of conversations that contributed to this file"
              }
            },
            "required": [
              "path",
              "conversations"
            ],
            "additionalProperties": false
          },
          "description": "Array of files with attributed ranges"
        },
        "metadata": {
          "type": "object",
          "additionalProperties": {},
          "description": "Additional metadata for implementation-specific or vendor-specific data"
        }
      },
      "required": [
        "version",
        "id",
        "timestamp",
        "files"
      ],
      "additionalProperties": false
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "https://agent-trace.dev/schemas/v1/trace-record.json",
  "title": "Agent Trace Record"
}"##;

    /// [`PUBLISHED_SCHEMA`] with `definitions` renamed to `$defs`, the only `$ref` form
    /// `schema_check` resolves. It enforces `type`, `required`, `enum` and
    /// `additionalProperties: false`; the keywords it does not know (`pattern`, `format`,
    /// `maxLength`, `minimum`) are asserted by [`assert_published_constraints`].
    fn published_schema() -> Value {
        let text = PUBLISHED_SCHEMA
            .replace("\"#/definitions/", "\"#/$defs/")
            .replace("\"definitions\":", "\"$defs\":");
        serde_json::from_str(&text).unwrap()
    }

    /// The published constraints `schema_check` skips, each checked against `record`.
    fn assert_published_constraints(record: &Value) {
        let schema: Value = serde_json::from_str(PUBLISHED_SCHEMA).unwrap();
        let props = &schema["definitions"]["TraceRecord"]["properties"];
        assert_eq!(props["version"]["pattern"], "^[0-9]+\\.[0-9]+$");
        let version = record["version"].as_str().unwrap();
        let parts: Vec<&str> = version.split('.').collect();
        assert!(
            parts.len() == 2
                && parts
                    .iter()
                    .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())),
            "version {version} does not match the published pattern"
        );
        assert_eq!(props["id"]["format"], "uuid");
        let id = record["id"].as_str().unwrap();
        assert!(
            id.len() == 36 && uuid::Uuid::parse_str(id).is_ok(),
            "id {id} is not a hyphenated uuid"
        );
        assert_eq!(props["timestamp"]["format"], "date-time");
        chrono::DateTime::parse_from_rfc3339(record["timestamp"].as_str().unwrap()).unwrap();
        let conv = &props["files"]["items"]["properties"]["conversations"]["items"]["properties"];
        let max_model = conv["contributor"]["properties"]["model_id"]["maxLength"]
            .as_u64()
            .unwrap() as usize;
        let min_line = conv["ranges"]["items"]["properties"]["start_line"]["minimum"]
            .as_i64()
            .unwrap();
        for file in record["files"].as_array().unwrap() {
            for c in file["conversations"].as_array().unwrap() {
                if let Some(m) = c["contributor"]["model_id"].as_str() {
                    assert!(m.chars().count() <= max_model, "{m}");
                }
                for r in c["ranges"].as_array().unwrap() {
                    let (a, b) = (
                        r["start_line"].as_i64().unwrap(),
                        r["end_line"].as_i64().unwrap(),
                    );
                    assert!(a >= min_line && b >= a, "{r}");
                }
            }
        }
    }

    #[test]
    fn the_vendored_schema_rejects_what_the_published_one_does() {
        let schema = published_schema();
        let ok = json!({
            "version": "0.1", "id": "x", "timestamp": "t",
            "files": [{ "path": "a", "conversations": [{ "ranges": [] }] }]
        });
        crate::schema_check::validate(&schema, &ok).unwrap();
        let mut extra = ok.clone();
        extra["stray"] = json!(1);
        assert!(crate::schema_check::validate(&schema, &extra).is_err());
        let mut conv_extra = ok.clone();
        conv_extra["files"][0]["conversations"][0]["conversation_id"] = json!("c");
        assert!(crate::schema_check::validate(&schema, &conv_extra).is_err());
        let mut bad_tool = ok;
        bad_tool["tool"] = json!({ "name": "callimachus" });
        assert!(crate::schema_check::validate(&schema, &bad_tool).is_err());
    }

    /// Export from a real repo: one record per session and commit, valid against the
    /// published schema, appended once; then `cal why` reads it and another tool's record
    /// written before the commit, and attributes the lines.
    #[test]
    fn export_then_read_back_with_real_git() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let dir = std::env::temp_dir().join(format!(
            "calli_trace_repo_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .env("GIT_AUTHOR_DATE", "1700000000 +0000")
                .env("GIT_COMMITTER_DATE", "1700000000 +0000")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(dir.join("src/auth.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.join("README.md"), "hi\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "init"]);
        let parent = git(&["rev-parse", "HEAD"]);
        std::fs::write(dir.join("src/auth.rs"), "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();
        std::fs::write(dir.join("README.md"), "hi\nthere\n").unwrap();
        git(&["commit", "-q", "-am", "feat: b and c"]);
        let sha = git(&["rev-parse", "HEAD"]);
        let repo = git(&["rev-parse", "--show-toplevel"]);

        let mut conn = temp_db();
        let uuid = "0f6c1d9e-1111-4222-8333-444455556666";
        // Claude Code's real shape: the directory is the project's absolute path, encoded.
        let tid = thread(
            &mut conn,
            "claude_code",
            &format!("-Users-alice-work-repo/{uuid}.jsonl"),
        );
        conn.execute(
            "UPDATE threads SET project_path = ?1, project_key = ?1 WHERE id = ?2",
            rusqlite::params![repo, tid],
        )
        .unwrap();
        // Two sessions linked to a commit this checkout never had: one missing commit.
        let other = thread(&mut conn, "codex", "rollout-2");
        conn.execute(
            "UPDATE threads SET project_path = ?1, project_key = ?1 WHERE id = ?2",
            rusqlite::params![repo, other],
        )
        .unwrap();
        for t in [tid, other] {
            conn.execute(
                "INSERT INTO thread_commits (thread_id, sha, short_sha, subject, committed_at, overlap, created_at)
                 VALUES (?1, ?2, 'x', 'gone', 1600000000, 1, 1)",
                rusqlite::params![t, "f".repeat(40)],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO thread_commits (thread_id, sha, short_sha, subject, committed_at, overlap, created_at)
             VALUES (?1, ?2, 'x', 'feat', 1700000000, 2, 1)",
            rusqlite::params![tid, sha],
        )
        .unwrap();
        // It edited auth.rs and only read the README: only auth.rs is attributed.
        for (path, op) in [("src/auth.rs", "edit"), ("README.md", "read")] {
            conn.execute(
                "INSERT INTO file_edits (thread_id, path, op, ts) VALUES (?1, ?2, ?3, 1)",
                rusqlite::params![tid, path, op],
            )
            .unwrap();
        }
        conn.execute(
            "UPDATE messages SET model = 'claude-opus-4-5' WHERE thread_id = ?1",
            [tid],
        )
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(30);
        let built = build_records(&conn, &repo, &repo, None, deadline).unwrap();
        assert_eq!(
            (built.records.len(), built.sessions, built.commits),
            (1, 1, 1)
        );
        assert_eq!(built.missing_commits, 1, "one commit, linked twice");
        assert_eq!(built.oversized_commits, 0);
        // The file is meant to be committed: nothing in it names a path on this machine.
        let line = &built.records[0].1;
        let home = dirs::home_dir().unwrap().to_string_lossy().into_owned();
        for leak in [
            "-Users-", "/Users/", "-home-", "/home/", "alice", &home, &repo,
        ] {
            assert!(!line.contains(leak), "{leak} in {line}");
        }
        let record: Value = serde_json::from_str(line).unwrap();
        crate::schema_check::validate(&published_schema(), &record).unwrap();
        assert_published_constraints(&record);
        assert_eq!(record["version"], SPEC_VERSION);
        assert_eq!(record["vcs"]["revision"], sha.as_str());
        assert_eq!(record["timestamp"], "2023-11-14T22:13:20Z");
        assert_eq!(record["metadata"]["conversation_id"], uuid);
        assert_eq!(record["metadata"]["agent"], "claude-code");
        let files = record["files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0]["path"], "src/auth.rs");
        let conv = &files[0]["conversations"][0];
        assert_eq!(conv["contributor"]["model_id"], "anthropic/claude-opus-4-5");
        assert_eq!(conv["ranges"], json!([{ "start_line": 2, "end_line": 3 }]));
        // A window that ends before the commit exports nothing.
        let none = build_records(&conn, &repo, &repo, Some(1_800_000_000), deadline).unwrap();
        assert!(none.records.is_empty());

        // Appended once: a second export finds its ids.
        let out = default_out(&repo);
        assert_eq!(append_new(&out, &built.records, true).unwrap(), (0, 0));
        assert!(!out.exists(), "a dry run writes nothing");
        assert_eq!(append_new(&out, &built.records, false).unwrap(), (1, 0));
        assert_eq!(append_new(&out, &built.records, false).unwrap(), (0, 1));
        assert_eq!(std::fs::read_to_string(&out).unwrap().lines().count(), 1);

        // Another tool's record, written at edit time (revision = the parent), for a
        // conversation we never indexed; and ours read back, which is the linked session.
        let foreign = reference_record(&parent, "src/auth.rs", 3, 3).replace("conv-1", "cursor-77");
        let mut f = std::fs::OpenOptions::new().append(true).open(&out).unwrap();
        std::io::Write::write_all(&mut f, format!("{foreign}\n").as_bytes()).unwrap();
        let t = crate::why::resolve("src/auth.rs", &dir, deadline).unwrap();
        let line3 = crate::why::why(&conn, &t, Some(3), deadline).unwrap();
        let c = &line3.commits[0];
        assert_eq!(c.sha, sha);
        assert_eq!(c.threads[0].id, tid);
        assert_eq!(
            c.trace.len(),
            1,
            "ours is the linked session: {:?}",
            c.trace
        );
        let a = &c.trace[0];
        assert_eq!(a.conversation_id.as_deref(), Some("cursor-77"));
        assert_eq!(a.agent.as_deref(), Some("cursor"));
        assert!(a.recorded_before_commit && a.thread_id.is_none());
        assert_eq!(a.trace_file, ".agent-trace/traces.jsonl");
        // Line 2 is ours alone; line 1 is the parent's, which no record names.
        let line2 = crate::why::why(&conn, &t, Some(2), deadline).unwrap();
        assert!(line2.commits[0].trace.is_empty());
        let line1 = crate::why::why(&conn, &t, Some(1), deadline).unwrap();
        assert_eq!(line1.commits[0].sha, parent);
        assert!(line1.commits[0].trace.is_empty());
        // The whole file: the commit owns lines 2-3, the parent line 1.
        let whole = crate::why::why(&conn, &t, None, deadline).unwrap();
        let ours = whole.commits.iter().find(|c| c.sha == sha).unwrap();
        assert_eq!(ours.ranges, vec![[2, 3]]);
        assert_eq!(ours.trace.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn has_git() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "calli_trace_{name}_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn git_in(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_DATE", "1700000000 +0000")
            .env("GIT_COMMITTER_DATE", "1700000000 +0000")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn init_repo(dir: &Path) {
        git_in(dir, &["init", "-q"]);
        git_in(dir, &["config", "user.email", "t@t"]);
        git_in(dir, &["config", "user.name", "t"]);
    }

    /// A cloned repo controls `.agent-trace/`: a log that is a symlink (to a large file
    /// elsewhere, to `/dev/zero`), or a `.agent-trace` that is one, is never read; a regular
    /// log beside them still is.
    #[cfg(unix)]
    #[test]
    fn trace_logs_never_follow_symlinks() {
        use std::os::unix::fs::symlink;
        let root = scratch("links");
        let repo = root.join("repo");
        let outside = root.join("outside");
        std::fs::create_dir_all(repo.join(TRACE_DIR)).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let record = reference_record(C, "src/auth.rs", 1, 1);
        // 8 MiB of records outside the repo: within the cap, so only the link check keeps
        // them out.
        let big = outside.join("big.jsonl");
        let mut text = String::new();
        while text.len() < 8 << 20 {
            text.push_str(&record.replace("conv-1", "outside"));
            text.push('\n');
        }
        std::fs::write(&big, &text).unwrap();
        symlink(&big, repo.join(TRACE_DIR).join("a.jsonl")).unwrap();
        symlink("/dev/zero", repo.join(TRACE_DIR).join("b.jsonl")).unwrap();
        std::fs::write(
            repo.join(TRACE_DIR).join("c.jsonl"),
            format!("{}\n", record.replace("conv-1", "inside")),
        )
        .unwrap();
        let started = Instant::now();
        let deadline = started + Duration::from_secs(10);
        let logs = trace_logs(&repo, deadline);
        assert!(started.elapsed() < Duration::from_secs(5));
        let names: Vec<&str> = logs.iter().map(|(_, n)| n.as_str()).collect();
        assert_eq!(names, vec![".agent-trace/c.jsonl"]);
        assert!(logs[0].0.contains("inside") && !logs[0].0.contains("outside"));

        // `.agent-trace` itself a link to the directory of large logs: nothing is read.
        let linked = root.join("linked");
        std::fs::create_dir_all(&linked).unwrap();
        symlink(&outside, linked.join(TRACE_DIR)).unwrap();
        assert!(trace_logs(&linked, deadline).is_empty());

        // A log past what is left of the budget is skipped, not cut.
        assert!(read_regular(&big, 1024, deadline).is_err());
        assert_eq!(
            read_regular(&big, 64 << 20, deadline).unwrap().len(),
            text.len()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `cal trace export` into a cloned repo writes nothing through a link: a symlinked
    /// `.agent-trace` or `traces.jsonl` is refused and the place it points to is untouched;
    /// a missing `.agent-trace` is created.
    #[cfg(unix)]
    #[test]
    fn export_never_writes_through_symlinks() {
        use std::os::unix::fs::symlink;
        let root = scratch("export_links");
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let records = vec![("id-1".to_string(), "{\"id\":\"id-1\"}".to_string())];

        let dir_link = root.join("a");
        std::fs::create_dir_all(&dir_link).unwrap();
        symlink(&outside, dir_link.join(TRACE_DIR)).unwrap();
        let err = append_new(&default_out(&dir_link.to_string_lossy()), &records, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("symlink"), "{err}");
        assert!(std::fs::read_dir(&outside).unwrap().next().is_none());

        let rc = outside.join("shellrc");
        std::fs::write(&rc, "export A=1\n").unwrap();
        let file_link = root.join("b");
        std::fs::create_dir_all(file_link.join(TRACE_DIR)).unwrap();
        symlink(&rc, file_link.join(TRACE_DIR).join(TRACE_FILE)).unwrap();
        let out = default_out(&file_link.to_string_lossy());
        assert!(append_new(&out, &records, false).is_err());
        assert!(
            append_new(&out, &records, true).is_err(),
            "a dry run reads it too"
        );
        assert_eq!(std::fs::read_to_string(&rc).unwrap(), "export A=1\n");

        let fresh = root.join("c");
        std::fs::create_dir_all(&fresh).unwrap();
        let out = default_out(&fresh.to_string_lossy());
        assert_eq!(append_new(&out, &records, false).unwrap(), (1, 0));
        assert_eq!(append_new(&out, &records, false).unwrap(), (0, 1));
        // `-o` into a missing directory whose parent is missing too: refused, not made.
        let deep = root.join("x").join("y").join("t.jsonl");
        assert!(append_new(&deep, &records, false).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Record ranges are positions at the recorded commit. After a later commit inserts
    /// lines above, the line the record names has moved: it is still attributed (by blame's
    /// position at the commit), and the line now at the recorded number is not.
    #[test]
    fn ranges_are_matched_at_the_recorded_revision() {
        if !has_git() {
            return;
        }
        let dir = scratch("moved");
        init_repo(&dir);
        std::fs::write(dir.join("f.txt"), "a\nb\n").unwrap();
        git_in(&dir, &["add", "."]);
        git_in(&dir, &["commit", "-q", "-m", "base"]);
        std::fs::write(dir.join("f.txt"), "a\nb\nagent line\n").unwrap();
        git_in(&dir, &["commit", "-q", "-am", "the agent's line"]);
        let c = git_in(&dir, &["rev-parse", "HEAD"]);
        std::fs::write(dir.join("f.txt"), "new 1\nnew 2\na\nb\nagent line\n").unwrap();
        git_in(&dir, &["commit", "-q", "-am", "two lines on top"]);
        std::fs::create_dir_all(dir.join(TRACE_DIR)).unwrap();
        std::fs::write(
            dir.join(TRACE_DIR).join(TRACE_FILE),
            format!(
                "{}\n",
                reference_record(&c, "f.txt", 3, 3).replace("conv-1", "conv-moved")
            ),
        )
        .unwrap();
        let conn = temp_db();
        let deadline = Instant::now() + Duration::from_secs(30);
        let t = crate::why::resolve("f.txt", &dir, deadline).unwrap();
        // Line 5 is the agent's line, written as line 3 at `c`.
        let moved = crate::why::why(&conn, &t, Some(5), deadline).unwrap();
        assert_eq!(moved.commits[0].sha, c);
        assert_eq!(moved.commits[0].trace.len(), 1, "{:?}", moved.commits[0]);
        assert_eq!(
            moved.commits[0].trace[0].conversation_id.as_deref(),
            Some("conv-moved")
        );
        // Line 3 today is `a`, from the base commit: no record names it.
        let three = crate::why::why(&conn, &t, Some(3), deadline).unwrap();
        assert!(
            three.commits.iter().all(|c| c.trace.is_empty()),
            "{three:?}"
        );
        // The whole file: only `c` carries the attribution.
        let whole = crate::why::why(&conn, &t, None, deadline).unwrap();
        let traced: Vec<&str> = whole
            .commits
            .iter()
            .filter(|c| !c.trace.is_empty())
            .map(|c| c.sha.as_str())
            .collect();
        assert_eq!(traced, vec![c.as_str()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Duplicates of linked sessions are dropped before the list is cut: five attributions
    /// to sessions the commit lists (at any rank) leave room for a sixth, foreign one.
    #[test]
    fn duplicates_are_dropped_before_the_cut() {
        let mut conn = temp_db();
        let mut linked = Vec::new();
        let mut attributions = Vec::new();
        for i in 0..MAX_ATTRIBUTIONS_PER_COMMIT + 2 {
            let ext = format!("rollout-session-{i}");
            linked.push(crate::why::WhyThread {
                id: thread(&mut conn, "codex", &ext),
                title: None,
                source: "codex".into(),
                last_active: None,
                edits: 0,
                touch: None,
                overlap: 1,
                facts: vec![],
            });
            attributions.push(attribution(&ext));
        }
        attributions.push(attribution("foreign-conversation"));
        let mut picked: Vec<Picked> = vec![(blamed(C, &[1]), linked, attributions)];
        link_threads(&conn, &mut picked).unwrap();
        let left: Vec<Option<&str>> = picked[0]
            .2
            .iter()
            .map(|a| a.conversation_id.as_deref())
            .collect();
        assert_eq!(left, vec![Some("foreign-conversation")]);
    }

    /// Many records for one commit merge per conversation without a pass over the merged
    /// list per record.
    #[test]
    fn records_merge_per_conversation() {
        let log: Vec<String> = (0..3000)
            .map(|i| {
                reference_record(C, "src/auth.rs", i + 1, i + 1)
                    .replace("conv-1", &format!("conv-{}", i % 3))
            })
            .collect();
        let trace = FileTrace {
            rel: "src/auth.rs".into(),
            entries: entries_in(&log.join("\n"), "/repo", "src/auth.rs", "f"),
            ..Default::default()
        };
        let owned: Vec<u32> = (1..=3000).collect();
        let all = trace.for_commit(&blamed(C, &owned), None);
        assert_eq!(all.len(), 3);
        let conv0 = all
            .iter()
            .find(|a| a.conversation_id.as_deref() == Some("conv-0"))
            .unwrap();
        assert_eq!(conv0.ranges.len(), 1000, "every third line, none adjacent");
    }

    /// A commit whose diff is past the bound is exported with its files named and no
    /// ranges, and counted; textconv filters are not applied.
    #[test]
    fn oversized_diffs_name_files_without_ranges() {
        if !has_git() {
            return;
        }
        let dir = scratch("big_diff");
        init_repo(&dir);
        std::fs::write(dir.join("small.rs"), "x\n").unwrap();
        git_in(&dir, &["add", "."]);
        git_in(&dir, &["commit", "-q", "-m", "base"]);
        std::fs::write(dir.join("small.rs"), "x\ny\n").unwrap();
        std::fs::write(dir.join("vendor.lock"), "line\n".repeat(20_000)).unwrap();
        std::fs::remove_file(dir.join("small.rs")).ok();
        std::fs::write(dir.join("small.rs"), "x\ny\n").unwrap();
        git_in(&dir, &["add", "."]);
        git_in(&dir, &["commit", "-q", "-m", "vendor"]);
        let sha = git_in(&dir, &["rev-parse", "HEAD"]);
        let deadline = Instant::now() + Duration::from_secs(30);
        let full = commit_changes(&dir, &sha, deadline).unwrap();
        assert!(!full.oversized);
        assert_eq!(
            full.files[1],
            ("vendor.lock".to_string(), vec![[1, 20_000]])
        );
        let bounded = commit_changes_within(&dir, &sha, deadline, 4096).unwrap();
        assert!(bounded.oversized);
        assert_eq!(
            bounded.files,
            vec![
                ("small.rs".to_string(), vec![]),
                ("vendor.lock".to_string(), vec![])
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
