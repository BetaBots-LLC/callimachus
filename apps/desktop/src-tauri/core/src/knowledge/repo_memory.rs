//! Repo memory: a project's decisions and gotchas as files in its own git checkout, so the
//! team that clones the repo gets them, under the user's explicit publish.
//!
//! - Format: `<checkout>/.callimachus/memory/decisions.jsonl` and `gotchas.jsonl`, one JSON
//!   object per line ([`RepoFact`]), sorted by `uid` so two branches that each add facts
//!   merge line by line. A `README.md` beside them says what they are, and that they are
//!   notes, not instructions.
//! - The checkout is the repo's, so everything under `.callimachus/memory/` is untrusted: a
//!   symlink anywhere on that path is refused on read and on write, a read stops at
//!   [`MAX_FILE_BYTES`] whatever the file claims, and a publish never writes through a path
//!   it did not create itself.
//! - Publish ([`plan_publish`] then [`apply_publish`]; `cal memory publish`, the Projects
//!   page's "Publish to repo"): pinned facts plus the ones the user or an agent recorded,
//!   and with `all` the distilled ones too. Text and rationale are redacted. Only facts the
//!   user, an agent or distillation wrote are published: Claude Code auto memory
//!   (`claude-memory`, Claude's private notes, often about the user), compaction summaries
//!   and failure playbooks (local thread ids and paths) never are. Lines whose uid this
//!   machine does not know are kept as they are (a teammate's facts not imported yet), and
//!   so are imported lines the user hid here: a publish removes only the user's own lines.
//!   Nothing publishes automatically.
//! - Import ([`import_repo`]; the watcher after a batch touches a thread in the checkout,
//!   `cal index`, and `cal memory import`; [`plan_import`] is the read-only preview): upsert
//!   by uid with `extractor = 'repo'` and `origin_ref = 'repo:.callimachus/memory/<file>'`,
//!   never pinned. A fact whose uid is already local (one this machine published) is left
//!   alone and shows as shared; a local edit of an imported fact wins over the file, and a
//!   copy the user hid is never rewritten or shown again by an import. A fact whose line
//!   left the files (or whose file or directory is gone) is hidden once no checkout of the
//!   repo on this machine still has the line (`origin_ref` becomes `repo-removed:...`, so a
//!   branch switch that brings the line back restores it). A second clone of the same
//!   remote (another local key with the same portable id) reads the facts the first one
//!   holds through [`BORROWED`].
//!
//! - Reversals travel too: a line's `supersedes` lists the uids of the facts it replaces
//!   (written by publish from the fact's active `supersedes` links), and an import links the
//!   line's fact to each listed fact it holds in the same repo, with source `repo` (no
//!   downgrade of a pinned target: a person published the line). The import keeps those
//!   links in step with the field, so a line that drops it unlinks.
//!
//! Imported facts are the team's, so the SessionStart hook and the AGENTS.md block include
//! them, marked `repo` in each line's provenance suffix (`export::provenance`) under a trust
//! line that names the files (`export::REPO_TRUST_LINE`); MCP results carry
//! `extractor: "repo"`.

use crate::context::redact_secrets;
use anyhow::{Context as _, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// `facts.extractor` of a fact imported from a repo memory file.
pub const EXTRACTOR_REPO: &str = "repo";
/// Where the files live, relative to the checkout.
pub const MEMORY_DIR: &str = ".callimachus/memory";
/// The fact files, with the kind each holds.
pub const FILES: [(&str, &str); 2] = [("decision", "decisions.jsonl"), ("gotcha", "gotchas.jsonl")];
/// `origin_ref` prefix of an imported fact (`repo:.callimachus/memory/decisions.jsonl`).
const ORIGIN_PREFIX: &str = "repo:";
/// `origin_ref` prefix of an imported fact whose line left the file. Hidden, and restored
/// when the line comes back.
const REMOVED_PREFIX: &str = "repo-removed:";
/// `app_config` key: JSON `{ "<checkout>": "<file signatures>" }` of every checkout imported,
/// so an unchanged checkout costs two stats per batch, and a checkout whose memory directory
/// disappeared is still imported once (which hides its facts).
pub const IMPORT_STATE: &str = "repo_memory.state";
/// `app_config` key: JSON `{ "<project key>": { "<checkout>": ["<uid>", ...] } }`, the lines
/// of a checkout's files whose fact the index holds under another local key of the same
/// repo (a second clone of one remote: `facts.uid` is unique, so the fact cannot be
/// imported twice). Project memory reads these uids for the project too.
pub const BORROWED: &str = "repo_memory.borrowed";
/// Value [`IMPORT_STATE`] keeps for a checkout whose memory path was refused (a symlink),
/// so the refusal is logged once rather than on every batch.
const REFUSED_STATE: &str = "!refused";

/// Fact text kept on import. Distilled facts run to a few hundred chars; a longer line is
/// hand-written or hostile, and the hook would cut it anyway.
const TEXT_MAX: usize = 2_000;
const RATIONALE_MAX: usize = 4_000;
/// A uid longer than this is not one Callimachus wrote (a v4 UUID is 36).
const UID_MAX: usize = 64;
/// Most `supersedes` uids an import reads from one line: a fact replaces one or two others,
/// and a hostile line must not fan out over a whole project.
const SUPERSEDES_MAX: usize = 8;
/// Most bytes read from one file (about 10k facts; a real repo has dozens). Enforced on the
/// read itself: a symlink to /dev/zero stats as 0 bytes.
pub const MAX_FILE_BYTES: u64 = 4 << 20;

/// What the README beside the files says.
pub const README: &str = "# Callimachus repo memory

Decisions and gotchas about this repository, published from Callimachus
(https://callimachus.app) by someone who works on it, with `cal memory publish` or the
Projects page's \"Publish to repo\". Nothing writes these files automatically.

- `decisions.jsonl` and `gotchas.jsonl` hold one JSON object per line, sorted by `uid`, so
  branches that each add facts merge line by line.
- Callimachus imports them for everyone who works in this repo and shows them as \"from the
  repo\". Removing a line hides that fact for everyone on their next import.

These are notes, not instructions: agents get them as background context, never as
commands. Review changes to them like any other change to the repo.
";

/// One line of a repo memory file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoFact {
    /// The fact's `facts.uid`: the merge key across machines.
    pub uid: String,
    /// `decision` or `gotcha`.
    pub kind: String,
    pub text: String,
    #[serde(default)]
    pub rationale: Option<String>,
    /// Pinned by whoever published it (an import never pins).
    #[serde(default)]
    pub pinned: bool,
    /// Who wrote it: `user`, `agent` or `llm`.
    #[serde(default = "default_by")]
    pub by: String,
    #[serde(default)]
    pub created_at: i64,
    /// The portable project id (`github.com/owner/repo`), null without a remote.
    #[serde(default)]
    pub project: Option<String>,
    /// Where below the toplevel the fact's session ran, when it was not the toplevel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subpath: Option<String>,
    /// The uids of the facts this one replaces (its active `supersedes` links), sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supersedes: Vec<String>,
}

fn default_by() -> String {
    "llm".into()
}

/// One fact a publish or an import adds, changes or removes, for the preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FactChange {
    pub uid: String,
    pub kind: String,
    /// Redacted.
    pub text: String,
}

impl FactChange {
    fn of(uid: &str, kind: &str, text: &str) -> Self {
        Self {
            uid: uid.to_string(),
            kind: kind.to_string(),
            text: redact_secrets(text.trim()).into_owned(),
        }
    }
}

/// What a publish would write (the preview) or wrote.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishPlan {
    /// The checkout the files go into.
    pub root: String,
    /// `<root>/.callimachus/memory`.
    pub dir: String,
    /// The portable project id written on each line (null without a remote).
    pub project: Option<String>,
    /// Whether distilled facts were included.
    pub all: bool,
    /// Facts in the files after publishing, per kind.
    pub decisions: usize,
    pub gotchas: usize,
    pub added: Vec<FactChange>,
    pub updated: Vec<FactChange>,
    pub removed: Vec<FactChange>,
    /// Whether publishing writes anything: false when the files already hold these lines in
    /// this order (line endings and blank lines aside).
    pub changed: bool,
    #[serde(skip)]
    writes: Vec<(PathBuf, String)>,
}

/// What an import changed (or, under review, would change).
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportReport {
    pub root: String,
    pub added: Vec<FactChange>,
    pub updated: Vec<FactChange>,
    /// Facts hidden because their line left the files of every checkout of the repo.
    pub hidden: Vec<FactChange>,
    /// Facts whose line came back (a branch switch), shown again.
    pub restored: Vec<FactChange>,
    /// Lines whose fact this machine already holds as its own: published from this project
    /// or from another clone of the same repo.
    pub shared: usize,
    /// Lines skipped: not JSON, no uid or text, an unknown kind, or a uid another project
    /// (not a clone of this repo) already holds.
    pub skipped: usize,
}

impl ImportReport {
    pub fn changed(&self) -> bool {
        !(self.added.is_empty()
            && self.updated.is_empty()
            && self.hidden.is_empty()
            && self.restored.is_empty())
    }
}

/// The Projects page's view of a project's repo memory.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoMemoryStatus {
    /// The project's git checkout, null when it has none on this machine (nothing to
    /// publish into).
    pub root: Option<String>,
    /// Whether `.callimachus/memory/` exists there.
    pub published: bool,
    /// Ids of this machine's own facts whose uid is in the files (published from here).
    pub shared_ids: Vec<i64>,
    /// Visible facts imported from the files.
    pub imported: i64,
}

/// The git checkout containing `path` (worktrees stay themselves: files are written into
/// the checkout the user commits from), or None outside one.
pub fn checkout_root(path: &Path) -> Option<PathBuf> {
    let root = crate::cli_core::git_root_of(path);
    root.join(".git").exists().then_some(root)
}

fn memory_dir(root: &Path) -> PathBuf {
    root.join(MEMORY_DIR)
}

fn origin_ref(file: &str) -> String {
    format!("{ORIGIN_PREFIX}{MEMORY_DIR}/{file}")
}

// ---- the untrusted path ----

fn is_not_found(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::NotFound
}

/// Whether `path` is a real directory (false: absent). A symlink or anything else there is
/// refused: the repo controls it, and following it would read or write outside the checkout.
fn real_dir(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => anyhow::bail!(
            "{} is a symlink; repo memory must be a real directory in the checkout (remove the \
             link, then try again)",
            path.display()
        ),
        Ok(m) if !m.is_dir() => anyhow::bail!("{} is not a directory", path.display()),
        Ok(_) => Ok(true),
        Err(e) if is_not_found(&e) => Ok(false),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Whether `path` is a regular file (false: absent). A symlink or anything else is refused.
fn regular_file(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => anyhow::bail!(
            "{} is a symlink; repo memory files must be regular files in the checkout (remove \
             the link, then try again)",
            path.display()
        ),
        Ok(m) if !m.is_file() => anyhow::bail!("{} is not a regular file", path.display()),
        Ok(_) => Ok(true),
        Err(e) if is_not_found(&e) => Ok(false),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Whether `<root>/.callimachus/memory` exists, with no symlink on the way (an error).
fn memory_dir_state(root: &Path) -> Result<bool> {
    Ok(real_dir(&root.join(".callimachus"))? && real_dir(&memory_dir(root))?)
}

/// The file at `path` as UTF-8, None when absent. Callers check the directory with
/// [`memory_dir_state`] first.
fn read_capped(path: &Path) -> Result<Option<String>> {
    use std::io::Read as _;
    if !regular_file(path)? {
        return Ok(None);
    }
    let file = std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    // Checked again on the open handle: the path can change between the stat and the open.
    if !file.metadata()?.is_file() {
        anyhow::bail!("{} is not a regular file", path.display());
    }
    let mut buf = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut buf)
        .with_context(|| format!("reading {}", path.display()))?;
    if buf.len() as u64 > MAX_FILE_BYTES {
        anyhow::bail!(
            "{} is over the {} MB repo memory limit",
            path.display(),
            MAX_FILE_BYTES >> 20
        );
    }
    String::from_utf8(buf)
        .map(Some)
        .map_err(|_| anyhow::anyhow!("{} is not UTF-8", path.display()))
}

// ---- reading the files ----

/// One parsed line, with the text it was read from (kept verbatim when unchanged, so a
/// publish never rewrites a line it has nothing to say about).
#[derive(Debug, Clone)]
struct Line {
    fact: RepoFact,
    raw: String,
}

/// A file's lines and how many were skipped, or None when it does not exist. `strict`
/// (publish) fails on a line it cannot read, naming it, rather than dropping someone's
/// fact; an import skips it.
fn read_file(path: &Path, strict: bool) -> Result<Option<(Vec<Line>, usize)>> {
    let Some(text) = read_capped(path)? else {
        return Ok(None);
    };
    let mut lines = Vec::new();
    let mut skipped = 0;
    for (i, raw) in text.lines().enumerate() {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        if ["<<<<<<<", "=======", ">>>>>>>", "|||||||"]
            .iter()
            .any(|m| raw.starts_with(m))
        {
            if strict {
                anyhow::bail!(
                    "{}:{} has a merge conflict marker; resolve the conflict, then publish again",
                    path.display(),
                    i + 1
                );
            }
            skipped += 1;
            continue;
        }
        match serde_json::from_str::<RepoFact>(raw).map_err(anyhow::Error::from) {
            Ok(fact) if valid(&fact) => lines.push(Line {
                fact,
                raw: raw.to_string(),
            }),
            Ok(_) | Err(_) if !strict => skipped += 1,
            Ok(_) => anyhow::bail!(
                "{}:{} is not a fact Callimachus can read (it needs a uid, a kind of \
                 decision or gotcha, and text); fix or remove that line, then publish again",
                path.display(),
                i + 1
            ),
            Err(e) => anyhow::bail!(
                "{}:{} is not valid JSON ({e}); fix or remove that line, then publish again",
                path.display(),
                i + 1
            ),
        }
    }
    Ok(Some((lines, skipped)))
}

fn valid(f: &RepoFact) -> bool {
    valid_uid(&f.uid)
        && matches!(f.kind.as_str(), "decision" | "gotcha")
        && !f.text.trim().is_empty()
}

fn valid_uid(uid: &str) -> bool {
    !uid.is_empty()
        && uid.len() <= UID_MAX
        && uid
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

fn cap(s: &str, max: usize) -> String {
    let s = s.trim();
    match s.char_indices().nth(max) {
        Some((i, _)) => s[..i].trim_end().to_string(),
        None => s.to_string(),
    }
}

// ---- publish ----

/// A local decision or gotcha of the project, as publish and status see it.
struct LocalRow {
    id: i64,
    uid: String,
    kind: String,
    text: String,
    rationale: Option<String>,
    pinned: bool,
    edited: bool,
    hidden: bool,
    extractor: String,
    created_at: i64,
    subagent: bool,
    project_path: Option<String>,
}

/// Every decision and gotcha of the project, hidden ones included (a hidden fact is a
/// removal). Facts are few (about 2k on a 21k-thread index), so this walks them off
/// idx_facts_kind and checks each one's thread by primary key.
pub(crate) const LOCAL_ROWS_SQL: &str = "SELECT f.id, f.uid, f.kind, f.text, f.rationale,
        f.pinned, f.edited, f.hidden, f.extractor, f.created_at, t.is_subagent,
        t.project_path
     FROM facts f JOIN threads t ON t.id = f.thread_id
     WHERE f.kind IN ('decision', 'gotcha') AND f.uid IS NOT NULL
       AND COALESCE(t.project_key, t.project_path) = ?1";

fn local_rows(conn: &Connection, project: &str) -> Result<Vec<LocalRow>> {
    let mut stmt = conn.prepare_cached(LOCAL_ROWS_SQL)?;
    let rows = stmt
        .query_map([project], |r| {
            Ok(LocalRow {
                id: r.get(0)?,
                uid: r.get(1)?,
                kind: r.get(2)?,
                text: r.get(3)?,
                rationale: r.get(4)?,
                pinned: r.get::<_, i64>(5)? != 0,
                edited: r.get::<_, i64>(6)? != 0,
                hidden: r.get::<_, i64>(7)? != 0,
                extractor: r.get(8)?,
                created_at: r.get(9)?,
                subagent: r.get::<_, i64>(10)? != 0,
                project_path: r.get(11)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Whether a publish writes this local fact: the user's trusted set (pinned) and what the
/// user or an agent recorded, plus distilled facts with `all`. An allowlist of who wrote it,
/// so a machine-local extractor is never published, pinned or not: auto memory
/// (`claude-memory`), compaction summaries (`claude-compact`), failure playbooks
/// (`playbook`, which cite local thread ids and paths), imports (`repo`), and any added
/// later. Never a hidden fact, and a subagent's fact only when pinned (as Project Memory
/// shows them).
fn publishable(r: &LocalRow, all: bool) -> bool {
    let authored = matches!(r.extractor.as_str(), "user" | "agent" | "llm");
    if !authored || r.hidden || (r.subagent && !r.pinned) {
        return false;
    }
    r.pinned || r.extractor != "llm" || all
}

fn by_of(extractor: &str) -> &'static str {
    match extractor {
        "user" => "user",
        "agent" => "agent",
        _ => "llm",
    }
}

/// Where below the project key the fact's session ran (`apps/web`), if it did.
fn subpath_of(project: &str, project_path: Option<&str>) -> Option<String> {
    let p = project_path?.trim().trim_end_matches(['/', '\\']);
    let base = project.trim_end_matches(['/', '\\']);
    let rest = p.strip_prefix(base)?;
    let rest = rest.strip_prefix(['/', '\\'])?;
    (!rest.is_empty()).then(|| rest.replace('\\', "/"))
}

fn entry_of(
    r: &LocalRow,
    project_key: &str,
    portable: Option<&str>,
    links: &HashMap<String, Vec<String>>,
) -> RepoFact {
    RepoFact {
        uid: r.uid.clone(),
        kind: r.kind.clone(),
        text: redact_secrets(r.text.trim()).into_owned(),
        rationale: r
            .rationale
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| redact_secrets(s).into_owned()),
        pinned: r.pinned,
        by: by_of(&r.extractor).to_string(),
        created_at: r.created_at,
        project: portable.map(str::to_string),
        subpath: subpath_of(project_key, r.project_path.as_deref()),
        supersedes: links.get(&r.uid).cloned().unwrap_or_default(),
    }
}

/// What publishing `project`'s memory into the checkout at or above `path` would write.
/// Reads the index and the files; writes nothing ([`apply_publish`] does).
pub fn plan_publish(conn: &Connection, path: &Path, all: bool) -> Result<PublishPlan> {
    let root = checkout_root(path).ok_or_else(|| {
        anyhow::anyhow!(
            "{} is not inside a git checkout; repo memory is written into a repository so it \
             can be committed",
            path.display()
        )
    })?;
    let project = super::project_key(&root.to_string_lossy());
    let portable = super::refresh_portable_project_id(conn, &project);
    plan_publish_with(conn, &root, &project, portable.as_deref(), all)
}

/// [`plan_publish`] with the checkout, its project key and its portable id given.
pub(crate) fn plan_publish_with(
    conn: &Connection,
    root: &Path,
    project: &str,
    portable: Option<&str>,
    all: bool,
) -> Result<PublishPlan> {
    let dir = memory_dir(root);
    let present = memory_dir_state(root)?;
    // uid -> (kind, line as it should be written). A BTreeMap: the files are sorted by uid.
    let mut next: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut existing: HashMap<String, Line> = HashMap::new();
    // file -> its lines as read (None: no file), to tell a real change from a file that
    // only differs in line endings or blank lines.
    let mut old_lines: HashMap<&str, Option<Vec<String>>> = HashMap::new();
    for (_, file) in FILES {
        let read = if present {
            read_file(&dir.join(file), true)?
        } else {
            None
        };
        if let Some((lines, _)) = read {
            old_lines.insert(file, Some(lines.iter().map(|l| l.raw.clone()).collect()));
            for l in lines {
                // A uid on two lines (a careless merge): the last one wins, once.
                existing.insert(l.fact.uid.clone(), l);
            }
        } else {
            old_lines.insert(file, None);
        }
    }
    let rows = local_rows(conn, project)?;
    let local: HashMap<&str, &LocalRow> = rows.iter().map(|r| (r.uid.as_str(), r)).collect();
    // What each published fact supersedes, limited to facts a teammate can know: this
    // project's own and the lines already in the files.
    let links = {
        let from: Vec<&str> = rows
            .iter()
            .filter(|r| publishable(r, all))
            .map(|r| r.uid.as_str())
            .collect();
        let mut links = super::truth::superseded_uids(conn, &from)?;
        for targets in links.values_mut() {
            targets.retain(|t| local.contains_key(t.as_str()) || existing.contains_key(t));
        }
        links.retain(|_, t| !t.is_empty());
        links
    };

    let mut plan = PublishPlan {
        root: root.to_string_lossy().into_owned(),
        dir: dir.to_string_lossy().into_owned(),
        project: portable.map(str::to_string),
        all,
        ..Default::default()
    };
    let mut existing_sorted: Vec<&Line> = existing.values().collect();
    existing_sorted.sort_by(|a, b| a.fact.uid.cmp(&b.fact.uid));
    for line in existing_sorted {
        let f = &line.fact;
        let keep = |next: &mut BTreeMap<String, (String, String)>| {
            next.insert(f.uid.clone(), (f.kind.clone(), line.raw.clone()));
        };
        match local.get(f.uid.as_str()) {
            // Not this project's on this machine: a teammate's fact not imported yet, one
            // imported under another clone of the repo, or one whose local copy is gone (a
            // re-distill replaced it). Kept as written.
            None => keep(&mut next),
            // A teammate's fact, imported. The rule: a publish removes a line only when it
            // is the user's own fact (written here, not imported) that is now hidden or no
            // longer selected. Hiding an imported fact is a local preference and keeps its
            // line for everyone else; the teammate who published it removes it.
            Some(r) if r.extractor == EXTRACTOR_REPO => {
                if r.edited && !r.hidden {
                    // The user's edit of an imported fact wins; everything else about the
                    // line (its author, date, pin) stays the publisher's.
                    let mut e = f.clone();
                    e.text = redact_secrets(r.text.trim()).into_owned();
                    if e == *f {
                        keep(&mut next);
                    } else {
                        plan.updated.push(FactChange::of(&e.uid, &e.kind, &e.text));
                        next.insert(e.uid.clone(), (e.kind.clone(), serde_json::to_string(&e)?));
                    }
                } else {
                    // Not edited here, or hidden here: the file is the source, even when it
                    // changed since the last import (a pulled update must not be reverted).
                    keep(&mut next);
                }
            }
            Some(r) if publishable(r, all) => {
                let e = entry_of(r, project, portable, &links);
                if e == *f {
                    keep(&mut next);
                } else {
                    plan.updated.push(FactChange::of(&e.uid, &e.kind, &e.text));
                    next.insert(e.uid.clone(), (e.kind.clone(), serde_json::to_string(&e)?));
                }
            }
            // The user's own, and hidden or no longer selected (or a machine-local
            // extractor, which never belongs here): the line goes.
            Some(_) => plan.removed.push(FactChange::of(&f.uid, &f.kind, &f.text)),
        }
    }
    let mut fresh: Vec<&LocalRow> = rows
        .iter()
        .filter(|r| publishable(r, all) && !existing.contains_key(&r.uid))
        .collect();
    fresh.sort_by(|a, b| a.uid.cmp(&b.uid));
    for r in fresh {
        let e = entry_of(r, project, portable, &links);
        plan.added.push(FactChange::of(&e.uid, &e.kind, &e.text));
        next.insert(e.uid.clone(), (e.kind.clone(), serde_json::to_string(&e)?));
    }

    for (kind, file) in FILES {
        let lines: Vec<&str> = next
            .values()
            .filter(|(k, _)| k == kind)
            .map(|(_, l)| l.as_str())
            .collect();
        match kind {
            "decision" => plan.decisions = lines.len(),
            _ => plan.gotchas = lines.len(),
        }
        // Compared line by line, not as text: a CRLF checkout (core.autocrlf on Windows) or a
        // stray blank line is not a change, while an add, an update, a removal, a line that
        // moved file, a duplicate or an order change is.
        let unchanged = match old_lines.get(file).and_then(Option::as_ref) {
            Some(old) => old.iter().map(String::as_str).eq(lines.iter().copied()),
            // Never create an empty file.
            None => lines.is_empty(),
        };
        if !unchanged {
            let mut body = lines.join("\n");
            if !body.is_empty() {
                body.push('\n');
            }
            plan.writes.push((dir.join(file), body));
        }
    }
    plan.changed = !plan.writes.is_empty();
    Ok(plan)
}

/// Write what [`plan_publish`] planned: each changed file swapped in atomically, and the
/// README when there is none yet. A no-op for an unchanged plan. Every path is checked
/// before anything is written, so a refusal (a symlink the repo committed) leaves the
/// checkout as it was.
pub fn apply_publish(plan: &PublishPlan) -> Result<()> {
    if !plan.changed {
        return Ok(());
    }
    let root = Path::new(&plan.root);
    let base = root.join(".callimachus");
    let dir = memory_dir(root);
    let base_exists = real_dir(&base)?;
    let dir_exists = base_exists && real_dir(&dir)?;
    let readme = dir.join("README.md");
    let mut need_readme = true;
    if dir_exists {
        for (path, _) in &plan.writes {
            regular_file(path)?;
        }
        need_readme = !regular_file(&readme)?;
    }
    // One level at a time, never create_dir_all: that would follow a symlinked parent.
    if !base_exists {
        std::fs::create_dir(&base).with_context(|| format!("creating {}", base.display()))?;
    }
    if !dir_exists {
        std::fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    for (path, body) in &plan.writes {
        write_atomic(path, body)?;
    }
    if need_readme {
        write_atomic(&readme, README)?;
    }
    Ok(())
}

/// Write `body` to a temp file beside `path`, then rename it over `path`. The temp name is
/// predictable and the directory is the repo's, so whatever sits there (a leftover, or a
/// committed symlink aimed outside the checkout) is removed, never written through, and the
/// file is created with `create_new` (O_EXCL), which refuses anything that appears in
/// between rather than following it.
fn write_atomic(path: &Path, body: &str) -> Result<()> {
    use std::io::Write as _;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.callimachus-tmp"));
    match std::fs::symlink_metadata(&tmp) {
        Ok(m) if m.is_dir() => anyhow::bail!(
            "{} is a directory; remove it, then publish again",
            tmp.display()
        ),
        Ok(_) => std::fs::remove_file(&tmp)
            .with_context(|| format!("removing the leftover {}", tmp.display()))?,
        Err(e) if is_not_found(&e) => {}
        Err(e) => return Err(e).with_context(|| format!("reading {}", tmp.display())),
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    f.write_all(body.as_bytes())
        .with_context(|| format!("writing {}", tmp.display()))?;
    drop(f);
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// [`plan_publish`] then [`apply_publish`].
pub fn publish(conn: &Connection, path: &Path, all: bool) -> Result<PublishPlan> {
    let plan = plan_publish(conn, path, all)?;
    apply_publish(&plan)?;
    Ok(plan)
}

// ---- import ----

/// The uid lookup an import makes per line, off idx_facts_uid.
pub(crate) const BY_UID_SQL: &str = "SELECT f.id, f.extractor, f.edited, f.hidden, f.text,
        f.rationale, f.kind, COALESCE(f.origin_ref, ''),
        COALESCE(t.project_key, t.project_path, '')
     FROM facts f JOIN threads t ON t.id = f.thread_id WHERE f.uid = ?1";

/// The visible imported facts of one file under one project key, off idx_facts_origin_ref.
/// Visible only: a copy the user hid keeps its `repo:` origin and its hide, so a line that
/// leaves and comes back (a branch switch) never shows it again; copies already marked
/// removed carry another origin and are not looked at again.
pub(crate) const BY_ORIGIN_SQL: &str = "SELECT f.id, f.uid, f.kind, f.text
     FROM facts f JOIN threads t ON t.id = f.thread_id
     WHERE f.origin_ref = ?1 AND f.extractor = 'repo' AND f.hidden = 0
       AND COALESCE(t.project_key, t.project_path) = ?2";

/// A fact an import line's uid already names.
struct Known {
    id: i64,
    extractor: String,
    edited: bool,
    hidden: bool,
    text: String,
    rationale: Option<String>,
    kind: String,
    origin: String,
    project: String,
}

/// One write an import makes. Planned before any is made, so the preview ([`plan_import`])
/// needs no write transaction.
enum ImportWrite {
    Insert {
        uid: String,
        kind: String,
        text: String,
        rationale: Option<String>,
        origin: String,
        created: i64,
    },
    /// The line is in another file now (its kind changed), or came back (`restore`).
    Relabel {
        id: i64,
        origin: String,
        restore: bool,
    },
    Update {
        id: i64,
        text: String,
        rationale: Option<String>,
        kind: String,
        text_changed: bool,
    },
    Hide {
        id: i64,
        origin: String,
    },
    /// Bring the `repo` links from `from` in line with its line's `supersedes`.
    Links {
        from: String,
        add: Vec<String>,
        remove: Vec<String>,
    },
}

struct ImportPlan {
    report: ImportReport,
    writes: Vec<ImportWrite>,
    /// Whether the memory directory exists (false: the import only hides).
    present: bool,
    /// Uids of lines whose fact lives under another clone's key (see [`BORROWED`]).
    borrowed: Vec<String>,
}

/// The local project keys that are this repo: `project` first, then every other clone on
/// this machine with the same portable id. Facts of the family count as this project's for
/// an import; they live under whichever key imported them first (`facts.uid` is unique).
fn family(conn: &Connection, project: &str) -> Vec<String> {
    let mut keys = vec![project.to_string()];
    if let Some(id) = super::portable_project_id(conn, project) {
        for k in super::local_keys_for_portable(conn, &id) {
            if !keys.contains(&k) {
                keys.push(k);
            }
        }
    }
    keys
}

/// The uids in the files of every other checkout of the family that an import has seen
/// (worktrees share a key, clones a portable id, and either can sit on another branch), or
/// None when one of them cannot be read: then nothing is hidden.
fn uids_elsewhere(conn: &Connection, root: &Path, family: &[String]) -> Option<HashSet<String>> {
    let mut out = HashSet::new();
    for other in load_state(conn).into_keys() {
        let other = PathBuf::from(other);
        if other == root || !family.contains(&super::project_key(&other.to_string_lossy())) {
            continue;
        }
        match memory_dir_state(&other) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(_) => return None,
        }
        for (_, file) in FILES {
            match read_file(&memory_dir(&other).join(file), false) {
                Ok(Some((lines, _))) => out.extend(lines.into_iter().map(|l| l.fact.uid)),
                Ok(None) => {}
                Err(_) => return None,
            }
        }
    }
    Some(out)
}

/// What importing the checkout at `root` would do, from reads alone.
fn plan_import_with(
    conn: &Connection,
    root: &Path,
    project: &str,
    family: &[String],
    now: i64,
) -> Result<ImportPlan> {
    let present = memory_dir_state(root)?;
    let dir = memory_dir(root);
    let mut report = ImportReport {
        root: root.to_string_lossy().into_owned(),
        ..Default::default()
    };
    // file -> its lines. A missing file (or directory) has none, so its facts are hidden; a
    // file that cannot be read says nothing, and then nothing at all is hidden (its lines
    // may have moved to the other file).
    let mut files: Vec<(&str, Vec<RepoFact>)> = Vec::new();
    let mut unreadable = false;
    for (_, file) in FILES {
        if !present {
            files.push((file, Vec::new()));
            continue;
        }
        match read_file(&dir.join(file), false) {
            Ok(Some((lines, skipped))) => {
                report.skipped += skipped;
                files.push((file, lines.into_iter().map(|l| l.fact).collect()));
            }
            Ok(None) => files.push((file, Vec::new())),
            Err(e) => {
                app_log!("[repo-memory] {e:#}");
                unreadable = true;
            }
        }
    }

    let mut writes = Vec::new();
    let mut borrowed = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    // Lines whose fact the import owns (new, or an imported copy it keeps in step), for the
    // link pass below: (line, whether it is new).
    let mut linkable: Vec<(&RepoFact, bool)> = Vec::new();
    for (file, lines) in &files {
        let origin = origin_ref(file);
        for f in lines {
            if !seen.insert(f.uid.clone()) {
                continue;
            }
            let text = cap(&f.text, TEXT_MAX);
            let rationale = f
                .rationale
                .as_deref()
                .map(|r| cap(r, RATIONALE_MAX))
                .filter(|r| !r.is_empty());
            let found = conn
                .query_row(BY_UID_SQL, [&f.uid], |r| {
                    Ok(Known {
                        id: r.get(0)?,
                        extractor: r.get(1)?,
                        edited: r.get::<_, i64>(2)? != 0,
                        hidden: r.get::<_, i64>(3)? != 0,
                        text: r.get(4)?,
                        rationale: r.get(5)?,
                        kind: r.get(6)?,
                        origin: r.get(7)?,
                        project: r.get(8)?,
                    })
                })
                .optional()?;
            let Some(k) = found else {
                linkable.push((f, true));
                report.added.push(FactChange::of(&f.uid, &f.kind, &text));
                writes.push(ImportWrite::Insert {
                    uid: f.uid.clone(),
                    kind: f.kind.clone(),
                    text,
                    rationale,
                    origin: origin.clone(),
                    created: if f.created_at > 0 {
                        f.created_at.min(now)
                    } else {
                        now
                    },
                });
                continue;
            };
            if !family.contains(&k.project) {
                // Another repo's file carries the same uid (a copied file or a fork): the
                // first project keeps it rather than the two trading it on every import.
                report.skipped += 1;
                continue;
            }
            if k.project != project {
                borrowed.push(f.uid.clone());
            }
            if k.extractor != EXTRACTOR_REPO {
                // This machine's own fact, published from here or from another clone: the
                // local copy wins.
                report.shared += 1;
                continue;
            }
            let was_removed = k.origin.starts_with(REMOVED_PREFIX);
            if was_removed {
                report.restored.push(FactChange::of(&f.uid, &f.kind, &text));
                writes.push(ImportWrite::Relabel {
                    id: k.id,
                    origin: origin.clone(),
                    restore: true,
                });
            } else if k.origin != origin {
                writes.push(ImportWrite::Relabel {
                    id: k.id,
                    origin: origin.clone(),
                    restore: false,
                });
            }
            if !k.hidden || was_removed {
                linkable.push((f, false));
            }
            // A local edit wins over the file, as a re-distill keeps an edited fact; a copy
            // the user hid is left exactly as they hid it (only the import's own hide, a
            // removed line coming back, is undone above).
            if k.edited || (k.hidden && !was_removed) {
                continue;
            }
            let text_changed = k.text != text;
            if text_changed || k.rationale != rationale || k.kind != f.kind {
                if !was_removed {
                    report.updated.push(FactChange::of(&f.uid, &f.kind, &text));
                }
                writes.push(ImportWrite::Update {
                    id: k.id,
                    text,
                    rationale,
                    kind: f.kind.clone(),
                    text_changed,
                });
            }
        }
    }

    // Each owned line's `supersedes`, as links. A target counts when this repo holds it (in
    // the family, same kind) or this import adds it; anything else (a teammate's fact not
    // here, another repo's uid, a kind mismatch) is ignored, so a hostile line cannot reach
    // facts outside the repo.
    let new_kinds: HashMap<&str, &str> = linkable
        .iter()
        .filter(|(_, new)| *new)
        .map(|(f, _)| (f.uid.as_str(), f.kind.as_str()))
        .collect();
    //
    // A wanted target the link writer would refuse (hidden here, which was the only way to
    // retire a replaced decision before 0035, a pair whose link the user undid in either
    // direction or settled as "both stand", or one that would close a cycle) is `blocked`:
    // neither added nor, if a link to it is already there, removed. Without that the line
    // would plan the same refused write, and show as updated, on every import.
    let truth = super::truth::has_truth_tables(conn);
    for (f, _) in linkable.iter().filter(|_| truth) {
        let mut want: Vec<String> = Vec::new();
        let mut blocked: Vec<String> = Vec::new();
        for t in f.supersedes.iter().take(SUPERSEDES_MAX) {
            if !valid_uid(t) || *t == f.uid || want.contains(t) || blocked.contains(t) {
                continue;
            }
            let ok = match new_kinds.get(t.as_str()) {
                Some(kind) => *kind == f.kind,
                None => {
                    let local = conn
                        .query_row(BY_UID_SQL, [t], |r| {
                            Ok((
                                r.get::<_, String>(6)?,
                                r.get::<_, String>(8)?,
                                r.get::<_, i64>(3)? != 0,
                            ))
                        })
                        .optional()?;
                    match local {
                        Some((kind, project, hidden))
                            if kind == f.kind && family.contains(&project) =>
                        {
                            if hidden {
                                blocked.push(t.clone());
                                false
                            } else {
                                true
                            }
                        }
                        _ => false,
                    }
                }
            };
            if !ok {
                continue;
            }
            if super::truth::pair_dismissed(conn, &f.uid, t)?
                || super::truth::supersedes_path(conn, t, &f.uid)?
            {
                blocked.push(t.clone());
            } else {
                want.push(t.clone());
            }
        }
        want.sort();
        let links = super::truth::supersedes_links_from(conn, &f.uid)?;
        let add: Vec<String> = want
            .iter()
            .filter(|t| !links.iter().any(|(to, _)| to == *t))
            .cloned()
            .collect();
        let remove: Vec<String> = links
            .into_iter()
            .filter(|(to, source)| source == "repo" && !want.contains(to) && !blocked.contains(to))
            .map(|(to, _)| to)
            .collect();
        if add.is_empty() && remove.is_empty() {
            continue;
        }
        let listed = |l: &[FactChange]| l.iter().any(|c| c.uid == f.uid);
        if !listed(&report.added) && !listed(&report.updated) && !listed(&report.restored) {
            report
                .updated
                .push(FactChange::of(&f.uid, &f.kind, &cap(&f.text, TEXT_MAX)));
        }
        writes.push(ImportWrite::Links {
            from: f.uid.clone(),
            add,
            remove,
        });
    }

    // Lines that left the files: hide their visible imported copies, across the family,
    // once no other checkout of the repo still has them.
    if !unreadable {
        let mut gone: Vec<(i64, String, String, String, &str)> = Vec::new();
        for (file, _) in &files {
            let origin = origin_ref(file);
            let mut stmt = conn.prepare_cached(BY_ORIGIN_SQL)?;
            for key in family {
                let rows = stmt
                    .query_map(params![origin, key], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, *file))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                gone.extend(rows.into_iter().filter(|g| !seen.contains(&g.1)));
            }
        }
        if !gone.is_empty() {
            match uids_elsewhere(conn, root, family) {
                Some(elsewhere) => gone.retain(|g| !elsewhere.contains(&g.1)),
                None => gone.clear(),
            }
        }
        for (id, uid, kind, text, file) in gone {
            report.hidden.push(FactChange::of(&uid, &kind, &text));
            writes.push(ImportWrite::Hide {
                id,
                origin: format!("{REMOVED_PREFIX}{MEMORY_DIR}/{file}"),
            });
        }
    }
    borrowed.sort();
    Ok(ImportPlan {
        report,
        writes,
        present,
        borrowed,
    })
}

/// What [`import_repo`] would change for the checkout at `root`, from reads alone: no
/// transaction and no write, so `cal memory import --review` can open the index read-only
/// (no migration, no write lock). Needs an index with `facts.uid` (migration 0034).
pub fn plan_import(conn: &Connection, root: &Path) -> Result<ImportReport> {
    let project = super::project_key(&root.to_string_lossy());
    let family = family(conn, &project);
    let now = chrono::Utc::now().timestamp();
    Ok(plan_import_with(conn, root, &project, &family, now)?.report)
}

/// Bring the imported facts of the checkout at `root` in line with its files, and record the
/// checkout (its signature for [`import_if_changed`], its borrowed uids). With `review`,
/// [`plan_import`]: report what would change and write nothing.
pub fn import_repo(conn: &Connection, root: &Path, now: i64, review: bool) -> Result<ImportReport> {
    let project = super::project_key(&root.to_string_lossy());
    // Before the write lock: resolving the portable id can run `git`.
    let family = family(conn, &project);
    if review {
        return Ok(plan_import_with(conn, root, &project, &family, now)?.report);
    }
    let sig = signature(root);
    let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let plan = plan_import_with(&tx, root, &project, &family, now)?;
    let mut thread: Option<i64> = None;
    for w in &plan.writes {
        match w {
            ImportWrite::Insert {
                uid,
                kind,
                text,
                rationale,
                origin,
                created,
            } => {
                let tid = match thread {
                    Some(t) => t,
                    None => *thread.insert(super::notes_thread_id(&tx, &project, now)?),
                };
                tx.execute(
                    "INSERT INTO facts (thread_id, kind, text, rationale, status, extractor,
                                        pinned, hidden, origin_ref, created_at, uid)
                     VALUES (?1, ?2, ?3, ?4, 'open', ?5, 0, 0, ?6, ?7, ?8)",
                    params![
                        tid,
                        kind,
                        text,
                        rationale,
                        EXTRACTOR_REPO,
                        origin,
                        created,
                        uid
                    ],
                )?;
            }
            ImportWrite::Relabel {
                id,
                origin,
                restore,
            } => {
                tx.execute(
                    "UPDATE facts SET origin_ref = ?2, hidden = CASE WHEN ?3 THEN 0 ELSE hidden END
                     WHERE id = ?1",
                    params![id, origin, restore],
                )?;
            }
            ImportWrite::Update {
                id,
                text,
                rationale,
                kind,
                text_changed,
            } => {
                // `edited = 0 AND hidden = 0` again in SQL: the rule the plan applied, held
                // by the statement itself.
                tx.execute(
                    "UPDATE facts SET text = ?2, rationale = ?3, kind = ?4,
                        embedded = CASE WHEN ?5 THEN 0 ELSE embedded END
                     WHERE id = ?1 AND edited = 0 AND hidden = 0",
                    params![id, text, rationale, kind, text_changed],
                )?;
                if *text_changed {
                    tx.execute("DELETE FROM vec_facts WHERE fact_id = ?1", [id])?;
                }
            }
            ImportWrite::Hide { id, origin } => {
                tx.execute(
                    "UPDATE facts SET hidden = 1, origin_ref = ?2 WHERE id = ?1",
                    params![id, origin],
                )?;
            }
            // After the inserts (the plan lists them first), so a target added by this
            // import exists. The plan checked the family; link_uids still refuses self
            // links, cycles and hidden facts.
            ImportWrite::Links { from, add, remove } => {
                for to in remove {
                    tx.execute(
                        "DELETE FROM fact_links WHERE from_uid = ?1 AND to_uid = ?2
                           AND kind = 'supersedes' AND source = 'repo'",
                        params![from, to],
                    )?;
                }
                for to in add {
                    super::truth::link_uids(
                        &tx,
                        from,
                        to,
                        super::truth::LinkKind::Supersedes,
                        super::truth::LinkSource::Repo,
                        None,
                        now,
                        false,
                    )?;
                }
            }
        }
    }
    record_checkout(
        &tx,
        root,
        &project,
        plan.present.then_some(sig.as_str()),
        &plan.borrowed,
    )?;
    tx.commit()?;
    Ok(plan.report)
}

/// The files' signatures (mtime and size, `-` when absent): an import is due when it moved.
fn signature(root: &Path) -> String {
    let dir = memory_dir(root);
    FILES
        .iter()
        .map(|(_, f)| match std::fs::symlink_metadata(dir.join(f)) {
            Ok(m) => {
                let mtime = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_nanos());
                format!("{f}:{mtime}:{}", m.len())
            }
            Err(_) => format!("{f}:-"),
        })
        .collect::<Vec<_>>()
        .join("|")
}

/// Whether `root` has imported repo memory before (it is in [`IMPORT_STATE`]). `cal memory
/// import` on a checkout whose memory directory is gone hides the imported facts when this is
/// true, and reports "no repo memory" when it is not.
pub fn is_known_checkout(conn: &Connection, root: &Path) -> bool {
    load_state(conn).contains_key(root.to_string_lossy().as_ref())
}

fn load_state(conn: &Connection) -> BTreeMap<String, String> {
    super::app_config::get(conn, IMPORT_STATE)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

type BorrowedMap = BTreeMap<String, BTreeMap<String, Vec<String>>>;

fn load_borrowed(conn: &Connection) -> BorrowedMap {
    super::app_config::get(conn, BORROWED)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// The uids `project` reads from another clone of the same repo (see [`BORROWED`]), sorted.
/// One `app_config` read; empty on any error (an index from before this table, a hook's
/// read-only connection).
pub fn borrowed_uids(conn: &Connection, project: &str) -> Vec<String> {
    let mut out: Vec<String> = load_borrowed(conn)
        .remove(project)
        .map(|m| m.into_values().flatten().collect())
        .unwrap_or_default();
    out.sort();
    out.dedup();
    out
}

/// Record an imported checkout: its signature (None: the directory is gone, so forget it)
/// and the uids it borrows. Writes only what changed.
fn record_checkout(
    conn: &Connection,
    root: &Path,
    project: &str,
    sig: Option<&str>,
    borrowed: &[String],
) -> Result<()> {
    let key = root.to_string_lossy().into_owned();
    let mut state = load_state(conn);
    let before = state.get(&key).cloned();
    match sig {
        Some(s) => state.insert(key.clone(), s.to_string()),
        None => state.remove(&key),
    };
    if state.get(&key).cloned() != before {
        super::app_config::put(conn, IMPORT_STATE, &serde_json::to_string(&state)?)?;
    }
    let mut map = load_borrowed(conn);
    let before = map.get(project).and_then(|m| m.get(&key)).cloned();
    let after = (!borrowed.is_empty()).then(|| borrowed.to_vec());
    if before != after {
        let entry = map.entry(project.to_string()).or_default();
        match after {
            Some(uids) => entry.insert(key, uids),
            None => entry.remove(&key),
        };
        map.retain(|_, m| !m.is_empty());
        super::app_config::put(conn, BORROWED, &serde_json::to_string(&map)?)?;
    }
    Ok(())
}

/// Import the checkout at `root` when its files changed since the last import here, or when
/// its memory directory disappeared since (which hides its facts). None when there is
/// nothing to do. A refused path (a symlink) is an error once, then None until it changes.
pub fn import_if_changed(conn: &Connection, root: &Path, now: i64) -> Result<Option<ImportReport>> {
    let key = root.to_string_lossy().into_owned();
    let state = load_state(conn);
    let known = state.get(&key);
    let present = match memory_dir_state(root) {
        Ok(p) => p,
        Err(e) => {
            if known.map(String::as_str) == Some(REFUSED_STATE) {
                return Ok(None);
            }
            let mut state = state;
            state.insert(key, REFUSED_STATE.to_string());
            super::app_config::put(conn, IMPORT_STATE, &serde_json::to_string(&state)?)?;
            return Err(e);
        }
    };
    if !present && known.is_none() {
        return Ok(None);
    }
    if present && known == Some(&signature(root)) {
        return Ok(None);
    }
    import_repo(conn, root, now, false).map(Some)
}

/// After an index pass or batch: import the repo memory of every checkout whose threads it
/// wrote (`last_indexed_at` at or after `since`, the newest 50, as the commit linker reads
/// them), and of the ones imported before whose directory has since gone. A checkout
/// without `.callimachus/memory/` costs a stat; an unchanged one two more.
/// Returns how many facts changed. Never fails the caller: errors are logged.
pub fn import_since(conn: &Connection, since: i64, now: i64) -> usize {
    let dirs: Vec<String> = match (|| -> rusqlite::Result<Vec<String>> {
        let mut stmt = conn.prepare_cached(crate::jobs::BATCH_THREADS_SQL)?;
        let rows = stmt
            .query_map(params![since, 50], |r| {
                let path: String = r.get(1)?;
                let key: String = r.get(2)?;
                Ok(if path.is_empty() { key } else { path })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })() {
        Ok(d) => d,
        Err(e) => {
            app_log!("[repo-memory] batch threads: {e:#}");
            return 0;
        }
    };
    let state = load_state(conn);
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for d in dirs {
        if d.is_empty() || !seen.insert(d.clone()) {
            continue;
        }
        if let Some(root) = checkout_root(Path::new(&d)) {
            let known = state.contains_key(root.to_string_lossy().as_ref());
            if (known || memory_dir(&root).is_dir()) && !roots.contains(&root) {
                roots.push(root);
            }
        }
    }
    let mut changed = 0;
    for root in roots {
        match import_if_changed(conn, &root, now) {
            Ok(Some(r)) if r.changed() => {
                let n = r.added.len() + r.updated.len() + r.hidden.len() + r.restored.len();
                app_log!(
                    "[repo-memory] {}: {} added, {} updated, {} hidden, {} restored",
                    r.root,
                    r.added.len(),
                    r.updated.len(),
                    r.hidden.len(),
                    r.restored.len()
                );
                changed += n;
            }
            Ok(_) => {}
            Err(e) if crate::db::is_busy(&e) => {}
            Err(e) => app_log!("[repo-memory] import {}: {e:#}", root.display()),
        }
    }
    changed
}

// ---- status ----

/// The Projects page's repo memory line for `project` (a project key): the checkout, whether
/// it has repo memory, which local facts it shares, how many it imported (its own and the
/// ones it reads from another clone of the repo).
pub fn status(conn: &Connection, project: &str) -> Result<RepoMemoryStatus> {
    let Some(root) = checkout_root(Path::new(project)) else {
        return Ok(RepoMemoryStatus::default());
    };
    let dir = memory_dir(&root);
    // A refused path (a symlink) reads as nothing here; the publish preview names it.
    let published = memory_dir_state(&root).unwrap_or(false);
    let mut uids: HashSet<String> = HashSet::new();
    if published {
        for (_, file) in FILES {
            if let Ok(Some((lines, _))) = read_file(&dir.join(file), false) {
                uids.extend(lines.into_iter().map(|l| l.fact.uid));
            }
        }
    }
    let rows = local_rows(conn, project)?;
    let mut shared_ids: Vec<i64> = rows
        .iter()
        .filter(|r| r.extractor != EXTRACTOR_REPO && !r.hidden && uids.contains(&r.uid))
        .map(|r| r.id)
        .collect();
    let mut imported = rows
        .iter()
        .filter(|r| r.extractor == EXTRACTOR_REPO && !r.hidden)
        .count() as i64;
    let borrowed = borrowed_uids(conn, project);
    if !borrowed.is_empty() {
        let mut stmt = conn.prepare_cached(BORROWED_ROWS_SQL)?;
        let rows = stmt
            .query_map([serde_json::to_string(&borrowed)?], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, uid, extractor) in rows {
            if extractor == EXTRACTOR_REPO {
                imported += 1;
            } else if uids.contains(&uid) {
                shared_ids.push(id);
            }
        }
    }
    shared_ids.sort_unstable();
    shared_ids.dedup();
    Ok(RepoMemoryStatus {
        root: Some(root.to_string_lossy().into_owned()),
        published,
        shared_ids,
        imported,
    })
}

/// The visible facts of a JSON array of borrowed uids, off idx_facts_uid (no kind filter:
/// the uids come from the decision and gotcha files, and one would pull the plan onto
/// idx_facts_kind).
pub(crate) const BORROWED_ROWS_SQL: &str = "SELECT f.id, f.uid, f.extractor FROM facts f
     WHERE f.uid IN (SELECT value FROM json_each(?1)) AND f.hidden = 0";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::{record_fact_with_origin, FactOrigin};

    const KEY: &str = "sk-ant-api03-AbCdEfGhIjKlMnOpQrStUv0123456789";

    fn temp_db() -> Connection {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "calli_repomem_{}_{}.db",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        crate::db::open(&p).unwrap()
    }

    /// A git checkout (just a `.git` dir: nothing here runs git) and its project key.
    fn checkout(tag: &str) -> (PathBuf, String) {
        let dir = std::env::temp_dir().join(format!("calli_repomem_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let root = std::fs::canonicalize(&dir).unwrap();
        let key = crate::knowledge::project_key(&root.to_string_lossy());
        (root, key)
    }

    fn record(conn: &Connection, project: &str, kind: &str, text: &str, origin: FactOrigin) -> i64 {
        record_fact_with_origin(conn, project, kind, text, None, origin, None, 1_000)
            .unwrap()
            .id
    }

    /// A distilled fact on a real session thread of `project` running in `path`.
    fn distilled(conn: &Connection, project: &str, path: &str, kind: &str, text: &str) -> i64 {
        let sid = crate::indexer::source_id(conn, "claude_code").unwrap();
        let ext = format!("s-{text}");
        conn.execute(
            "INSERT INTO threads (source_id, external_id, title, project_path, project_key,
                                  created_at, updated_at, message_count)
             VALUES (?1, ?2, 'a session', ?3, ?4, 1, 1, 6)",
            params![sid, ext, path, project],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO facts (thread_id, kind, text, extractor, created_at, uid)
             VALUES (?1, ?2, ?3, 'llm', 2000, ?4)",
            params![tid, kind, text, crate::knowledge::new_fact_uid()],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn publish_here(conn: &Connection, root: &Path, key: &str, all: bool) -> PublishPlan {
        let plan = plan_publish_with(conn, root, key, Some("github.com/acme/app"), all).unwrap();
        apply_publish(&plan).unwrap();
        plan
    }

    fn file(root: &Path, name: &str) -> String {
        std::fs::read_to_string(memory_dir(root).join(name)).unwrap_or_default()
    }

    fn uid_of(conn: &Connection, id: i64) -> String {
        conn.query_row("SELECT uid FROM facts WHERE id = ?1", [id], |r| r.get(0))
            .unwrap()
    }

    /// (text, hidden, pinned, origin_ref) of the imported facts, by text.
    fn imported(conn: &Connection) -> Vec<(String, bool, bool, String)> {
        let mut stmt = conn
            .prepare(
                "SELECT text, hidden, pinned, origin_ref FROM facts
                 WHERE extractor = 'repo' ORDER BY text",
            )
            .unwrap();
        stmt.query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get::<_, i64>(1)? != 0,
                r.get::<_, i64>(2)? != 0,
                r.get(3)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
    }

    /// A reversal travels with the files: publish writes the newer line's `supersedes`, an
    /// import links it (source `repo`, no downgrade), and the preview shows a change of the
    /// field alone, both ways.
    #[test]
    fn supersedes_round_trips_through_repo_memory() {
        use crate::knowledge::truth::{fact_truth, FactStatus, LinkKind, LinkSource};
        let (root, key) = checkout("supersedes");
        let a = temp_db();
        let old = record(&a, &key, "decision", "Cache with bincode", FactOrigin::User);
        let new = record(&a, &key, "decision", "Cache with JSON", FactOrigin::User);
        publish_here(&a, &root, &key, false);
        assert!(!file(&root, "decisions.jsonl").contains("supersedes"));

        // Only the link changed: the preview lists the newer line as updated.
        crate::knowledge::link_facts(
            &a,
            new,
            old,
            LinkKind::Supersedes,
            LinkSource::User,
            None,
            2,
        )
        .unwrap();
        let plan = plan_publish_with(&a, &root, &key, Some("github.com/acme/app"), false).unwrap();
        assert!(plan.changed);
        assert_eq!(plan.updated.len(), 1);
        assert_eq!(plan.updated[0].uid, uid_of(&a, new));
        apply_publish(&plan).unwrap();
        let line: RepoFact = serde_json::from_str(
            file(&root, "decisions.jsonl")
                .lines()
                .find(|l| l.contains(&uid_of(&a, new)))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(line.supersedes, vec![uid_of(&a, old)]);

        // Another machine imports both and the link; its memory shows only the newer one.
        let b = temp_db();
        let r = import_repo(&b, &root, 5_000, false).unwrap();
        assert_eq!(r.added.len(), 2);
        let links: Vec<(String, String, String)> = b
            .prepare("SELECT from_uid, to_uid, source FROM fact_links")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            links,
            vec![(uid_of(&a, new), uid_of(&a, old), "repo".into())]
        );
        let mem = crate::knowledge::get_project_memory(&b, &key, 20).unwrap();
        let texts: Vec<&str> = mem.decisions.iter().map(|f| f.text.as_str()).collect();
        assert_eq!(texts, ["Cache with JSON"]);
        assert!(!import_repo(&b, &root, 5_000, false).unwrap().changed());

        // The publisher undoes the reversal: the field goes, and the import unlinks.
        crate::knowledge::unlink_facts(&a, new, old, None, 4_000).unwrap();
        let plan = publish_here(&a, &root, &key, false);
        assert_eq!(plan.updated.len(), 1);
        assert!(!file(&root, "decisions.jsonl").contains("supersedes"));
        let preview = plan_import(&b, &root).unwrap();
        assert_eq!(preview.updated.len(), 1, "{preview:?}");
        import_repo(&b, &root, 5_000, false).unwrap();
        let old_uid = uid_of(&a, old);
        assert_eq!(
            fact_truth(&b, &[&old_uid]).unwrap()[&old_uid].status,
            FactStatus::Current
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A line's `supersedes` that this index already satisfies another way (the user linked
    /// the pair), or that the link writer would refuse (the target is hidden here), plans no
    /// write: the line does not show as updated on every import.
    #[test]
    fn imported_supersedes_already_satisfied_or_refused_change_nothing() {
        use crate::knowledge::truth::{LinkKind, LinkSource};
        let (root, key) = checkout("supersedes_settled");
        let a = temp_db();
        let old = record(&a, &key, "decision", "Cache with bincode", FactOrigin::User);
        let new = record(&a, &key, "decision", "Cache with JSON", FactOrigin::User);
        crate::knowledge::link_facts(
            &a,
            new,
            old,
            LinkKind::Supersedes,
            LinkSource::User,
            None,
            2,
        )
        .unwrap();
        publish_here(&a, &root, &key, false);
        let b = temp_db();
        import_repo(&b, &root, 5_000, false).unwrap();
        let id_in_b = |id| -> i64 {
            b.query_row(
                "SELECT id FROM facts WHERE uid = ?1",
                [uid_of(&a, id)],
                |r| r.get(0),
            )
            .unwrap()
        };
        let (old_b, new_b) = (id_in_b(old), id_in_b(new));
        let unchanged = |why: &str| {
            let preview = plan_import(&b, &root).unwrap();
            assert!(preview.updated.is_empty(), "{why}: {preview:?}");
            for _ in 0..2 {
                assert!(
                    !import_repo(&b, &root, 5_000, false).unwrap().changed(),
                    "{why}"
                );
            }
        };

        // The user's own link for the pair satisfies the line and is left theirs.
        b.execute("DELETE FROM fact_links", []).unwrap();
        crate::knowledge::link_facts(
            &b,
            new_b,
            old_b,
            LinkKind::Supersedes,
            LinkSource::User,
            None,
            6_000,
        )
        .unwrap();
        unchanged("user link");
        let source: String = b
            .query_row("SELECT source FROM fact_links", [], |r| r.get(0))
            .unwrap();
        assert_eq!(source, "user");

        // The target hidden here (how a replaced decision was retired before links): the
        // link is refused, so it is not planned again and again.
        b.execute("DELETE FROM fact_links", []).unwrap();
        crate::knowledge::set_fact_hidden(&b, old_b, true).unwrap();
        unchanged("hidden target");
        crate::knowledge::set_fact_hidden(&b, old_b, false).unwrap();

        // The user undid a link for the pair that is not the line's own row: the reverse
        // `supersedes`, or a `contradicts` they settled as "both stand". The import is
        // refused for a dismissed pair, so it must not plan the link on every run either.
        let undone = |from: i64, to: i64, kind: LinkKind| {
            b.execute("DELETE FROM fact_links", []).unwrap();
            let id = match crate::knowledge::link_facts(
                &b,
                from,
                to,
                kind,
                LinkSource::User,
                None,
                6_000,
            )
            .unwrap()
            {
                crate::knowledge::truth::LinkOutcome::Linked { id, .. } => id,
                other => panic!("{other:?}"),
            };
            assert!(crate::knowledge::truth::unlink_link(&b, id, 7_000).unwrap());
        };
        undone(old_b, new_b, LinkKind::Supersedes);
        unchanged("reverse link undone");
        undone(new_b, old_b, LinkKind::Contradicts);
        unchanged("both stand");
        undone(old_b, new_b, LinkKind::Contradicts);
        unchanged("both stand, other direction");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A line cannot reach a fact outside its repo, nor link a fact to itself.
    #[test]
    fn imported_supersedes_ignore_uids_outside_the_repo() {
        let (root, key) = checkout("supersedes_hostile");
        let conn = temp_db();
        let elsewhere = record(
            &conn,
            "/other/repo",
            "decision",
            "Elsewhere",
            FactOrigin::User,
        );
        let target = uid_of(&conn, elsewhere);
        std::fs::create_dir_all(memory_dir(&root)).unwrap();
        let line = RepoFact {
            uid: "11111111-2222-4333-8444-555555555555".into(),
            kind: "decision".into(),
            text: "Hostile line".into(),
            rationale: None,
            pinned: false,
            by: "user".into(),
            created_at: 1,
            project: None,
            subpath: None,
            supersedes: vec![
                target.clone(),
                "11111111-2222-4333-8444-555555555555".into(),
                "../../etc".into(),
            ],
        };
        std::fs::write(
            memory_dir(&root).join("decisions.jsonl"),
            format!("{}\n", serde_json::to_string(&line).unwrap()),
        )
        .unwrap();
        let r = import_repo(&conn, &root, 100, false).unwrap();
        assert_eq!(r.added.len(), 1);
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM fact_links", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        let _ = (key, std::fs::remove_dir_all(&root));
    }

    #[test]
    fn publish_writes_sorted_redacted_lines_and_round_trips_into_another_index() {
        let (root, key) = checkout("roundtrip");
        let a = temp_db();
        let u = record(
            &a,
            &key,
            "decision",
            &format!("Deploy with {KEY}"),
            FactOrigin::User,
        );
        let g = record(&a, &key, "gotcha", "Port 1420 is fixed", FactOrigin::Agent);
        record(&a, &key, "decision", "Second decision", FactOrigin::Agent);
        a.execute(
            "UPDATE facts SET rationale = ?2 WHERE id = ?1",
            params![u, format!("because {KEY}")],
        )
        .unwrap();
        // Distilled facts go out only with `all`.
        distilled(
            &a,
            &key,
            &format!("{key}/apps/web"),
            "gotcha",
            "Web tests need a build first",
        );
        // Claude Code auto memory never does, pinned or not.
        let cm = record(&a, &key, "gotcha", "User prefers tabs", FactOrigin::User);
        a.execute(
            "UPDATE facts SET extractor = 'claude-memory' WHERE id = ?1",
            [cm],
        )
        .unwrap();

        let plan = publish_here(&a, &root, &key, false);
        assert!(plan.changed);
        assert_eq!((plan.decisions, plan.gotchas, plan.added.len()), (2, 1, 3));
        let decisions = file(&root, "decisions.jsonl");
        let gotchas = file(&root, "gotchas.jsonl");
        assert!(
            !decisions.contains(KEY) && decisions.contains("[REDACTED"),
            "{decisions}"
        );
        assert!(!gotchas.contains("tabs") && !decisions.contains("tabs"));
        assert!(!gotchas.contains("Web tests"));
        assert!(memory_dir(&root).join("README.md").exists());
        // One object per line, sorted by uid, every field present but the subpath.
        let uids: Vec<String> = decisions
            .lines()
            .map(|l| serde_json::from_str::<RepoFact>(l).unwrap().uid)
            .collect();
        let mut sorted = uids.clone();
        sorted.sort();
        assert_eq!(uids, sorted);
        let first: serde_json::Value =
            serde_json::from_str(gotchas.lines().next().unwrap()).unwrap();
        assert_eq!(first["uid"], uid_of(&a, g));
        assert_eq!(first["by"], "agent");
        assert_eq!(first["pinned"], false);
        assert_eq!(first["rationale"], serde_json::Value::Null);
        assert_eq!(first["project"], "github.com/acme/app");
        assert!(first.get("subpath").is_none());
        let user: serde_json::Value = serde_json::from_str(
            decisions
                .lines()
                .find(|l| l.contains(&uid_of(&a, u)))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            (user["by"].as_str(), user["pinned"].as_bool()),
            (Some("user"), Some(true))
        );

        // Publishing again changes nothing.
        let again = plan_publish_with(&a, &root, &key, Some("github.com/acme/app"), false).unwrap();
        assert!(!again.changed && again.added.is_empty() && again.updated.is_empty());

        // `all` adds the distilled gotcha with where its session ran.
        let all = publish_here(&a, &root, &key, true);
        assert_eq!((all.added.len(), all.gotchas), (1, 2));
        assert!(file(&root, "gotchas.jsonl").contains(r#""subpath":"apps/web""#));

        // Another machine imports them: never pinned, origin names the file.
        let b = temp_db();
        let r = import_repo(&b, &root, 5_000, false).unwrap();
        assert_eq!((r.added.len(), r.shared, r.skipped), (4, 0, 0));
        let got = imported(&b);
        assert_eq!(got.len(), 4);
        assert!(got.iter().all(|(_, hidden, pinned, _)| !hidden && !pinned));
        assert!(got.iter().any(|(t, _, _, o)| t == "Port 1420 is fixed"
            && o == "repo:.callimachus/memory/gotchas.jsonl"));
        // The same uids, in the project's memory, from the repo.
        let mem = crate::knowledge::get_project_memory(&b, &key, 20).unwrap();
        assert_eq!(mem.decisions.len() + mem.gotchas.len(), 4);
        assert!(mem.gotchas.iter().all(|f| f.extractor == EXTRACTOR_REPO));
        let uid_b: String = b
            .query_row(
                "SELECT uid FROM facts WHERE text = 'Port 1420 is fixed'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(uid_b, uid_of(&a, g));
        // Importing the same files again changes nothing; on the publisher they are shared.
        assert!(!import_repo(&b, &root, 5_000, false).unwrap().changed());
        let own = import_repo(&a, &root, 5_000, false).unwrap();
        assert_eq!((own.added.len(), own.shared), (0, 4));
        let st = status(&a, &key).unwrap();
        assert_eq!(st.shared_ids.len(), 4);
        assert!(st.published && st.imported == 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn merge_by_uid_keeps_unknown_lines_and_removes_only_ours() {
        let (root, key) = checkout("merge");
        let a = temp_db();
        let mine = record(&a, &key, "decision", "Mine", FactOrigin::User);
        publish_here(&a, &root, &key, false);
        // A teammate's line arrives with a pull, before any import here.
        let theirs = r#"{"uid":"00000000-0000-4000-8000-000000000001","kind":"decision","text":"Theirs","rationale":null,"pinned":false,"by":"user","created_at":5,"project":null,"future":1}"#;
        let path = memory_dir(&root).join("decisions.jsonl");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(theirs);
        text.push('\n');
        std::fs::write(&path, text).unwrap();

        // Hiding mine and publishing removes my line and keeps theirs verbatim (with the
        // field this version does not know), sorted in.
        crate::knowledge::set_fact_hidden(&a, mine, true).unwrap();
        let plan = publish_here(&a, &root, &key, false);
        assert_eq!((plan.removed.len(), plan.added.len()), (1, 0));
        assert_eq!(file(&root, "decisions.jsonl"), format!("{theirs}\n"));

        // An edit here of my own fact is an update of its line.
        crate::knowledge::set_fact_hidden(&a, mine, false).unwrap();
        publish_here(&a, &root, &key, false);
        crate::knowledge::edit_fact(&a, mine, "Mine, reworded").unwrap();
        let plan = publish_here(&a, &root, &key, false);
        assert_eq!(plan.updated.len(), 1);
        let body = file(&root, "decisions.jsonl");
        assert!(body.contains("Mine, reworded") && body.contains("Theirs"));
        // Sorted by uid: the teammate's all-zero uid comes first.
        assert!(body.starts_with(theirs));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_local_edit_of_an_imported_fact_wins_and_removal_hides_until_the_line_returns() {
        let (root, key) = checkout("edit");
        let dir = memory_dir(&root);
        std::fs::create_dir_all(&dir).unwrap();
        let line = |uid: &str, text: &str| {
            format!(
                r#"{{"uid":"{uid}","kind":"gotcha","text":"{text}","rationale":null,"pinned":true,"by":"user","created_at":10,"project":null}}"#
            )
        };
        let both = format!("{}\n{}\n", line("a1", "First"), line("b2", "Second"));
        std::fs::write(dir.join("gotchas.jsonl"), &both).unwrap();
        let conn = temp_db();
        let r = import_repo(&conn, &root, 100, false).unwrap();
        assert_eq!(r.added.len(), 2);
        // Pinned in the file, never pinned on import.
        assert!(imported(&conn).iter().all(|(_, _, pinned, _)| !pinned));

        // The user rewords the first; the file then changes it too: the local edit wins.
        let id: i64 = conn
            .query_row("SELECT id FROM facts WHERE uid = 'a1'", [], |r| r.get(0))
            .unwrap();
        crate::knowledge::edit_fact(&conn, id, "First, my way").unwrap();
        std::fs::write(
            dir.join("gotchas.jsonl"),
            format!(
                "{}\n{}\n",
                line("a1", "First, their way"),
                line("b2", "Second v2")
            ),
        )
        .unwrap();
        let r = import_repo(&conn, &root, 100, false).unwrap();
        assert_eq!(r.updated.len(), 1, "only the unedited one");
        let texts: Vec<String> = imported(&conn).into_iter().map(|f| f.0).collect();
        assert_eq!(texts, ["First, my way", "Second v2"]);
        // And publishing from here writes the edit, keeping the publisher's pin and date.
        let plan = publish_here(&conn, &root, &key, false);
        assert_eq!((plan.updated.len(), plan.added.len()), (1, 0));
        let body = file(&root, "gotchas.jsonl");
        assert!(
            body.contains(r#""text":"First, my way","rationale":null,"pinned":true"#),
            "{body}"
        );
        assert!(body.contains("Second v2"));

        // A line removed from the file hides its imported copy...
        std::fs::write(
            dir.join("gotchas.jsonl"),
            format!("{}\n", line("a1", "First, my way")),
        )
        .unwrap();
        let r = import_repo(&conn, &root, 100, false).unwrap();
        assert_eq!(r.hidden.len(), 1);
        assert!(crate::knowledge::get_project_memory(&conn, &key, 10)
            .unwrap()
            .gotchas
            .iter()
            .all(|f| f.text != "Second v2"));
        // ...and a branch that brings it back restores it.
        std::fs::write(dir.join("gotchas.jsonl"), &both).unwrap();
        let r = import_repo(&conn, &root, 100, false).unwrap();
        assert_eq!(r.restored.len(), 1);
        // A copy the user hid stays hidden, and publishing keeps the teammate's line: a
        // hide is local, and only the user's own lines are removed.
        let id: i64 = conn
            .query_row("SELECT id FROM facts WHERE uid = 'b2'", [], |r| r.get(0))
            .unwrap();
        crate::knowledge::set_fact_hidden(&conn, id, true).unwrap();
        assert!(!import_repo(&conn, &root, 100, false)
            .unwrap()
            .restored
            .iter()
            .any(|c| c.uid == "b2"));
        assert!(imported(&conn)
            .iter()
            .any(|(t, hidden, _, _)| t == "Second" && *hidden));
        let plan = publish_here(&conn, &root, &key, false);
        assert!(plan.removed.is_empty(), "{:?}", plan.removed);
        assert!(file(&root, "gotchas.jsonl").contains("b2"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The id, text and hidden flag of the fact with `uid`.
    fn by_uid(conn: &Connection, uid: &str) -> (i64, String, bool) {
        conn.query_row(
            "SELECT id, text, hidden FROM facts WHERE uid = ?1",
            [uid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0)),
        )
        .unwrap()
    }

    fn gotcha_line(uid: &str, text: &str) -> String {
        format!(r#"{{"uid":"{uid}","kind":"gotcha","text":"{text}"}}"#)
    }

    fn write_gotchas(root: &Path, lines: &[String]) {
        let dir = memory_dir(root);
        std::fs::create_dir_all(&dir).unwrap();
        let mut body = lines.join("\n");
        body.push('\n');
        std::fs::write(dir.join("gotchas.jsonl"), body).unwrap();
    }

    #[test]
    fn a_fact_the_user_hid_stays_hidden_and_unchanged_through_removal_and_return() {
        let (root, _) = checkout("hidden");
        let conn = temp_db();
        let both = [gotcha_line("a1", "First"), gotcha_line("b2", "Second")];
        write_gotchas(&root, &both);
        import_repo(&conn, &root, 100, false).unwrap();
        let (b2, _, _) = by_uid(&conn, "b2");
        crate::knowledge::set_fact_hidden(&conn, b2, true).unwrap();
        // The file rewords it: a hidden copy is not rewritten.
        write_gotchas(
            &root,
            &[gotcha_line("a1", "First"), gotcha_line("b2", "Second v2")],
        );
        let r = import_repo(&conn, &root, 100, false).unwrap();
        assert!(r.updated.is_empty(), "{:?}", r.updated);
        assert_eq!(by_uid(&conn, "b2").1, "Second");
        // A branch without the line, then back: still hidden.
        write_gotchas(&root, &[gotcha_line("a1", "First")]);
        let r = import_repo(&conn, &root, 100, false).unwrap();
        assert!(r.hidden.is_empty(), "already hidden, so not hidden again");
        write_gotchas(&root, &both);
        let r = import_repo(&conn, &root, 100, false).unwrap();
        assert!(r.restored.is_empty());
        assert!(
            by_uid(&conn, "b2").2,
            "the user's hide survives the round trip"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_file_or_directory_hides_its_facts_and_a_read_error_hides_nothing() {
        let (root, _) = checkout("missing");
        let conn = temp_db();
        write_gotchas(&root, &[gotcha_line("g1", "Team gotcha")]);
        let dir = memory_dir(&root);
        std::fs::write(
            dir.join("decisions.jsonl"),
            "{\"uid\":\"d1\",\"kind\":\"decision\",\"text\":\"Team decision\"}\n",
        )
        .unwrap();
        assert_eq!(
            import_if_changed(&conn, &root, 1)
                .unwrap()
                .unwrap()
                .added
                .len(),
            2
        );
        // An unreadable file (not UTF-8) hides nothing, in either file.
        std::fs::write(dir.join("gotchas.jsonl"), [0xff, 0xfe, b'\n']).unwrap();
        let r = import_if_changed(&conn, &root, 1).unwrap().unwrap();
        assert!(r.hidden.is_empty(), "{:?}", r.hidden);
        // `git rm gotchas.jsonl`: its fact is hidden, the other file's stays.
        std::fs::remove_file(dir.join("gotchas.jsonl")).unwrap();
        let r = import_if_changed(&conn, &root, 1).unwrap().unwrap();
        assert_eq!(
            r.hidden.iter().map(|c| c.uid.as_str()).collect::<Vec<_>>(),
            ["g1"]
        );
        assert!(!by_uid(&conn, "d1").2);
        // A branch without the directory: the rest is hidden, and the checkout forgotten.
        std::fs::remove_dir_all(root.join(".callimachus")).unwrap();
        let r = import_if_changed(&conn, &root, 1).unwrap().unwrap();
        assert_eq!(r.hidden.len(), 1);
        assert!(by_uid(&conn, "d1").2);
        assert!(import_if_changed(&conn, &root, 1).unwrap().is_none());
        assert!(!load_state(&conn).contains_key(root.to_string_lossy().as_ref()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn two_clones_of_one_repo_share_its_facts_and_hide_only_what_neither_has() {
        let (a, key_a) = checkout("clone_a");
        let (b, key_b) = checkout("clone_b");
        let conn = temp_db();
        // Both clones resolved to one remote, as the watcher's linker caches it.
        for k in [&key_a, &key_b] {
            conn.execute(
                "INSERT INTO projects (key, portable_id, refreshed_at) VALUES (?1, ?2, ?3)",
                params![k, "github.com/acme/app", chrono::Utc::now().timestamp()],
            )
            .unwrap();
        }
        // The user's own fact, published from clone A.
        let own = record(&conn, &key_a, "decision", "Own decision", FactOrigin::User);
        publish_here(&conn, &a, &key_a, false);
        let team = [gotcha_line("g1", "Kept"), gotcha_line("g2", "Dropped")];
        write_gotchas(&a, &team);
        assert_eq!(import_repo(&conn, &a, 1, false).unwrap().added.len(), 2);

        // Clone B: the same files. Nothing new to add (the uids are taken), yet all of it
        // is this project's memory too; the user's own line counts as shared.
        std::fs::create_dir_all(memory_dir(&b)).unwrap();
        std::fs::copy(
            memory_dir(&a).join("decisions.jsonl"),
            memory_dir(&b).join("decisions.jsonl"),
        )
        .unwrap();
        write_gotchas(&b, &team);
        let r = import_repo(&conn, &b, 1, false).unwrap();
        assert_eq!((r.added.len(), r.shared, r.skipped), (0, 1, 0));
        let mem = crate::knowledge::get_project_memory_facts(&conn, &key_b, 10).unwrap();
        assert_eq!((mem.decisions.len(), mem.gotchas.len()), (1, 2));
        let st = status(&conn, &key_b).unwrap();
        assert_eq!((st.imported, st.shared_ids.clone()), (2, vec![own]));

        // B drops g2: A still has it, so it stays.
        write_gotchas(&b, &[gotcha_line("g1", "Kept")]);
        assert!(import_repo(&conn, &b, 1, false).unwrap().hidden.is_empty());
        assert!(!by_uid(&conn, "g2").2);
        // A drops it too: now no checkout has it.
        write_gotchas(&a, &[gotcha_line("g1", "Kept")]);
        let r = import_repo(&conn, &a, 1, false).unwrap();
        assert_eq!(r.hidden.len(), 1);
        assert!(by_uid(&conn, "g2").2);
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn plan_import_reads_only() {
        let (root, _) = checkout("plan_ro");
        write_gotchas(&root, &[gotcha_line("g1", "Team gotcha")]);
        let p = std::env::temp_dir().join(format!("calli_repomem_ro_{}.db", std::process::id()));
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        drop(crate::db::open(&p).unwrap());
        let ro = crate::db::open_readonly(&p).unwrap();
        let r = plan_import(&ro, &root).unwrap();
        assert_eq!(r.added.len(), 1);
        let n: i64 = ro
            .query_row(
                "SELECT COUNT(*) FROM facts WHERE extractor = 'repo'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn crlf_files_and_blank_lines_are_not_a_change() {
        let (root, key) = checkout("crlf");
        let conn = temp_db();
        record(&conn, &key, "decision", "One", FactOrigin::User);
        record(&conn, &key, "decision", "Two", FactOrigin::User);
        publish_here(&conn, &root, &key, false);
        let path = memory_dir(&root).join("decisions.jsonl");
        let crlf = std::fs::read_to_string(&path)
            .unwrap()
            .replace('\n', "\r\n")
            + "\r\n";
        std::fs::write(&path, &crlf).unwrap();
        let plan =
            plan_publish_with(&conn, &root, &key, Some("github.com/acme/app"), false).unwrap();
        assert!(!plan.changed, "{:?}", plan.writes);
        // An order change is one: the file goes back to sorted.
        let mut lines: Vec<&str> = crlf.lines().filter(|l| !l.is_empty()).collect();
        lines.reverse();
        std::fs::write(&path, lines.join("\n")).unwrap();
        let plan =
            plan_publish_with(&conn, &root, &key, Some("github.com/acme/app"), false).unwrap();
        assert!(plan.changed && plan.added.is_empty() && plan.updated.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn playbooks_compaction_summaries_and_auto_memory_are_never_published() {
        let (root, key) = checkout("local_only");
        let conn = temp_db();
        for (extractor, text) in [
            (
                "playbook",
                "E0308 seen 3 times; fixed in abc1234, see thread 42",
            ),
            ("claude-compact", "Session gist"),
            ("claude-memory", "About the user"),
        ] {
            let id = record(&conn, &key, "gotcha", text, FactOrigin::User);
            conn.execute(
                "UPDATE facts SET extractor = ?2, pinned = 1 WHERE id = ?1",
                params![id, extractor],
            )
            .unwrap();
        }
        for all in [false, true] {
            let plan = plan_publish_with(&conn, &root, &key, None, all).unwrap();
            assert!(!plan.changed && plan.added.is_empty(), "all={all}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_in_the_memory_path_are_refused_on_read_and_write() {
        use std::os::unix::fs::symlink;
        let (root, key) = checkout("symlink");
        let conn = temp_db();
        record(&conn, &key, "decision", "Mine", FactOrigin::User);
        let dir = memory_dir(&root);
        std::fs::create_dir_all(&dir).unwrap();
        // A file that stats as 0 bytes and never ends: refused, not read.
        symlink("/dev/zero", dir.join("decisions.jsonl")).unwrap();
        let err = import_repo(&conn, &root, 1, false);
        assert!(err.is_ok() && imported(&conn).is_empty());
        let err = plan_publish_with(&conn, &root, &key, None, false).unwrap_err();
        assert!(format!("{err:#}").contains("symlink"), "{err:#}");
        assert!(status(&conn, &key).unwrap().shared_ids.is_empty());
        std::fs::remove_file(dir.join("decisions.jsonl")).unwrap();

        // A committed temp-file symlink aimed outside the checkout is never written through.
        let victim = root.with_file_name(format!("calli_repomem_victim_{}", std::process::id()));
        std::fs::write(&victim, "precious").unwrap();
        symlink(&victim, dir.join(".decisions.jsonl.callimachus-tmp")).unwrap();
        publish_here(&conn, &root, &key, false);
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious");
        let meta = std::fs::symlink_metadata(dir.join("decisions.jsonl")).unwrap();
        assert!(meta.file_type().is_file());
        assert!(file(&root, "decisions.jsonl").contains("Mine"));

        // A symlinked memory directory: both directions refuse, and nothing is created.
        let outside = root.with_file_name(format!("calli_repomem_out_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::remove_dir_all(root.join(".callimachus")).unwrap();
        symlink(&outside, root.join(".callimachus")).unwrap();
        assert!(import_repo(&conn, &root, 1, false).is_err());
        let err = plan_publish_with(&conn, &root, &key, None, false).unwrap_err();
        assert!(format!("{err:#}").contains("symlink"), "{err:#}");
        let plan = PublishPlan {
            root: root.to_string_lossy().into_owned(),
            changed: true,
            writes: vec![(dir.join("decisions.jsonl"), "x\n".into())],
            ..Default::default()
        };
        assert!(apply_publish(&plan).is_err());
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
        let _ = std::fs::remove_file(&victim);
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reads_stop_at_the_size_cap() {
        let (root, _) = checkout("cap");
        let dir = memory_dir(&root);
        std::fs::create_dir_all(&dir).unwrap();
        let big = dir.join("gotchas.jsonl");
        std::fs::write(&big, vec![b' '; MAX_FILE_BYTES as usize + 1]).unwrap();
        let err = read_capped(&big).unwrap_err();
        assert!(format!("{err:#}").contains("limit"), "{err:#}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn review_writes_nothing_and_bad_lines_are_skipped_or_refused() {
        let (root, key) = checkout("review");
        let dir = memory_dir(&root);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("decisions.jsonl"),
            "not json\n{\"uid\":\"x\",\"kind\":\"todo\",\"text\":\"t\"}\n\
             {\"uid\":\"ok-1\",\"kind\":\"decision\",\"text\":\"Fine\"}\n",
        )
        .unwrap();
        let conn = temp_db();
        let r = import_repo(&conn, &root, 100, true).unwrap();
        assert_eq!((r.added.len(), r.skipped), (1, 2));
        assert!(imported(&conn).is_empty(), "review rolled back");
        // Publish refuses the unreadable line rather than dropping it.
        let err = plan_publish_with(&conn, &root, &key, None, false).unwrap_err();
        assert!(format!("{err:#}").contains("decisions.jsonl:1"), "{err:#}");
        std::fs::write(dir.join("decisions.jsonl"), "<<<<<<< HEAD\n").unwrap();
        let err = plan_publish_with(&conn, &root, &key, None, false).unwrap_err();
        assert!(format!("{err:#}").contains("merge conflict"), "{err:#}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn nothing_to_publish_creates_nothing_and_import_runs_only_when_files_change() {
        let (root, key) = checkout("empty");
        let conn = temp_db();
        // Only auto memory and a distilled fact: nothing for a default publish.
        let cm = record(&conn, &key, "gotcha", "About the user", FactOrigin::User);
        conn.execute(
            "UPDATE facts SET extractor = 'claude-memory' WHERE id = ?1",
            [cm],
        )
        .unwrap();
        distilled(&conn, &key, &key, "decision", "Distilled");
        let plan = plan_publish_with(&conn, &root, &key, None, false).unwrap();
        assert!(!plan.changed && plan.added.is_empty());
        apply_publish(&plan).unwrap();
        assert!(!memory_dir(&root).exists());
        assert!(import_if_changed(&conn, &root, 1).unwrap().is_none());

        let dir = memory_dir(&root);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("gotchas.jsonl"),
            "{\"uid\":\"g1\",\"kind\":\"gotcha\",\"text\":\"G\"}\n",
        )
        .unwrap();
        let r = import_if_changed(&conn, &root, 1).unwrap().unwrap();
        assert_eq!(r.added.len(), 1);
        assert!(
            import_if_changed(&conn, &root, 1).unwrap().is_none(),
            "unchanged"
        );
        let st = status(&conn, &key).unwrap();
        assert_eq!((st.imported, st.shared_ids.len()), (1, 0));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn imported_facts_reach_the_hook_marked_with_the_repo_trust_line() {
        let (root, key) = checkout("hook");
        let dir = memory_dir(&root);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("gotchas.jsonl"),
            "{\"uid\":\"g1\",\"kind\":\"gotcha\",\"text\":\"Team gotcha\"}\n",
        )
        .unwrap();
        let conn = temp_db();
        import_repo(&conn, &root, 1, false).unwrap();
        let mem = crate::knowledge::get_project_memory_facts(&conn, &key, 10).unwrap();
        let md = crate::export::agent_memory_md_budgeted(
            &key,
            &mem,
            None,
            &crate::export::MemoryBudget::HOOK,
        );
        // Dated like every line, and marked `repo` where the date's provenance goes.
        assert!(md.contains("\n- Team gotcha ("), "{md}");
        assert!(md.ends_with(", repo)"), "{md}");
        assert!(md.contains(crate::export::REPO_TRUST_LINE), "{md}");
        let body = crate::export::agents_file_body(&conn, &key).unwrap();
        assert!(
            body.contains("Team gotcha"),
            "the AGENTS block includes the team's facts"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn import_queries_stay_on_their_indexes() {
        let conn = temp_db();
        let plan = |sql: &str| -> String {
            let n = sql.matches('?').count();
            let args: Vec<String> = (0..n).map(|_| "x".to_string()).collect();
            let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            stmt.query_map(rusqlite::params_from_iter(args.iter()), |r| {
                r.get::<_, String>(3)
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join(" | ")
        };
        let p = plan(BY_UID_SQL);
        assert!(p.contains("idx_facts_uid"), "{p}");
        let p = plan(BY_ORIGIN_SQL);
        assert!(p.contains("idx_facts_origin_ref"), "{p}");
        let p = plan(BORROWED_ROWS_SQL);
        assert!(p.contains("idx_facts_uid"), "{p}");
        let p = plan(LOCAL_ROWS_SQL);
        assert!(p.contains("SEARCH t USING INTEGER PRIMARY KEY"), "{p}");
    }
}
