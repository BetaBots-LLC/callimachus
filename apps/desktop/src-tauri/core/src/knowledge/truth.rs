//! Fact truth: which facts still hold. A later session can reverse a decision, disagree with
//! it, confirm it again or show it no longer holds; migration 0035 stores that next to the
//! facts (`fact_links`, `fact_signals`, both keyed by `facts.uid`) and this module is the one
//! place that reads and writes it.
//!
//! - Supersession is explicit, never inferred: only the distiller (from a session that plainly
//!   states it), the user, an agent (`record_decision` `replaces`), a repo memory import or a
//!   conflict review the user acted on create a link. Dates validate a declared direction and
//!   never decide one.
//! - History is kept: a superseded fact stays in `facts`; readers leave it out of injection and
//!   label it where they show it.
//! - Session time is the clock: [`DECIDED_AT_SQL`] dates a distilled fact by when its session
//!   started, not when the distiller happened to run (a backlog distills newest first).
//! - Machine-written links cannot silently remove what the user trusts: a `supersedes` from the
//!   distiller, an agent or a repo import whose target is pinned or edited is stored as
//!   `contradicts` ([`LinkOutcome::Downgraded`]), an agent flag or a distilled `retires` never
//!   hides a pinned or edited fact (the flag is stored and shown, the fact stays injected), and
//!   caps bound what one prompt-injected session can write.
//!
//! Readers that run on an index the app has not migrated yet (a newer `cal`, hook or MCP
//! server opens read-only and never migrates) check [`has_truth_tables`] first and fall back to
//! "everything is current".

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

/// Extractors whose facts come from a real session's transcript, so their date is the
/// session's start: `llm` (distilled) and `claude-compact` (a compaction summary of the
/// session). Everything else keeps its own `created_at`: recorded facts (`user`, `agent`) and
/// imports (`repo`, `claude-memory`, whose time is the file's) were written when they say, a
/// playbook when the fix landed, and a heuristic TODO already carries its message's time
/// (`todo_created_at`), which is finer than the session start.
pub const SESSION_EXTRACTORS: &[&str] = &["llm", "claude-compact"];

/// Whether a fact with this extractor came from a session transcript (see
/// [`SESSION_EXTRACTORS`]): its provenance can name the session's agent and thread.
pub fn is_session_extractor(extractor: &str) -> bool {
    SESSION_EXTRACTORS.contains(&extractor)
}

/// A fact's `decided_at` (epoch seconds) as a select expression over `facts f JOIN threads t`:
/// the session's start for a session fact, the fact's own time otherwise. A thread with no
/// (or a zero) start falls back to the fact's time. Every fact reader orders and dates by this.
pub const DECIDED_AT_SQL: &str = "(CASE WHEN f.extractor IN ('llm', 'claude-compact') \
     AND t.created_at > 0 THEN t.created_at ELSE f.created_at END)";

/// The latest a fact could have been decided, over `facts f JOIN threads t`: its session's
/// last activity for a session fact (a session that started Monday and ran to Friday can
/// reverse something decided on Wednesday), the fact's own time otherwise. Only the direction
/// check of a machine link reads it; ordering stays on [`DECIDED_AT_SQL`], so a long session
/// does not outrank newer ones for what it decided early.
const SESSION_END_SQL: &str = "(CASE WHEN f.extractor IN ('llm', 'claude-compact') \
     AND t.created_at > 0 THEN MAX(t.created_at, COALESCE(t.updated_at, 0)) \
     ELSE f.created_at END)";

/// Whether the newer side `$a` (a `facts` alias) of a `supersedes` link still stands over
/// what it replaced: it is visible, or the user hid it after it had itself left memory
/// (another visible fact supersedes it, or a later session retired it as stale). So in a chain
/// C supersedes B supersedes A, hiding B keeps A out: C still stands. Hiding a replacement that
/// was still in force (or one flagged `wrong`) revives what it replaced, as the user rejecting
/// the replacement. One step deep: chains are a handful long and hiding two links of one is
/// rare. Off idx_fact_links_to and idx_fact_signals_fact, only for a hidden `$a`.
macro_rules! newer_stands {
    ($a:literal) => {
        concat!(
            "(",
            $a,
            ".hidden = 0
              OR EXISTS (SELECT 1 FROM fact_links l2 JOIN facts n2 ON n2.uid = l2.from_uid
                   WHERE l2.to_uid = ",
            $a,
            ".uid AND l2.kind = 'supersedes'
                     AND l2.dismissed_at IS NULL AND n2.hidden = 0)
              OR EXISTS (SELECT 1 FROM fact_signals s2
                   WHERE s2.fact_uid = ",
            $a,
            ".uid AND s2.kind = 'stale'
                     AND s2.resolved_at IS NULL))"
        )
    };
}

/// [`newer_stands!`] over the `nf` alias, for callers that build their own link join.
pub(crate) const NEWER_STANDS_NF_SQL: &str = newer_stands!("nf");

/// `f` (a `facts` alias) has an active `supersedes` link pointing at it: one whose newer side
/// still exists and stands ([`NEWER_STANDS_NF_SQL`]), and that the user has not undone
/// (`dismissed_at`). Off idx_fact_links_to, then idx_facts_uid.
pub(crate) const SUPERSEDED_SQL: &str = concat!(
    "EXISTS (SELECT 1 FROM fact_links lk
         JOIN facts nf ON nf.uid = lk.from_uid
         WHERE lk.to_uid = f.uid AND lk.kind = 'supersedes' AND lk.dismissed_at IS NULL
           AND ",
    newer_stands!("nf"),
    ")"
);

/// `f` has an unresolved `stale` or `wrong` flag. Off idx_fact_signals_fact.
pub(crate) const FLAGGED_SQL: &str = "EXISTS (SELECT 1 FROM fact_signals sg
         WHERE sg.fact_uid = f.uid AND sg.kind IN ('stale', 'wrong') AND sg.resolved_at IS NULL)";

/// The newest session start among the other threads that reaffirmed `f` (NULL when none).
/// Must agree with [`fact_truth`]'s `last_confirmed`: a reaffirmation whose thread was deleted
/// (thread_id set NULL) no longer counts, else its distill wall-clock time would rank the fact
/// above newer decisions (the backlog bug session time fixes); a thread with no start falls
/// back to the signal's own time, as there.
pub(crate) const LAST_CONFIRMED_SQL: &str =
    "(SELECT MAX(COALESCE(NULLIF(rt.created_at, 0), sg.created_at))
         FROM fact_signals sg LEFT JOIN threads rt ON rt.id = sg.thread_id
         WHERE sg.fact_uid = f.uid AND sg.kind = 'reaffirmed'
           AND sg.thread_id IS NOT NULL AND sg.thread_id IS NOT f.thread_id)";

/// The project memory ordering key after `pinned`: the later of when the fact was decided and
/// when a later session last confirmed it.
pub(crate) fn recency_sql(truth: bool) -> String {
    if truth {
        format!("MAX({DECIDED_AT_SQL}, COALESCE({LAST_CONFIRMED_SQL}, 0))")
    } else {
        DECIDED_AT_SQL.to_string()
    }
}

/// Whether the index has migration 0035's tables. Read-only sidecars and pooled readers never
/// migrate, and naming a missing table fails the whole query, so every reader checks this and
/// treats an older index as "every fact is current".
pub fn has_truth_tables(conn: &Connection) -> bool {
    conn.prepare_cached(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type = 'table' AND name IN ('fact_links', 'fact_signals')",
    )
    .and_then(|mut stmt| stmt.query_row([], |r| r.get::<_, i64>(0)))
    .map(|n| n == 2)
    .unwrap_or(false)
}

/// `YYYY-MM-DD` (UTC) of an epoch-seconds time, the date every surface prints next to a fact.
pub fn fact_date(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

// ---- derived status ----

/// Where a fact stands, in priority order: an active `supersedes` link points at it
/// (`superseded`), an unresolved stale / wrong flag (`flagged`), an active `contradicts` link
/// on either side whose other fact is still in play, not hidden or superseded (`disputed`),
/// else `current`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum FactStatus {
    #[default]
    Current,
    Disputed,
    Flagged,
    Superseded,
}

impl FactStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            FactStatus::Current => "current",
            FactStatus::Disputed => "disputed",
            FactStatus::Flagged => "flagged",
            FactStatus::Superseded => "superseded",
        }
    }
}

/// The other side of an active link: the fact that replaced this one, one it replaces, or one
/// that disputes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LinkedFact {
    pub id: i64,
    pub text: String,
    /// See [`DECIDED_AT_SQL`].
    pub decided_at: i64,
    /// Its session thread; None for a recorded or imported fact (no session to cite).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<i64>,
}

/// An unresolved stale / wrong flag on a fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FactFlag {
    /// The `fact_signals` row.
    pub id: i64,
    /// `stale` or `wrong`.
    pub kind: String,
    /// `agent`, `distill` (a memory pass retired it) or `user`.
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<i64>,
}

/// Everything derived about one fact from its links and signals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactTruth {
    pub status: FactStatus,
    /// The newest fact that actively supersedes this one.
    pub superseded_by: Option<LinkedFact>,
    /// The facts this one actively supersedes, newest first.
    pub replaces: Vec<LinkedFact>,
    /// The facts an active `contradicts` link pairs it with, newest first.
    pub disputed_by: Vec<LinkedFact>,
    /// Unresolved stale / wrong flags, newest first.
    pub flags: Vec<FactFlag>,
    /// 1 + the distinct other sessions that reaffirmed it.
    pub support: i64,
    /// The newest start among those sessions.
    pub last_confirmed: Option<i64>,
}

impl Default for FactTruth {
    fn default() -> Self {
        FactTruth {
            status: FactStatus::Current,
            superseded_by: None,
            replaces: Vec::new(),
            disputed_by: Vec::new(),
            flags: Vec::new(),
            support: 1,
            last_confirmed: None,
        }
    }
}

/// [`FactTruth`] for each of `uids`, in two queries over a `json_each` list (no per-fact
/// query). A uid with nothing recorded maps to the default (current, support 1); so does every
/// uid on an index without 0035's tables.
///
/// A link is active while both facts exist and the user has not undone it (`dismissed_at`).
/// A `supersedes` also needs its newer (`from`) side to stand (the same rule as
/// [`SUPERSEDED_SQL`]). A `contradicts` needs its other side visible and not superseded: a fact
/// the user deleted, or one a later fact replaced, is history and disputes nothing. Links
/// whose uid matches no row are ignored. A uid listed twice is looked up once.
pub fn fact_truth(conn: &Connection, uids: &[&str]) -> Result<HashMap<String, FactTruth>> {
    let mut out: HashMap<String, FactTruth> = uids
        .iter()
        .map(|u| (u.to_string(), FactTruth::default()))
        .collect();
    if uids.is_empty() || !has_truth_tables(conn) {
        return Ok(out);
    }
    // The map's keys, not `uids`: a caller merging lists (a pinned flagged fact is in both the
    // live and the Flagged list) may pass one uid twice, and json_each would return its links
    // and flags once per copy.
    let keys: Vec<&str> = out.keys().map(String::as_str).collect();
    let list = serde_json::to_string(&keys)?;
    let da = DECIDED_AT_SQL;
    let sup = SUPERSEDED_SQL;
    let stands = newer_stands!("f");
    let sql = format!(
        "SELECT j.value, 0, lk.kind, f.id, f.text, {da}, f.thread_id, f.extractor
         FROM json_each(?1) j
         JOIN fact_links lk ON lk.to_uid = j.value
         JOIN facts f ON f.uid = lk.from_uid
         JOIN threads t ON t.id = f.thread_id
         WHERE lk.dismissed_at IS NULL
           AND CASE WHEN lk.kind = 'supersedes' THEN {stands}
                    ELSE f.hidden = 0 AND NOT {sup} END
         UNION ALL
         SELECT j.value, 1, lk.kind, f.id, f.text, {da}, f.thread_id, f.extractor
         FROM json_each(?1) j
         JOIN fact_links lk ON lk.from_uid = j.value
         JOIN facts me ON me.uid = j.value
         JOIN facts f ON f.uid = lk.to_uid
         JOIN threads t ON t.id = f.thread_id
         WHERE me.hidden = 0 AND lk.dismissed_at IS NULL
           AND (lk.kind = 'supersedes' OR (f.hidden = 0 AND NOT {sup}))"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map([&list], |r| {
        let extractor: String = r.get(7)?;
        let thread: i64 = r.get(6)?;
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)? == 1,
            r.get::<_, String>(2)?,
            LinkedFact {
                id: r.get(3)?,
                text: r.get(4)?,
                decided_at: r.get(5)?,
                thread_id: is_session_extractor(&extractor).then_some(thread),
            },
        ))
    })?;
    for row in rows {
        let (uid, outgoing, kind, other) = row?;
        let Some(t) = out.get_mut(&uid) else { continue };
        match (kind.as_str(), outgoing) {
            ("supersedes", false) => {
                if t.superseded_by
                    .as_ref()
                    .is_none_or(|s| (other.decided_at, other.id) > (s.decided_at, s.id))
                {
                    t.superseded_by = Some(other);
                }
            }
            ("supersedes", true) => t.replaces.push(other),
            // Either direction disputes it; a pair linked both ways counts once.
            ("contradicts", _) if !t.disputed_by.iter().any(|d| d.id == other.id) => {
                t.disputed_by.push(other);
            }
            _ => {}
        }
    }

    let mut stmt = conn.prepare_cached(
        "SELECT j.value, sg.id, sg.kind, sg.source, sg.reason, sg.created_at, sg.thread_id,
                rt.created_at, sg.thread_id IS NOT f.thread_id
         FROM json_each(?1) j
         JOIN fact_signals sg ON sg.fact_uid = j.value
         JOIN facts f ON f.uid = j.value
         LEFT JOIN threads rt ON rt.id = sg.thread_id
         WHERE sg.kind = 'reaffirmed' OR sg.resolved_at IS NULL",
    )?;
    let rows = stmt.query_map([&list], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, Option<i64>>(6)?,
            r.get::<_, Option<i64>>(7)?,
            r.get::<_, bool>(8)?,
        ))
    })?;
    let mut confirming: HashMap<String, HashSet<i64>> = HashMap::new();
    for row in rows {
        let (uid, id, kind, source, reason, created_at, thread_id, thread_start, other) = row?;
        let Some(t) = out.get_mut(&uid) else { continue };
        if kind == "reaffirmed" {
            // A reaffirmation only counts from another session than the fact's own.
            let Some(tid) = thread_id.filter(|_| other) else {
                continue;
            };
            if confirming.entry(uid).or_default().insert(tid) {
                t.support += 1;
            }
            let when = thread_start.filter(|s| *s > 0).unwrap_or(created_at);
            t.last_confirmed = Some(t.last_confirmed.map_or(when, |w| w.max(when)));
        } else {
            t.flags.push(FactFlag {
                id,
                kind,
                source,
                reason,
                created_at,
                thread_id,
            });
        }
    }

    for t in out.values_mut() {
        t.replaces
            .sort_by_key(|f| std::cmp::Reverse((f.decided_at, f.id)));
        t.disputed_by
            .sort_by_key(|f| std::cmp::Reverse((f.decided_at, f.id)));
        t.flags
            .sort_by_key(|f| std::cmp::Reverse((f.created_at, f.id)));
        t.status = if t.superseded_by.is_some() {
            FactStatus::Superseded
        } else if !t.flags.is_empty() {
            FactStatus::Flagged
        } else if !t.disputed_by.is_empty() {
            FactStatus::Disputed
        } else {
            FactStatus::Current
        };
    }
    Ok(out)
}

// ---- writers: links ----

/// A relation between two facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkKind {
    /// The `from` fact replaces the `to` fact (the newer one is `from`).
    Supersedes,
    /// The two disagree and nothing settled it: both show as disputed.
    Contradicts,
}

impl LinkKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LinkKind::Supersedes => "supersedes",
            LinkKind::Contradicts => "contradicts",
        }
    }

    /// `supersedes` / `contradicts` (trimmed, any case), else None.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "supersedes" => Some(LinkKind::Supersedes),
            "contradicts" => Some(LinkKind::Contradicts),
            _ => None,
        }
    }
}

/// Who declared a link (`fact_links.source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkSource {
    /// The distiller, from a session that plainly states it.
    Distill,
    /// The user, in the desktop app.
    User,
    /// An agent (`record_decision` / `record_gotcha` `replaces`).
    Agent,
    /// A repo memory import (a line's `supersedes`, published by a person).
    Repo,
    /// A conflict review the user acted on.
    Review,
}

impl LinkSource {
    pub fn as_str(self) -> &'static str {
        match self {
            LinkSource::Distill => "distill",
            LinkSource::User => "user",
            LinkSource::Agent => "agent",
            LinkSource::Repo => "repo",
            LinkSource::Review => "review",
        }
    }

    /// Machine sources: their `supersedes` never removes a pinned or edited fact (it is stored
    /// as `contradicts`, as a repo import's is), and their direction must match the dates.
    fn is_machine(self) -> bool {
        matches!(self, LinkSource::Distill | LinkSource::Agent)
    }
}

/// Why [`link_facts`] stored nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum LinkRejection {
    /// A fact does not exist or is hidden.
    NotFound,
    /// Only decisions and gotchas link.
    NotLinkable,
    /// A decision and a gotcha.
    KindMismatch,
    /// The two facts belong to different projects.
    CrossProject,
    /// A fact and itself.
    SelfLink,
    /// The `to` fact already (transitively) supersedes the `from` fact.
    Cycle,
    /// A machine-declared `supersedes` whose `from` side's session ended before its `to` side
    /// was decided.
    WrongDirection,
    /// A `contradicts` for a pair a `supersedes` already settles.
    AlreadySettled,
    /// A machine source (the distiller, an agent, a repo import) asserting a pair the user
    /// undid a link between: the user's verdict stands until the user links it again.
    Dismissed,
}

/// What [`link_facts`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "outcome")]
pub enum LinkOutcome {
    /// Stored as asked (or it was already there). `id` is the `fact_links` row.
    Linked {
        id: i64,
        kind: LinkKind,
    },
    /// A machine or repo `supersedes` whose target is pinned or user-edited, stored as
    /// `contradicts`: the pair shows as disputed and the user resolves it.
    Downgraded {
        id: i64,
    },
    Rejected {
        reason: LinkRejection,
    },
}

impl LinkOutcome {
    /// The `fact_links` row written (or found), unless rejected.
    pub fn link_id(&self) -> Option<i64> {
        match self {
            LinkOutcome::Linked { id, .. } | LinkOutcome::Downgraded { id } => Some(*id),
            LinkOutcome::Rejected { .. } => None,
        }
    }

    /// The kind stored, unless rejected.
    pub fn stored_kind(&self) -> Option<LinkKind> {
        match self {
            LinkOutcome::Linked { kind, .. } => Some(*kind),
            LinkOutcome::Downgraded { .. } => Some(LinkKind::Contradicts),
            LinkOutcome::Rejected { .. } => None,
        }
    }
}

/// One fact as the writers check it.
struct FactRow {
    uid: String,
    kind: String,
    pinned: bool,
    edited: bool,
    hidden: bool,
    project: String,
    decided_at: i64,
    /// See [`SESSION_END_SQL`].
    session_end: i64,
}

fn fact_row(conn: &Connection, by_uid: bool, key: &dyn rusqlite::ToSql) -> Result<Option<FactRow>> {
    let sql = format!(
        "SELECT f.uid, f.kind, f.pinned, f.edited, f.hidden,
                COALESCE(t.project_key, t.project_path, ''), {DECIDED_AT_SQL}, {SESSION_END_SQL}
         FROM facts f JOIN threads t ON t.id = f.thread_id WHERE {} = ?1",
        if by_uid { "f.uid" } else { "f.id" }
    );
    Ok(conn
        .prepare_cached(&sql)?
        .query_row([key], |r| {
            Ok(FactRow {
                uid: r.get(0)?,
                kind: r.get(1)?,
                pinned: r.get::<_, i64>(2)? != 0,
                edited: r.get::<_, i64>(3)? != 0,
                hidden: r.get::<_, i64>(4)? != 0,
                project: r.get(5)?,
                decided_at: r.get(6)?,
                session_end: r.get(7)?,
            })
        })
        .optional()?)
}

/// The uid of fact `id`, if it exists.
pub(crate) fn uid_of(conn: &Connection, id: i64) -> Result<Option<String>> {
    Ok(conn
        .prepare_cached("SELECT uid FROM facts WHERE id = ?1")?
        .query_row([id], |r| r.get(0))
        .optional()?)
}

/// Link two facts: `from` supersedes or contradicts `to`. Both must be visible decisions or
/// gotchas of the same kind and project, and not the same fact.
///
/// - A `supersedes` from [`LinkSource::Distill`] or [`LinkSource::Agent`] must not point back
///   in time (`from`'s session ended before `to` was decided:
///   [`LinkRejection::WrongDirection`]). When its target is pinned or user-edited, it and a
///   [`LinkSource::Repo`] one are stored as `contradicts` ([`LinkOutcome::Downgraded`]): a
///   line in a checkout (a contributor's branch included) must not silently retire what the
///   user pinned. A user or review link is stored as asked.
/// - A `supersedes` that would close a cycle (`to` already supersedes `from`, directly or
///   through others) is rejected; storing one removes any `contradicts` between the pair.
/// - A `contradicts` for a pair a `supersedes` already settles is rejected; one that exists
///   the other way round is returned as it is.
/// - A pair the user undid a link between (any kind, either direction: [`unlink_link`] keeps
///   the row with `dismissed_at`) refuses the distiller, an agent and a repo import
///   ([`LinkRejection::Dismissed`]), so a re-distill of the session that declared it cannot
///   bring it back. A user or review link clears the dismissal of the row it writes.
/// - The same link asserted again is not duplicated. A user's assertion takes over a link
///   another source wrote, and any non-distill source takes over a distilled one, so a
///   re-distill (which replaces the thread's own distilled links) cannot drop it. Two
///   sessions distilling the same link leave it owned by neither (`thread_id` NULL), for the
///   same reason.
/// - "Same project" includes another clone of the same repo whose fact this project reads
///   through repo memory (see [`same_repo`]).
/// - All or nothing ([`atomic`]): inside the caller's transaction when it holds one.
pub fn link_facts(
    conn: &Connection,
    from_id: i64,
    to_id: i64,
    kind: LinkKind,
    source: LinkSource,
    thread_id: Option<i64>,
    now: i64,
) -> Result<LinkOutcome> {
    if from_id == to_id {
        return Ok(LinkOutcome::Rejected {
            reason: LinkRejection::SelfLink,
        });
    }
    let from = fact_row(conn, false, &from_id)?;
    let to = fact_row(conn, false, &to_id)?;
    link_rows(conn, from, to, kind, source, thread_id, now, true)
}

/// [`link_facts`] by uid. `same_project: false` skips the project check, for a repo import
/// whose caller already checked that both uids belong to the repo's family of clones.
#[allow(clippy::too_many_arguments)]
pub(crate) fn link_uids(
    conn: &Connection,
    from_uid: &str,
    to_uid: &str,
    kind: LinkKind,
    source: LinkSource,
    thread_id: Option<i64>,
    now: i64,
    same_project: bool,
) -> Result<LinkOutcome> {
    if from_uid == to_uid {
        return Ok(LinkOutcome::Rejected {
            reason: LinkRejection::SelfLink,
        });
    }
    let from = fact_row(conn, true, &from_uid)?;
    let to = fact_row(conn, true, &to_uid)?;
    link_rows(conn, from, to, kind, source, thread_id, now, same_project)
}

/// Run `f` so that it writes all or nothing: in its own IMMEDIATE transaction when the caller
/// holds none (the write lock up front, so a read followed by a write never fails on a lock
/// upgrade while the app's writer is busy), else in a savepoint of the caller's transaction.
pub(crate) fn atomic<T>(conn: &Connection, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    if conn.is_autocommit() {
        let tx =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        // An error drops `tx`, which rolls back.
        let out = f(&tx)?;
        tx.commit()?;
        return Ok(out);
    }
    conn.execute_batch("SAVEPOINT cal_atomic")?;
    match f(conn) {
        Ok(out) => {
            conn.execute_batch("RELEASE cal_atomic")?;
            Ok(out)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK TO cal_atomic; RELEASE cal_atomic");
            Err(e)
        }
    }
}

/// Whether two facts under different project keys are still one repo's memory: the keys are
/// clones with the same portable id (from the `projects` cache only, never `git`), or one
/// project reads the other's fact through repo memory ([`super::repo_memory::BORROWED`]).
/// Project memory lists borrowed facts beside local ones, so the user or an agent linking
/// the two must not be refused as cross-project.
fn same_repo(conn: &Connection, a: &FactRow, b: &FactRow) -> bool {
    let portable = conn
        .prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM projects pa JOIN projects pb
               ON pb.key = ?2 AND pb.portable_id = pa.portable_id
             WHERE pa.key = ?1 AND pa.portable_id IS NOT NULL)",
        )
        .and_then(|mut stmt| stmt.query_row(params![a.project, b.project], |r| r.get(0)))
        .unwrap_or(false);
    portable
        || super::repo_memory::borrowed_uids(conn, &a.project).contains(&b.uid)
        || super::repo_memory::borrowed_uids(conn, &b.project).contains(&a.uid)
}

/// [`link_rows_once`] all or nothing: removing a pair's `contradicts` and storing its
/// `supersedes` happen together.
#[allow(clippy::too_many_arguments)]
fn link_rows(
    conn: &Connection,
    from: Option<FactRow>,
    to: Option<FactRow>,
    kind: LinkKind,
    source: LinkSource,
    thread_id: Option<i64>,
    now: i64,
    same_project: bool,
) -> Result<LinkOutcome> {
    atomic(conn, |c| {
        link_rows_once(c, from, to, kind, source, thread_id, now, same_project)
    })
}

#[allow(clippy::too_many_arguments)]
fn link_rows_once(
    conn: &Connection,
    from: Option<FactRow>,
    to: Option<FactRow>,
    kind: LinkKind,
    source: LinkSource,
    thread_id: Option<i64>,
    now: i64,
    same_project: bool,
) -> Result<LinkOutcome> {
    let reject = |reason| Ok(LinkOutcome::Rejected { reason });
    let (Some(from), Some(to)) = (from, to) else {
        return reject(LinkRejection::NotFound);
    };
    if from.hidden || to.hidden {
        return reject(LinkRejection::NotFound);
    }
    if from.uid == to.uid {
        return reject(LinkRejection::SelfLink);
    }
    let linkable = |k: &str| matches!(k, "decision" | "gotcha");
    if !linkable(&from.kind) || !linkable(&to.kind) {
        return reject(LinkRejection::NotLinkable);
    }
    if from.kind != to.kind {
        return reject(LinkRejection::KindMismatch);
    }
    if same_project && from.project != to.project && !same_repo(conn, &from, &to) {
        return reject(LinkRejection::CrossProject);
    }
    let by_user = matches!(source, LinkSource::User | LinkSource::Review);
    if !by_user && pair_dismissed(conn, &from.uid, &to.uid)? {
        return reject(LinkRejection::Dismissed);
    }
    let mut stored = kind;
    if kind == LinkKind::Supersedes {
        // Against the session's end, not its start: a session can reverse what was decided
        // while it ran (an agent's record_decision at 10:30 in a session that began at 10:00).
        if source.is_machine() && from.session_end < to.decided_at {
            return reject(LinkRejection::WrongDirection);
        }
        if (source.is_machine() || source == LinkSource::Repo) && (to.pinned || to.edited) {
            stored = LinkKind::Contradicts;
        }
    }
    let pair = params![from.uid, to.uid];
    match stored {
        LinkKind::Supersedes => {
            if supersedes_path(conn, &to.uid, &from.uid)? {
                return reject(LinkRejection::Cycle);
            }
            // The pair is settled now: an earlier "they disagree" no longer holds.
            conn.execute(
                "DELETE FROM fact_links WHERE kind = 'contradicts'
                   AND ((from_uid = ?1 AND to_uid = ?2) OR (from_uid = ?2 AND to_uid = ?1))",
                pair,
            )?;
        }
        LinkKind::Contradicts => {
            let settled: bool = conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM fact_links WHERE kind = 'supersedes'
                   AND dismissed_at IS NULL
                   AND ((from_uid = ?1 AND to_uid = ?2) OR (from_uid = ?2 AND to_uid = ?1)))",
                pair,
                |r| r.get(0),
            )?;
            if settled {
                return reject(LinkRejection::AlreadySettled);
            }
            let reverse: Option<(i64, bool)> = conn
                .query_row(
                    "SELECT id, dismissed_at IS NOT NULL FROM fact_links
                     WHERE from_uid = ?2 AND to_uid = ?1 AND kind = 'contradicts'",
                    pair,
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((id, dismissed)) = reverse {
                // Only a user link gets here past a dismissed pair: it revives that row.
                if dismissed {
                    conn.execute(
                        "UPDATE fact_links SET dismissed_at = NULL, source = ?2 WHERE id = ?1",
                        params![id, source.as_str()],
                    )?;
                }
                return Ok(outcome(kind, stored, id));
            }
        }
    }
    // One row per pair and kind. A user's assertion takes it over, and so does any source
    // over a distilled one. Two sessions distilling the same pair (the newer one declares it,
    // then the older one is distilled and declares it again) leave it with no single owning
    // thread (NULL), so neither one's re-distill can drop a link the other still asserts and
    // bring the replaced decision back; only the user's Undo removes it then. A dismissed row
    // is reached only by a user or review link (machine sources stopped above), which revives
    // it as theirs.
    conn.execute(
        "INSERT INTO fact_links (from_uid, to_uid, kind, source, thread_id, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (from_uid, to_uid, kind) DO UPDATE
            SET source = excluded.source,
                thread_id = CASE
                    WHEN fact_links.source = 'distill' AND excluded.source = 'distill'
                     AND fact_links.thread_id IS NOT excluded.thread_id THEN NULL
                    ELSE excluded.thread_id END,
                dismissed_at = NULL
          WHERE excluded.source = 'user' OR fact_links.source = 'distill'
             OR fact_links.dismissed_at IS NOT NULL",
        params![
            from.uid,
            to.uid,
            stored.as_str(),
            source.as_str(),
            thread_id,
            now
        ],
    )?;
    let id: i64 = conn.query_row(
        "SELECT id FROM fact_links WHERE from_uid = ?1 AND to_uid = ?2 AND kind = ?3",
        params![from.uid, to.uid, stored.as_str()],
        |r| r.get(0),
    )?;
    Ok(outcome(kind, stored, id))
}

/// Whether the user undid a link between `a` and `b` (either direction, any kind). A repo
/// import plans around it too, since the link writer refuses such a pair for it.
pub(crate) fn pair_dismissed(conn: &Connection, a: &str, b: &str) -> Result<bool> {
    Ok(conn
        .prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM fact_links WHERE dismissed_at IS NOT NULL
               AND ((from_uid = ?1 AND to_uid = ?2) OR (from_uid = ?2 AND to_uid = ?1)))",
        )?
        .query_row(params![a, b], |r| r.get(0))?)
}

/// Whether any link relates `a` and `b` (either direction, any kind, undone or not). A repo
/// import whose line supersedes a pinned or edited fact plans around it: the writer stores
/// that as `contradicts`, so no `supersedes` row ever shows the line as satisfied.
pub(crate) fn pair_linked(conn: &Connection, a: &str, b: &str) -> Result<bool> {
    Ok(conn
        .prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM fact_links
               WHERE (from_uid = ?1 AND to_uid = ?2) OR (from_uid = ?2 AND to_uid = ?1))",
        )?
        .query_row(params![a, b], |r| r.get(0))?)
}

/// The targets of `from_uid`'s active `contradicts` links that a repo import wrote (a line's
/// `supersedes` onto a pinned or edited fact), sorted.
pub(crate) fn repo_contradicts_from(conn: &Connection, from_uid: &str) -> Result<Vec<String>> {
    if !has_truth_tables(conn) {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare_cached(
        "SELECT to_uid FROM fact_links WHERE from_uid = ?1 AND kind = 'contradicts'
           AND source = 'repo' AND dismissed_at IS NULL ORDER BY to_uid",
    )?;
    let rows = stmt
        .query_map([from_uid], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(rows)
}

fn outcome(asked: LinkKind, stored: LinkKind, id: i64) -> LinkOutcome {
    if asked != stored {
        LinkOutcome::Downgraded { id }
    } else {
        LinkOutcome::Linked { id, kind: stored }
    }
}

/// How far a cycle check follows `supersedes` links. Chains are a handful long in practice;
/// the bound only keeps a corrupted graph from looping.
const CYCLE_DEPTH: i64 = 64;

/// Whether `start` reaches `target` through `supersedes` links (any source, active or not:
/// a cycle is refused even through a hidden fact, which the user may unhide). A link the user
/// undid does not count: after an Undo the user may link the pair the other way round.
pub(crate) fn supersedes_path(conn: &Connection, start: &str, target: &str) -> Result<bool> {
    Ok(conn
        .prepare_cached(
            "WITH RECURSIVE reach(uid, depth) AS (
                SELECT ?1, 0
                UNION
                SELECT lk.to_uid, r.depth + 1 FROM fact_links lk JOIN reach r ON lk.from_uid = r.uid
                WHERE lk.kind = 'supersedes' AND lk.dismissed_at IS NULL AND r.depth < ?3
             )
             SELECT EXISTS (SELECT 1 FROM reach WHERE uid = ?2)",
        )?
        .query_row(params![start, target, CYCLE_DEPTH], |r| r.get(0))?)
}

/// The user undoes one link by its `fact_links` id. The row stays with `dismissed_at` set,
/// inactive for every reader, so the distiller, an agent or a repo import asserting the pair
/// again is a no-op ([`LinkRejection::Dismissed`]) instead of undoing the Undo; a later user
/// link clears it. Returns whether an active link was dismissed.
pub fn unlink_link(conn: &Connection, link_id: i64, now: i64) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE fact_links SET dismissed_at = ?2 WHERE id = ?1 AND dismissed_at IS NULL",
        params![link_id, now],
    )? > 0)
}

/// [`unlink_link`] for the links from fact `from_id` to fact `to_id` (of `kind`, or both
/// kinds), in that direction only, e.g. the Projects page's Undo on a replaced fact. Returns
/// how many were dismissed.
pub fn unlink_facts(
    conn: &Connection,
    from_id: i64,
    to_id: i64,
    kind: Option<LinkKind>,
    now: i64,
) -> Result<usize> {
    let (Some(from), Some(to)) = (uid_of(conn, from_id)?, uid_of(conn, to_id)?) else {
        return Ok(0);
    };
    Ok(conn.execute(
        "UPDATE fact_links SET dismissed_at = ?4 WHERE from_uid = ?1 AND to_uid = ?2
           AND (?3 IS NULL OR kind = ?3) AND dismissed_at IS NULL",
        params![from, to, kind.map(LinkKind::as_str), now],
    )?)
}

/// The user restores fact `fact_id` as current (the Replaced list's and Today's Undo): every
/// `supersedes` link onto it that the user has not undone yet is dismissed, in one
/// transaction ([`atomic`]), as [`unlink_link`] dismisses one, so no machine source declares
/// them again. One Undo used to dismiss only the newest replacer's link, and a fact two
/// sessions replaced stayed replaced by the other (the #125 follow-up). A link whose newer
/// side is hidden goes too: it is inactive only until that fact is unhidden, which must not
/// take back the restore. Returns how many links were dismissed.
pub fn restore_fact(conn: &Connection, fact_id: i64, now: i64) -> Result<usize> {
    if !has_truth_tables(conn) {
        return Ok(0);
    }
    atomic(conn, |c| {
        let Some(uid) = uid_of(c, fact_id)? else {
            return Ok(0);
        };
        Ok(c.execute(
            "UPDATE fact_links SET dismissed_at = ?2
             WHERE to_uid = ?1 AND kind = 'supersedes' AND dismissed_at IS NULL",
            params![uid, now],
        )?)
    })
}

// ---- writers: signals ----

/// A flag on one fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FlagKind {
    /// It was true once and no longer is.
    Stale,
    /// It was never right.
    Wrong,
}

impl FlagKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FlagKind::Stale => "stale",
            FlagKind::Wrong => "wrong",
        }
    }

    /// `stale` / `wrong` (trimmed, any case), else None.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "stale" => Some(FlagKind::Stale),
            "wrong" => Some(FlagKind::Wrong),
            _ => None,
        }
    }
}

/// Who raised a signal (`fact_signals.source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SignalSource {
    /// The distiller (`retires`, `reaffirms`).
    Distill,
    /// An agent (MCP `flag_fact`).
    Agent,
    /// The user.
    User,
}

impl SignalSource {
    pub fn as_str(self) -> &'static str {
        match self {
            SignalSource::Distill => "distill",
            SignalSource::Agent => "agent",
            SignalSource::User => "user",
        }
    }
}

/// Unresolved agent flags a project may hold at once on facts the flag keeps out of memory
/// (visible, not superseded, not pinned or edited). A prompt-injected session that flags
/// everything it was shown hides at most this many facts until the user reviews them.
pub const AGENT_FLAG_CAP: i64 = 10;

/// Longest flag reason kept, in chars.
const FLAG_REASON_MAX: usize = 500;

/// What [`flag_fact`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "outcome")]
pub enum FlagOutcome {
    /// Stored. `id` is the `fact_signals` row.
    Flagged { id: i64 },
    /// An unresolved flag of that kind was already there; nothing new was written.
    AlreadyFlagged { id: i64 },
    /// The project already holds [`AGENT_FLAG_CAP`] unresolved agent flags; nothing written.
    CapReached,
    /// No visible decision or gotcha has that id.
    NotFound,
}

/// Flag fact `fact_id` stale or wrong. A flagged fact leaves injected memory until the user
/// keeps it ([`resolve_fact_flags`]) or hides it, except a pinned or user-edited one: its flag
/// is stored and reported (status `flagged` in MCP and the app) but the user's trusted fact
/// stays injected. Agent flags are capped per project ([`AGENT_FLAG_CAP`]). `reason` is
/// redacted and capped.
pub fn flag_fact(
    conn: &Connection,
    fact_id: i64,
    kind: FlagKind,
    source: SignalSource,
    reason: Option<&str>,
    thread_id: Option<i64>,
    now: i64,
) -> Result<FlagOutcome> {
    let Some(f) = fact_row(conn, false, &fact_id)? else {
        return Ok(FlagOutcome::NotFound);
    };
    if f.hidden || !matches!(f.kind.as_str(), "decision" | "gotcha") {
        return Ok(FlagOutcome::NotFound);
    }
    flag_row(conn, &f, kind, source, reason, thread_id, now)
}

fn flag_row(
    conn: &Connection,
    f: &FactRow,
    kind: FlagKind,
    source: SignalSource,
    reason: Option<&str>,
    thread_id: Option<i64>,
    now: i64,
) -> Result<FlagOutcome> {
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM fact_signals
             WHERE fact_uid = ?1 AND kind = ?2 AND resolved_at IS NULL ORDER BY id LIMIT 1",
            params![f.uid, kind.as_str()],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        return Ok(FlagOutcome::AlreadyFlagged { id });
    }
    if source == SignalSource::Agent {
        // Only flags that are what keeps their fact out of memory count: one on a fact the
        // user hid (the Flagged group's Hide resolves nothing) or that another fact replaced
        // hides nothing, and counting it would leave flag_fact refused for good after ten
        // Hides in a project; nor does one on a pinned or edited fact, which stays injected
        // (each fact holds one unresolved flag of a kind, so these cannot pile up either).
        let held: i64 = conn
            .prepare_cached(&format!(
                "SELECT COUNT(*) FROM fact_signals sg
                 JOIN facts f ON f.uid = sg.fact_uid
                 JOIN threads t ON t.id = f.thread_id
                 WHERE sg.source = 'agent' AND sg.kind IN ('stale', 'wrong')
                   AND sg.resolved_at IS NULL AND f.hidden = 0
                   AND f.pinned = 0 AND f.edited = 0 AND NOT {SUPERSEDED_SQL}
                   AND COALESCE(t.project_key, t.project_path, '') = ?1"
            ))?
            .query_row([&f.project], |r| r.get(0))?;
        if held >= AGENT_FLAG_CAP {
            return Ok(FlagOutcome::CapReached);
        }
    }
    let reason = reason
        .map(|r| crate::context::redact_secrets(r.trim()).into_owned())
        .map(|r| cut_words(&r, FLAG_REASON_MAX))
        .filter(|r| !r.is_empty());
    conn.execute(
        "INSERT INTO fact_signals (fact_uid, kind, source, thread_id, reason, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            f.uid,
            kind.as_str(),
            source.as_str(),
            thread_id,
            reason,
            now
        ],
    )?;
    Ok(FlagOutcome::Flagged {
        id: conn.last_insert_rowid(),
    })
}

/// The user keeps fact `fact_id`: resolve its unresolved stale / wrong flags (kept as
/// history). Returns how many were resolved.
pub fn resolve_fact_flags(conn: &Connection, fact_id: i64, now: i64) -> Result<usize> {
    let Some(uid) = uid_of(conn, fact_id)? else {
        return Ok(0);
    };
    Ok(conn.execute(
        "UPDATE fact_signals SET resolved_at = ?2
         WHERE fact_uid = ?1 AND kind IN ('stale', 'wrong') AND resolved_at IS NULL",
        params![uid, now],
    )?)
}

// ---- caps on what an agent can write ----

/// Agent `supersedes` links (MCP `record_*` `replaces`, `cal remember --replaces`) a project
/// may take within [`AGENT_SUPERSEDE_WINDOW_SECS`]. Each one takes its target out of injected
/// memory with no review step, so a prompt-injected session looping `replaces` over the ids it
/// was shown would otherwise empty the project's memory. Past the cap the link is stored as
/// `contradicts`: both facts stay injected, marked disputed, and the user settles it.
pub const AGENT_SUPERSEDE_CAP: i64 = 5;

/// The window [`AGENT_SUPERSEDE_CAP`] counts over. A window rather than a count of every link
/// held (as [`AGENT_FLAG_CAP`] does): a flag ends when the user keeps or hides the fact, but an
/// accepted replacement is never reviewed, so a held count would turn every later legitimate
/// reversal into a dispute for good. A day covers any one session.
pub const AGENT_SUPERSEDE_WINDOW_SECS: i64 = 24 * 3600;

/// How many active agent `supersedes` links (newer side visible) the facts of `project` wrote
/// since `now - AGENT_SUPERSEDE_WINDOW_SECS`. An agent's facts live in the project's notes
/// thread, so the newer side's thread names the project. `fact_links` is small (links are
/// declared, never inferred), so scanning it on a write is cheap.
pub fn agent_supersedes_recent(conn: &Connection, project: &str, now: i64) -> Result<i64> {
    if !has_truth_tables(conn) {
        return Ok(0);
    }
    Ok(conn
        .prepare_cached(
            "SELECT COUNT(*) FROM fact_links lk
             JOIN facts nf ON nf.uid = lk.from_uid
             JOIN threads t ON t.id = nf.thread_id
             WHERE lk.source = 'agent' AND lk.kind = 'supersedes' AND lk.created_at > ?2
               AND lk.dismissed_at IS NULL
               AND nf.hidden = 0 AND COALESCE(t.project_key, t.project_path, '') = ?1",
        )?
        .query_row(params![project, now - AGENT_SUPERSEDE_WINDOW_SECS], |r| {
            r.get(0)
        })?)
}

/// Whether fact `fact_id` is one of `project`'s: its own, another clone's of the same repo, or
/// one `project` reads through repo memory (the same rule [`link_facts`] applies), so an agent
/// flagging what its memory showed is never refused for a borrowed fact. None when no fact
/// has that id.
pub fn fact_in_project(conn: &Connection, fact_id: i64, project: &str) -> Result<Option<bool>> {
    let Some(f) = fact_row(conn, false, &fact_id)? else {
        return Ok(None);
    };
    if f.project == project {
        return Ok(Some(true));
    }
    // `same_repo` compares two facts; the project side has no fact, so its uid matches none.
    let side = FactRow {
        uid: String::new(),
        kind: String::new(),
        pinned: false,
        edited: false,
        hidden: false,
        project: project.to_string(),
        decided_at: 0,
        session_end: 0,
    };
    Ok(Some(same_repo(conn, &f, &side)))
}

/// Record that session `thread_id` reaffirmed the fact `uid` (once per session).
fn reaffirm_uid(conn: &Connection, uid: &str, thread_id: i64, now: i64) -> Result<bool> {
    Ok(conn.execute(
        "INSERT OR IGNORE INTO fact_signals (fact_uid, kind, source, thread_id, created_at)
         VALUES (?1, 'reaffirmed', 'distill', ?2, ?3)",
        params![uid, thread_id, now],
    )? > 0)
}

// ---- the distiller's view of existing memory ----

/// Most current decisions the distiller is shown from the thread's project.
pub const DISTILL_CONTEXT_DECISIONS: usize = 30;
/// Most current gotchas the distiller is shown.
pub const DISTILL_CONTEXT_GOTCHAS: usize = 20;
/// Longest item text the distiller is shown, in chars (cut at a word boundary).
pub const DISTILL_CONTEXT_TEXT_CHARS: usize = 200;

/// Per distilled thread: the most links, reaffirmations and retirements one session may
/// declare. A prompt-injected transcript can reshape at most this much memory per thread.
pub const DISTILL_MAX_SUPERSEDES: usize = 3;
pub const DISTILL_MAX_CONTRADICTS: usize = 3;
pub const DISTILL_MAX_REAFFIRMS: usize = 5;
pub const DISTILL_MAX_RETIRES: usize = 3;

/// One existing item the distiller was shown (`D1`, `G1`...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefEntry {
    pub id: i64,
    pub uid: String,
    /// `decision` or `gotcha`.
    pub kind: String,
    pub decided_at: i64,
    pub pinned: bool,
    pub edited: bool,
}

/// The refs of a [`distill_memory_context`] block, handed back to [`super::store_distilled`]
/// with the model's output. The default (no items) is what a caller that shows the model no
/// existing memory passes: then every `D` / `G` ref is invalid and dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefMap {
    entries: HashMap<String, RefEntry>,
    /// The distilled session's start (epoch seconds), when the thread has one.
    pub session_started_at: Option<i64>,
    /// Its last activity (`threads.updated_at`), when later than the start. A `retires` of an
    /// item decided after this is dropped: an older session cannot retire a newer decision.
    pub session_ended_at: Option<i64>,
}

impl RefMap {
    /// The item behind `D1` / `G3` (exact case: `d1` is the output's own item).
    pub fn get(&self, r: &str) -> Option<&RefEntry> {
        self.entries.get(r.trim())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The session's start as `YYYY-MM-DD`, for the block's `session_started` attribute.
    pub fn session_date(&self) -> Option<String> {
        self.session_started_at.map(fact_date)
    }

    /// The session's last day as `YYYY-MM-DD`, for the block's `session_ended` attribute:
    /// None when it is the start's day (the attribute would say nothing new).
    pub fn session_end_date(&self) -> Option<String> {
        let end = self.session_ended_at.map(fact_date)?;
        (Some(&end) != self.session_date().as_ref()).then_some(end)
    }

    /// The latest an item could have been decided and still be one this session saw: its
    /// last activity, else its start.
    fn session_end(&self) -> Option<i64> {
        self.session_ended_at.or(self.session_started_at)
    }

    /// Add an entry (tests and callers that build a map by hand).
    pub fn insert(&mut self, r: &str, entry: RefEntry) {
        self.entries.insert(r.to_string(), entry);
    }
}

/// `s` with whitespace collapsed, cut to at most `max` chars at a word boundary ("..." marks
/// a cut).
pub(crate) fn cut_words(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let keep = max.saturating_sub(3);
    let end = flat.char_indices().nth(keep).map_or(flat.len(), |(i, _)| i);
    let head = &flat[..end];
    // Back off to the last space unless that loses most of the text (one huge word).
    let head = match head.rfind(' ') {
        Some(i) if i >= end / 2 => &head[..i],
        _ => head,
    };
    format!("{}...", head.trim_end())
}

/// The bounded list of existing memory the distiller sees for `thread_id`'s project, and the
/// [`RefMap`] that resolves its refs: up to [`DISTILL_CONTEXT_DECISIONS`] decisions and
/// [`DISTILL_CONTEXT_GOTCHAS`] gotchas that are not superseded or flagged (disputed ones are
/// listed: the session may settle them; so are the ones only this thread's own last distill
/// superseded or retired, which a re-distill must be able to declare again), excluding the thread's own facts, Claude Code's
/// private auto memory and unpinned failure playbooks other than those this thread's last distill linked or retired, pinned first, then by session time. One line each,
/// `D1 (2026-10-01): text`, the text redacted and cut at [`DISTILL_CONTEXT_TEXT_CHARS`]. An
/// empty project (or a thread with no project) gives an empty block. The caller fences it.
pub fn distill_memory_context(conn: &Connection, thread_id: i64) -> Result<(String, RefMap)> {
    let mut refs = RefMap::default();
    let thread: Option<(Option<String>, Option<i64>, Option<i64>)> = conn
        .query_row(
            "SELECT COALESCE(project_key, project_path), created_at, updated_at
             FROM threads WHERE id = ?1",
            [thread_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((project, started, updated)) = thread else {
        return Ok((String::new(), refs));
    };
    refs.session_started_at = started.filter(|s| *s > 0);
    refs.session_ended_at = refs
        .session_started_at
        .and_then(|s| updated.filter(|u| *u > s));
    let Some(project) = project.filter(|p| !p.is_empty()) else {
        return Ok((String::new(), refs));
    };
    let truth = has_truth_tables(conn);
    // Live as memory injects it (the same standing and pinned-or-edited rules), except that
    // this thread's own distilled links and flags do not count: a re-distill replaces them, so
    // the model must see the items it superseded or retired last time to declare it again
    // (else the item would come back as current).
    let live = if truth {
        format!(
            " AND NOT EXISTS (SELECT 1 FROM fact_links lk JOIN facts nf ON nf.uid = lk.from_uid
               WHERE lk.to_uid = f.uid AND lk.kind = 'supersedes' AND {NEWER_STANDS_NF_SQL}
                 AND lk.dismissed_at IS NULL
                 AND NOT (lk.source = 'distill' AND lk.thread_id IS ?3))
           AND (f.pinned = 1 OR f.edited = 1 OR NOT EXISTS (SELECT 1 FROM fact_signals sg
               WHERE sg.fact_uid = f.uid AND sg.kind IN ('stale', 'wrong')
                 AND sg.resolved_at IS NULL
                 AND NOT (sg.source = 'distill' AND sg.thread_id IS ?3)))"
        )
    } else {
        String::new()
    };
    // An unpinned playbook is no memory here either (`super::MemoryQuery::playbooks`): they
    // are a repo's newest gotchas, and 27 in one measured repo would take all 20 slots. Except
    // one this thread's last pass linked (supersedes or contradicts, either way) or retired
    // (before playbooks left this list): the re-distill drops that link or flag, so unlisted,
    // the playbook came back as current or lost its dispute. An OR, not `IN (to, from)`: SQLite
    // then searches both indexes of fact_links instead of scanning it.
    let own_playbook = if truth {
        " OR EXISTS (SELECT 1 FROM fact_links lk
               WHERE (lk.to_uid = f.uid OR lk.from_uid = f.uid) AND lk.source = 'distill'
                 AND lk.thread_id IS ?3 AND lk.dismissed_at IS NULL)
           OR EXISTS (SELECT 1 FROM fact_signals sg
               WHERE sg.fact_uid = f.uid AND sg.kind IN ('stale', 'wrong')
                 AND sg.source = 'distill' AND sg.thread_id IS ?3 AND sg.resolved_at IS NULL)"
    } else {
        ""
    };
    let sql = format!(
        "SELECT f.id, f.uid, f.text, f.pinned, f.edited, {DECIDED_AT_SQL}
         FROM facts f JOIN threads t ON t.id = f.thread_id
         WHERE f.kind = ?1 AND COALESCE(t.project_key, t.project_path) = ?2
           AND f.thread_id != ?3 AND f.hidden = 0 AND f.uid IS NOT NULL
           AND f.extractor != ?4 AND (f.extractor != '{}' OR f.pinned = 1{own_playbook})
           AND (t.is_subagent = 0 OR f.pinned = 1){live}
         ORDER BY f.pinned DESC, {} DESC, f.id DESC LIMIT ?5",
        super::EXTRACTOR_PLAYBOOK,
        recency_sql(truth)
    );
    let mut lines = Vec::new();
    for (kind, prefix, max) in [
        ("decision", 'D', DISTILL_CONTEXT_DECISIONS),
        ("gotcha", 'G', DISTILL_CONTEXT_GOTCHAS),
    ] {
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt
            .query_map(
                params![
                    kind,
                    project,
                    thread_id,
                    super::harvest::EXTRACTOR_CLAUDE_MEMORY,
                    max as i64
                ],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)? != 0,
                        r.get::<_, i64>(4)? != 0,
                        r.get::<_, i64>(5)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut seen = HashSet::new();
        for (id, uid, text, pinned, edited, decided_at) in rows {
            // Redact before cutting, so a cut never leaves half a secret behind.
            let text = cut_words(
                &crate::context::redact_secrets(&text),
                DISTILL_CONTEXT_TEXT_CHARS,
            );
            if text.is_empty() || !seen.insert(text.to_lowercase()) {
                continue;
            }
            let r = format!("{prefix}{}", seen.len());
            lines.push(format!("{r} ({}): {text}", fact_date(decided_at)));
            refs.insert(
                &r,
                RefEntry {
                    id,
                    uid,
                    kind: kind.to_string(),
                    decided_at,
                    pinned,
                    edited,
                },
            );
        }
    }
    Ok((lines.join("\n"), refs))
}

/// Apply a distilled thread's `links`, `reaffirms` and `retires` (see
/// [`crate::agent::Distilled`]). `own` resolves the output's own refs (`d1`, `g2`) to the uids
/// [`super::store_distilled`] just wrote or kept; `refs` resolves the listed items. Caps per
/// thread ([`DISTILL_MAX_SUPERSEDES`] and the rest) count what was stored; an invalid ref, a
/// kind mismatch, a self link, a link between two listed items (this session states nothing
/// about two others), a link within the session, a link to a listed item the user undid a link
/// of this session to, a supersede that points back in time, or a retirement of an item decided
/// after the session ended is dropped silently. Runs inside `store_distilled`'s transaction, after it removed the
/// thread's previous distilled links and unresolved signals.
pub(crate) fn apply_distilled_truth(
    conn: &Connection,
    thread_id: i64,
    d: &crate::agent::Distilled,
    own: &HashMap<String, String>,
    refs: &RefMap,
    now: i64,
) -> Result<()> {
    if !has_truth_tables(conn) {
        return Ok(());
    }
    let resolve = |r: &str| -> Option<(String, bool)> {
        let r = r.trim();
        if let Some(uid) = own.get(r) {
            return Some((uid.clone(), true));
        }
        refs.get(r).map(|e| (e.uid.clone(), false))
    };
    let (mut supersedes, mut contradicts) = (0, 0);
    for l in &d.links {
        let Some(kind) = LinkKind::parse(&l.kind) else {
            continue;
        };
        let used = match kind {
            LinkKind::Supersedes => &mut supersedes,
            LinkKind::Contradicts => &mut contradicts,
        };
        let cap = match kind {
            LinkKind::Supersedes => DISTILL_MAX_SUPERSEDES,
            LinkKind::Contradicts => DISTILL_MAX_CONTRADICTS,
        };
        if *used >= cap {
            continue;
        }
        let (Some((from, from_own)), Some((to, to_own))) = (resolve(&l.from), resolve(&l.to))
        else {
            continue;
        };
        // Exactly one side is this session's: it relates its own item to a listed one.
        if from_own == to_own {
            continue;
        }
        let listed = if from_own { &to } else { &from };
        // The user undid a link this session declared to this listed item: no re-distill of
        // the session relates any of its items to it again. Keyed on the listed side, whose
        // uid is stable: the session's own item gets a new uid when the model rewords it, and
        // pair_dismissed (exact pair) would then let the reworded item re-link. A dismissed
        // link two sessions both declared has no thread (NULL) and keeps exact-pair
        // protection only.
        let undone: bool = conn
            .prepare_cached(
                "SELECT EXISTS (SELECT 1 FROM fact_links WHERE dismissed_at IS NOT NULL
                   AND source = 'distill' AND thread_id = ?1 AND (from_uid = ?2 OR to_uid = ?2))",
            )?
            .query_row(params![thread_id, listed], |r| r.get(0))?;
        if undone {
            continue;
        }
        // An incoming link this session declared before (a listed item supersedes its own) is
        // kept across a re-distill (see store_distilled); declaring it again takes no new slot.
        let held: bool = conn
            .prepare_cached(
                "SELECT EXISTS (SELECT 1 FROM fact_links WHERE from_uid = ?1 AND to_uid = ?2
                   AND kind = ?3 AND source = 'distill' AND thread_id = ?4
                   AND dismissed_at IS NULL)",
            )?
            .query_row(params![from, to, kind.as_str(), thread_id], |r| r.get(0))?;
        let o = link_uids(
            conn,
            &from,
            &to,
            kind,
            LinkSource::Distill,
            Some(thread_id),
            now,
            true,
        )?;
        if o.link_id().is_some() && !held {
            *used += 1;
        }
    }
    let mut seen = HashSet::new();
    for r in d.reaffirms.iter().take(DISTILL_MAX_REAFFIRMS * 2) {
        if seen.len() >= DISTILL_MAX_REAFFIRMS {
            break;
        }
        let Some(e) = refs.get(r) else { continue };
        if seen.insert(e.uid.clone()) && fact_row(conn, true, &e.uid)?.is_some_and(|f| !f.hidden) {
            reaffirm_uid(conn, &e.uid, thread_id, now)?;
        }
    }
    let mut retired = 0;
    for r in &d.retires {
        if retired >= DISTILL_MAX_RETIRES {
            break;
        }
        let Some(e) = refs.get(r) else { continue };
        // An older session (a backlog distill) cannot retire a decision made after it ended,
        // the way a supersede may not point back in time.
        if refs.session_end().is_some_and(|end| e.decided_at > end) {
            continue;
        }
        let Some(f) = fact_row(conn, true, &e.uid)? else {
            continue;
        };
        if f.hidden {
            continue;
        }
        // The user kept this fact after this very session retired it once: a re-distill of
        // the same session must not raise the same flag again.
        let kept: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM fact_signals WHERE fact_uid = ?1 AND thread_id = ?2
               AND source = 'distill' AND kind IN ('stale', 'wrong')
               AND resolved_at IS NOT NULL)",
            params![f.uid, thread_id],
            |r| r.get(0),
        )?;
        if kept {
            continue;
        }
        let o = flag_row(
            conn,
            &f,
            FlagKind::Stale,
            SignalSource::Distill,
            Some("a later session shows it no longer holds"),
            Some(thread_id),
            now,
        )?;
        if matches!(o, FlagOutcome::Flagged { .. }) {
            retired += 1;
        }
    }
    Ok(())
}

/// The links of a published fact for repo memory: the uids each of `uids` actively supersedes,
/// sorted. Empty on an index without 0035's tables.
pub(crate) fn superseded_uids(
    conn: &Connection,
    uids: &[&str],
) -> Result<HashMap<String, Vec<String>>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    if uids.is_empty() || !has_truth_tables(conn) {
        return Ok(out);
    }
    let mut stmt = conn.prepare_cached(
        "SELECT lk.from_uid, lk.to_uid FROM json_each(?1) j
         JOIN fact_links lk ON lk.from_uid = j.value
         JOIN facts me ON me.uid = lk.from_uid
         JOIN facts f ON f.uid = lk.to_uid
         WHERE lk.kind = 'supersedes' AND lk.dismissed_at IS NULL AND me.hidden = 0",
    )?;
    let rows = stmt.query_map([serde_json::to_string(uids)?], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (from, to) = row?;
        out.entry(from).or_default().push(to);
    }
    for v in out.values_mut() {
        v.sort();
        v.dedup();
    }
    Ok(out)
}

/// Every `supersedes` target of `from_uid`, any source, with that source (`repo` for the
/// ones a repo import recorded), sorted by target. A repo import compares a line's
/// `supersedes` against all of them: a pair the user or an agent already linked is satisfied
/// as it is (the upsert keeps their row), and only `repo` rows are the import's to remove.
/// A link the user undid reports source `dismissed`: it satisfies the line (re-adding it
/// would be refused anyway) and is never the import's to remove, so the Undo outlives a
/// publisher who later drops the field and adds it back.
pub(crate) fn supersedes_links_from(
    conn: &Connection,
    from_uid: &str,
) -> Result<Vec<(String, String)>> {
    if !has_truth_tables(conn) {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare_cached(
        "SELECT to_uid, CASE WHEN dismissed_at IS NULL THEN source ELSE 'dismissed' END
         FROM fact_links WHERE from_uid = ?1 AND kind = 'supersedes' ORDER BY to_uid",
    )?;
    let rows = stmt
        .query_map([from_uid], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<(String, String)>>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{DistillLink, Distilled};
    use crate::knowledge::{
        check_contradiction, get_project_memory_facts, get_project_memory_with,
        get_thread_knowledge, record_fact_with_origin, set_fact_hidden, set_fact_pinned,
        store_distilled, FactOrigin, MemoryQuery,
    };

    const P: &str = "/proj/app";

    fn temp_db() -> Connection {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "calli_truth_{}_{}.db",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        crate::db::open(&p).unwrap()
    }

    fn day(m: u32, d: u32) -> i64 {
        chrono::NaiveDate::from_ymd_opt(2026, m, d)
            .unwrap()
            .and_hms_opt(9, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp()
    }

    /// A real (distillable) session of `project` that started at `started`.
    fn session(conn: &Connection, ext: &str, project: &str, started: i64) -> i64 {
        let sid = crate::indexer::source_id(conn, "claude_code").unwrap();
        conn.execute(
            "INSERT INTO threads (source_id, external_id, title, project_path, project_key,
                                  created_at, updated_at, message_count)
             VALUES (?1, ?2, ?2, ?3, ?3, ?4, ?4, 6)",
            params![sid, ext, project, started],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    /// A distilled fact on `tid`, distilled long after every session (as a backlog is).
    fn fact(conn: &Connection, tid: i64, kind: &str, text: &str) -> i64 {
        conn.execute(
            "INSERT INTO facts (thread_id, kind, text, extractor, created_at, uid)
             VALUES (?1, ?2, ?3, 'llm', ?4, ?5)",
            params![
                tid,
                kind,
                text,
                day(12, 1),
                crate::knowledge::new_fact_uid()
            ],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn id_of(conn: &Connection, text: &str) -> i64 {
        conn.query_row("SELECT id FROM facts WHERE text = ?1", [text], |r| r.get(0))
            .unwrap()
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn status(conn: &Connection, id: i64) -> FactStatus {
        let uid = uid_of(conn, id).unwrap().unwrap();
        fact_truth(conn, &[&uid]).unwrap()[&uid].status
    }

    /// `text -> ref` from a distill memory block (`D2 (2026-10-01): text`).
    fn refs_by_text(block: &str) -> HashMap<String, String> {
        block
            .lines()
            .filter_map(|l| {
                let (r, rest) = l.split_once(" (")?;
                let (_, text) = rest.split_once("): ")?;
                Some((text.to_string(), r.to_string()))
            })
            .collect()
    }

    fn link(from: &str, to: &str, kind: &str) -> DistillLink {
        DistillLink {
            from: from.into(),
            to: to.into(),
            kind: kind.into(),
        }
    }

    /// Distill `tid` the way the callers do: build the memory context, then store the
    /// model's output against its refs.
    fn distill(
        conn: &mut Connection,
        tid: i64,
        d: impl Fn(&HashMap<String, String>) -> Distilled,
        now: i64,
    ) {
        let (block, refs) = distill_memory_context(conn, tid).unwrap();
        let d = d(&refs_by_text(&block));
        store_distilled(conn, tid, &d, &refs, now).unwrap();
    }

    fn vector(conn: &Connection, id: i64, v: &[f32]) {
        conn.execute(
            "INSERT INTO vec_facts (fact_id, embedding) VALUES (?1, ?2)",
            params![id, crate::embed::vec_to_bytes(v)],
        )
        .unwrap();
    }

    const BINCODE: &str = "use bincode for the cache";
    const JSON: &str = "use JSON for the cache: bincode broke across versions";
    const ZSTD: &str = "compress log archives with zstd";

    /// The reviewer's scenario, at the knowledge and export level: an older session decided
    /// bincode, a newer one reversed it to JSON. `b_declares`: B is distilled after A and
    /// declares the supersede; otherwise the backlog order (newest first: B, then A), where
    /// A's distill links B's listed decision to A's own new item.
    fn reversed_decision(b_declares: bool) {
        let mut conn = temp_db();
        let (a_start, b_start) = (day(10, 1), day(10, 3));
        let ta = session(&conn, "A", P, a_start);
        let tb = session(&conn, "B", P, b_start);
        let a_out = |links: Vec<DistillLink>| Distilled {
            summary: "set up the cache".into(),
            decisions: vec![BINCODE.into(), ZSTD.into()],
            links,
            ..Default::default()
        };
        if b_declares {
            distill(&mut conn, ta, |_| a_out(Vec::new()), day(11, 2));
            distill(
                &mut conn,
                tb,
                |refs| Distilled {
                    decisions: vec![JSON.into()],
                    links: vec![link("d1", &refs[BINCODE], "supersedes")],
                    ..Default::default()
                },
                day(11, 1),
            );
        } else {
            distill(
                &mut conn,
                tb,
                |_| Distilled {
                    decisions: vec![JSON.into()],
                    ..Default::default()
                },
                day(11, 1),
            );
            distill(
                &mut conn,
                ta,
                |refs| a_out(vec![link(&refs[JSON], "d1", "supersedes")]),
                day(11, 2),
            );
        }
        let (bin, json) = (id_of(&conn, BINCODE), id_of(&conn, JSON));
        let zstd_uid = uid_of(&conn, id_of(&conn, ZSTD)).unwrap();

        // Project memory: only B's decision on the topic, dated by its session, naming the
        // date of what it replaced; ordered by session time although A was distilled last.
        let mem = get_project_memory_facts(&conn, P, 10).unwrap();
        let texts: Vec<&str> = mem.decisions.iter().map(|f| f.text.as_str()).collect();
        assert_eq!(texts, [JSON, ZSTD], "{texts:?}");
        let b = &mem.decisions[0];
        assert_eq!(b.decided_at, b_start);
        assert_eq!(b.replaces_decided_at, Some(a_start));
        assert_eq!(b.replaces, [bin]);
        assert_eq!(b.status, FactStatus::Current);
        assert_eq!(b.source, "claude_code");
        assert_eq!(mem.decisions[1].decided_at, a_start);
        // The hook block (HOOK budget) carries B and not A.
        let block = crate::export::agent_memory_md_budgeted(
            P,
            &mem,
            None,
            &crate::export::MemoryBudget::HOOK,
        );
        assert!(block.contains(JSON), "{block}");
        assert!(!block.contains(BINCODE), "{block}");

        // The guard: "use bincode" matches A best, but B (current) comes first; A is
        // labelled superseded and names B.
        let mut q = vec![0.0_f32; crate::embed::DIM];
        q[0] = 1.0;
        let mut near = q.clone();
        near[1] = 0.7; // cosine about 0.82
        vector(&conn, bin, &q);
        vector(&conn, json, &near);
        let hits = check_contradiction(&conn, &q, Some(P), 8).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!((hits[0].id, hits[0].status), (json, FactStatus::Current));
        assert_eq!(hits[0].replaces, vec![bin]);
        assert_eq!((hits[1].id, hits[1].status), (bin, FactStatus::Superseded));
        let by = hits[1].superseded_by.as_ref().unwrap();
        assert_eq!(
            (by.id, by.decided_at, by.thread_id),
            (json, b_start, Some(tb))
        );
        assert_eq!(hits[1].decided_at, a_start);
        assert_eq!(
            hits[1].match_strength,
            Some(crate::knowledge::GuardMatch::Close)
        );
        // Plain recall leaves the superseded decision out.
        let plain = crate::knowledge::recall(&conn, &q, "decision", Some(P), 8, false).unwrap();
        assert_eq!(plain.iter().map(|h| h.id).collect::<Vec<_>>(), [json]);
        let k = get_thread_knowledge(&conn, ta).unwrap();
        let kb = k.decisions.iter().find(|f| f.id == bin).unwrap();
        assert_eq!(
            (kb.status, kb.decided_at),
            (FactStatus::Superseded, a_start)
        );

        // Re-distilling A (same output, then one that leaves the link out, as an LLM may)
        // never brings A's decision back as current, and never duplicates it, whichever
        // session declared the link.
        for links in [true, false] {
            distill(
                &mut conn,
                ta,
                |refs| {
                    let mut d = a_out(Vec::new());
                    if !b_declares && links {
                        d.links = vec![link(&refs[JSON], "d1", "supersedes")];
                    }
                    d
                },
                day(11, 5),
            );
            assert_eq!(
                count(
                    &conn,
                    "SELECT COUNT(*) FROM facts WHERE text = 'use bincode for the cache'"
                ),
                1
            );
            assert_eq!(status(&conn, id_of(&conn, BINCODE)), FactStatus::Superseded);
            let mem = get_project_memory_facts(&conn, P, 10).unwrap();
            assert!(mem.decisions.iter().all(|f| f.text != BINCODE));
            let block = crate::export::agent_memory_md_budgeted(
                P,
                &mem,
                None,
                &crate::export::MemoryBudget::HOOK,
            );
            assert!(!block.contains(BINCODE), "{block}");
            assert_eq!(block.matches(JSON).count(), 1, "{block}");
        }
        if b_declares {
            distill(
                &mut conn,
                ta,
                |_| Distilled {
                    decisions: vec![ZSTD.into()],
                    ..Default::default()
                },
                day(11, 6),
            );
            assert_eq!(
                status(&conn, bin),
                FactStatus::Superseded,
                "kept as history"
            );
            assert_eq!(id_of(&conn, BINCODE), bin);
        }
        let hist = get_project_memory_with(
            &conn,
            P,
            &MemoryQuery {
                per_kind: 10,
                history: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(hist.superseded.len(), 1);
        assert_eq!(hist.superseded[0].text, BINCODE);
        assert_eq!(
            hist.superseded[0].superseded_by.as_ref().map(|f| f.id),
            Some(json)
        );
        assert_eq!(
            zstd_uid,
            uid_of(&conn, id_of(&conn, ZSTD)).unwrap(),
            "a fact distilled again keeps its uid"
        );
    }

    #[test]
    fn a_reversal_declared_by_the_newer_session_replaces_the_older_decision() {
        reversed_decision(true);
    }

    #[test]
    fn a_reversal_found_while_distilling_the_older_session_in_a_backlog() {
        reversed_decision(false);
    }

    #[test]
    fn distilled_links_are_validated_and_capped() {
        let mut conn = temp_db();
        let old = session(&conn, "old", P, day(9, 1));
        let olds: Vec<String> = (1..=6).map(|i| format!("old decision {i}")).collect();
        for t in &olds {
            fact(&conn, old, "decision", t);
        }
        fact(&conn, old, "gotcha", "old gotcha");
        let newer = session(&conn, "newer", P, day(10, 20));
        fact(&conn, newer, "decision", "newer decision");
        let other = session(&conn, "other", "/proj/other", day(9, 1));
        fact(&conn, other, "decision", "another project's decision");
        let tid = session(&conn, "now", P, day(10, 10));

        let out = |r: &HashMap<String, String>| {
            assert!(!r.contains_key("another project's decision"));
            let o = |i: usize| r[&format!("old decision {i}")].clone();
            let mut links = vec![
                link("d1", "D99", "supersedes"),                // no such item
                link("x1", &o(1), "supersedes"),                // not a ref
                link("d1", &r["old gotcha"], "supersedes"),     // kind mismatch
                link("d1", "d1", "supersedes"),                 // self
                link("d1", "d2", "supersedes"),                 // within the session
                link(&o(1), &o(2), "supersedes"),               // two listed items
                link("d2", &r["newer decision"], "supersedes"), // points back in time
                link("d1", &o(1), "replaces"),                  // unknown kind
            ];
            links.extend((1..=5).map(|i| link("d1", &o(i), "supersedes")));
            links.extend([4, 5, 6, 1].map(|i| link("d2", &o(i), "contradicts")));
            Distilled {
                decisions: vec!["new decision one".into(), "new decision two".into()],
                gotchas: vec!["new gotcha".into()],
                links,
                reaffirms: (1..=6)
                    .map(o)
                    .chain(["D99".to_string(), r["newer decision"].clone()])
                    .collect(),
                retires: vec![
                    "G99".into(),
                    r["old gotcha"].clone(),
                    o(4),
                    o(5),
                    o(6),
                    r["newer decision"].clone(),
                ],
                ..Default::default()
            }
        };
        let counts = |conn: &Connection| {
            let q = |sql: &str| count(conn, &sql.replace("TID", &tid.to_string()));
            (
                q("SELECT COUNT(*) FROM fact_links WHERE thread_id = TID AND kind = 'supersedes'"),
                q("SELECT COUNT(*) FROM fact_links WHERE thread_id = TID AND kind = 'contradicts'"),
                q("SELECT COUNT(*) FROM fact_signals WHERE thread_id = TID AND kind = 'reaffirmed'"),
                q("SELECT COUNT(*) FROM fact_signals WHERE thread_id = TID AND kind = 'stale'
                     AND resolved_at IS NULL"),
            )
        };
        distill(&mut conn, tid, out, day(11, 1));
        assert_eq!(counts(&conn), (3, 3, 5, 3));
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM fact_links WHERE source != 'distill'"
            ),
            0
        );
        let newer_id = id_of(&conn, "newer decision");
        assert_eq!(
            status(&conn, newer_id),
            FactStatus::Current,
            "not retired: capped"
        );
        for i in 1..=3 {
            let id = id_of(&conn, &format!("old decision {i}"));
            assert_eq!(status(&conn, id), FactStatus::Superseded);
        }
        let d2 = id_of(&conn, "new decision two");
        assert_eq!(status(&conn, d2), FactStatus::Disputed);
        // Reaffirmed by one other session: support 2, confirmed at that session's start.
        let o4 = id_of(&conn, "old decision 4");
        let uid = uid_of(&conn, o4).unwrap().unwrap();
        let t = &fact_truth(&conn, &[&uid]).unwrap()[&uid];
        assert_eq!((t.support, t.last_confirmed), (2, Some(day(10, 10))));

        // A re-distill of the same output is idempotent.
        distill(&mut conn, tid, out, day(11, 2));
        assert_eq!(counts(&conn), (3, 3, 5, 3));
        // The user keeps a retired gotcha: a re-distill of the same session does not flag it
        // again.
        let g = id_of(&conn, "old gotcha");
        assert_eq!(status(&conn, g), FactStatus::Flagged);
        assert_eq!(resolve_fact_flags(&conn, g, day(11, 3)).unwrap(), 1);
        distill(&mut conn, tid, out, day(11, 4));
        assert_eq!(status(&conn, g), FactStatus::Current);
        // The cap counts what was stored: the next listed retirement takes the slot.
        assert_eq!(counts(&conn).3, 3);
        assert_eq!(
            status(&conn, id_of(&conn, "old decision 6")),
            FactStatus::Flagged
        );
    }

    #[test]
    fn machine_links_to_trusted_facts_are_downgraded_and_user_links_are_not() {
        let conn = temp_db();
        let old = session(&conn, "old", P, day(9, 1));
        let pinned = fact(&conn, old, "decision", "pinned decision");
        set_fact_pinned(&conn, pinned, true).unwrap();
        let edited = fact(&conn, old, "decision", "edited decision");
        crate::knowledge::edit_fact(&conn, edited, "edited decision, reworded").unwrap();
        let plain = fact(&conn, old, "decision", "plain decision");
        let newer = session(&conn, "new", P, day(10, 1));
        let n = fact(&conn, newer, "decision", "newer decision");

        let o = link_facts(
            &conn,
            n,
            pinned,
            LinkKind::Supersedes,
            LinkSource::Distill,
            Some(newer),
            1,
        )
        .unwrap();
        assert!(matches!(o, LinkOutcome::Downgraded { .. }), "{o:?}");
        assert_eq!(o.stored_kind(), Some(LinkKind::Contradicts));
        assert_eq!(status(&conn, pinned), FactStatus::Disputed);
        assert_eq!(status(&conn, n), FactStatus::Disputed);
        let o = link_facts(
            &conn,
            n,
            edited,
            LinkKind::Supersedes,
            LinkSource::Agent,
            None,
            1,
        )
        .unwrap();
        assert!(matches!(o, LinkOutcome::Downgraded { .. }), "{o:?}");

        // An agent's `replaces`: downgraded for a pinned target, stored for a plain one.
        let rec = |text: &str, origin, replaces| {
            record_fact_with_origin(
                &conn,
                P,
                "decision",
                text,
                None,
                origin,
                Some(replaces),
                day(11, 1),
            )
            .unwrap()
            .link
            .unwrap()
        };
        assert!(matches!(
            rec("agent replaces pinned", FactOrigin::Agent, pinned),
            LinkOutcome::Downgraded { .. }
        ));
        assert!(matches!(
            rec("agent replaces plain", FactOrigin::Agent, plain),
            LinkOutcome::Linked {
                kind: LinkKind::Supersedes,
                ..
            }
        ));
        assert_eq!(status(&conn, plain), FactStatus::Superseded);

        // The user's link is stored as asked, settles the pair (its contradicts goes) and
        // takes the pinned fact out of memory.
        let o = link_facts(
            &conn,
            n,
            pinned,
            LinkKind::Supersedes,
            LinkSource::User,
            None,
            2,
        )
        .unwrap();
        assert!(
            matches!(
                o,
                LinkOutcome::Linked {
                    kind: LinkKind::Supersedes,
                    ..
                }
            ),
            "{o:?}"
        );
        assert_eq!(status(&conn, pinned), FactStatus::Superseded);
        let n_uid = uid_of(&conn, n).unwrap().unwrap();
        let p_uid = uid_of(&conn, pinned).unwrap().unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM fact_links WHERE kind = 'contradicts'
                   AND ((from_uid = ?1 AND to_uid = ?2) OR (from_uid = ?2 AND to_uid = ?1))",
                [&n_uid, &p_uid],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        let mem = get_project_memory_facts(&conn, P, 20).unwrap();
        assert!(mem
            .decisions
            .iter()
            .all(|f| f.id != pinned && f.id != plain));
        // A user's record with `replaces` is a user link: no downgrade.
        assert!(matches!(
            rec("user replaces edited", FactOrigin::User, edited),
            LinkOutcome::Linked {
                kind: LinkKind::Supersedes,
                ..
            }
        ));
    }

    #[test]
    fn links_refuse_cycles_self_cross_project_and_hidden_facts() {
        let conn = temp_db();
        let t = session(&conn, "s", P, day(9, 1));
        let a = fact(&conn, t, "decision", "a");
        let b = fact(&conn, t, "decision", "b");
        let c = fact(&conn, t, "decision", "c");
        let g = fact(&conn, t, "gotcha", "g");
        let todo = fact(&conn, t, "todo", "a todo");
        let x = fact(
            &conn,
            session(&conn, "x", "/proj/other", day(9, 1)),
            "decision",
            "x",
        );
        let user =
            |from, to, kind| link_facts(&conn, from, to, kind, LinkSource::User, None, 1).unwrap();
        let rejected = |o: LinkOutcome| match o {
            LinkOutcome::Rejected { reason } => reason,
            other => panic!("stored: {other:?}"),
        };
        assert!(user(b, a, LinkKind::Supersedes).link_id().is_some());
        assert!(user(c, b, LinkKind::Supersedes).link_id().is_some());
        assert_eq!(
            rejected(user(a, b, LinkKind::Supersedes)),
            LinkRejection::Cycle
        );
        assert_eq!(
            rejected(user(a, c, LinkKind::Supersedes)),
            LinkRejection::Cycle
        );
        assert_eq!(
            rejected(user(a, a, LinkKind::Supersedes)),
            LinkRejection::SelfLink
        );
        assert_eq!(
            rejected(user(c, x, LinkKind::Supersedes)),
            LinkRejection::CrossProject
        );
        assert_eq!(
            rejected(user(c, g, LinkKind::Contradicts)),
            LinkRejection::KindMismatch
        );
        assert_eq!(
            rejected(user(todo, a, LinkKind::Supersedes)),
            LinkRejection::NotLinkable
        );
        assert_eq!(
            rejected(user(b, a, LinkKind::Contradicts)),
            LinkRejection::AlreadySettled
        );
        assert_eq!(
            rejected(user(c, 9_999, LinkKind::Supersedes)),
            LinkRejection::NotFound
        );
        // Asserting a link again returns the same row.
        assert_eq!(
            user(b, a, LinkKind::Supersedes).link_id(),
            user(b, a, LinkKind::Supersedes).link_id()
        );

        assert_eq!(status(&conn, b), FactStatus::Superseded);
        // A hidden newer side supersedes nothing; a hidden fact links to nothing.
        set_fact_hidden(&conn, c, true).unwrap();
        assert_eq!(status(&conn, b), FactStatus::Current);
        assert_eq!(
            rejected(user(c, a, LinkKind::Supersedes)),
            LinkRejection::NotFound
        );
        set_fact_hidden(&conn, c, false).unwrap();
        assert_eq!(
            unlink_facts(&conn, c, b, Some(LinkKind::Supersedes), 5).unwrap(),
            1
        );
        assert_eq!(status(&conn, b), FactStatus::Current);
        let id = user(c, b, LinkKind::Supersedes).link_id().unwrap();
        assert!(unlink_link(&conn, id, 6).unwrap());
        assert!(!unlink_link(&conn, id, 7).unwrap());
        // A link whose fact is gone (a re-distill replaced it) is ignored on read.
        conn.execute("DELETE FROM facts WHERE id = ?1", [b])
            .unwrap();
        assert_eq!(status(&conn, a), FactStatus::Current);
    }

    #[test]
    fn agent_flags_are_capped_and_never_hide_a_pinned_fact() {
        let conn = temp_db();
        let ids: Vec<i64> = (0..14)
            .map(|i| {
                record_fact_with_origin(
                    &conn,
                    P,
                    "decision",
                    &format!("decision {i}"),
                    None,
                    FactOrigin::Agent,
                    None,
                    10 + i,
                )
                .unwrap()
                .id
            })
            .collect();
        let pinned = record_fact_with_origin(
            &conn,
            P,
            "decision",
            "the user's decision",
            None,
            FactOrigin::User,
            None,
            5,
        )
        .unwrap()
        .id;
        let flag = |id, source| {
            flag_fact(
                &conn,
                id,
                FlagKind::Stale,
                source,
                Some("the code moved on"),
                None,
                50,
            )
            .unwrap()
        };
        // The pinned one first: flagged, yet still injected, and not counted (it hides
        // nothing), so ten more fit under the cap.
        assert!(matches!(
            flag(pinned, SignalSource::Agent),
            FlagOutcome::Flagged { .. }
        ));
        for id in &ids[..10] {
            assert!(matches!(
                flag(*id, SignalSource::Agent),
                FlagOutcome::Flagged { .. }
            ));
        }
        assert_eq!(flag(ids[10], SignalSource::Agent), FlagOutcome::CapReached);
        assert!(matches!(
            flag(ids[0], SignalSource::Agent),
            FlagOutcome::AlreadyFlagged { .. }
        ));
        assert!(matches!(
            flag(ids[11], SignalSource::User),
            FlagOutcome::Flagged { .. }
        ));
        assert_eq!(flag(9_999, SignalSource::Agent), FlagOutcome::NotFound);

        let mem = get_project_memory_facts(&conn, P, 40).unwrap();
        let live: Vec<i64> = mem.decisions.iter().map(|f| f.id).collect();
        assert_eq!(
            live,
            [pinned, ids[13], ids[12], ids[10]],
            "flagged facts leave memory"
        );
        assert_eq!(mem.decisions[0].status, FactStatus::Flagged);
        assert_eq!(
            mem.decisions[0].flags[0].reason.as_deref(),
            Some("the code moved on")
        );
        let hist = get_project_memory_with(
            &conn,
            P,
            &MemoryQuery {
                per_kind: 40,
                history: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            hist.flagged.len(),
            12,
            "the agent's ten, the pinned one and the user's one"
        );
        let pinned_in = |l: &[crate::knowledge::MemoryFact]| l.iter().any(|f| f.id == pinned);
        assert!(pinned_in(&hist.decisions) && pinned_in(&hist.flagged));
        // Listed in both groups, its flag is reported once (one status lookup per uid).
        let f = hist.flagged.iter().find(|f| f.id == pinned).unwrap();
        assert_eq!(f.flags.len(), 1, "{:?}", f.flags);

        // The user keeps one: it is back, and the agent has room for another flag.
        assert_eq!(resolve_fact_flags(&conn, ids[3], 60).unwrap(), 1);
        assert_eq!(status(&conn, ids[3]), FactStatus::Current);
        let mem = get_project_memory_facts(&conn, P, 40).unwrap();
        assert!(mem.decisions.iter().any(|f| f.id == ids[3]));
        assert!(matches!(
            flag(ids[10], SignalSource::Agent),
            FlagOutcome::Flagged { .. }
        ));
        assert_eq!(flag(ids[12], SignalSource::Agent), FlagOutcome::CapReached);

        // The Flagged group's Hide resolves nothing, but a flag on a fact the user hid hides
        // nothing either: it stops counting, so ten Hides cannot lock flag_fact out.
        set_fact_hidden(&conn, ids[0], true).unwrap();
        assert!(matches!(
            flag(ids[12], SignalSource::Agent),
            FlagOutcome::Flagged { .. }
        ));
        // Same for a flagged fact another fact replaced.
        let newer = record_fact_with_origin(
            &conn,
            P,
            "decision",
            "the replacement",
            None,
            FactOrigin::Agent,
            Some(ids[1]),
            100,
        )
        .unwrap();
        assert_eq!(status(&conn, ids[1]), FactStatus::Superseded);
        assert!(matches!(
            flag(newer.id, SignalSource::Agent),
            FlagOutcome::Flagged { .. }
        ));
        let another = record_fact_with_origin(
            &conn,
            P,
            "decision",
            "one more",
            None,
            FactOrigin::Agent,
            None,
            101,
        )
        .unwrap()
        .id;
        assert_eq!(flag(another, SignalSource::Agent), FlagOutcome::CapReached);
    }

    #[test]
    fn links_reach_another_clone_of_the_same_repo() {
        let conn = temp_db();
        let here = fact(&conn, session(&conn, "h", P, day(9, 2)), "decision", "here");
        let clone = "/elsewhere/app";
        let there = fact(
            &conn,
            session(&conn, "c", clone, day(9, 1)),
            "decision",
            "there",
        );
        let user = |from, to| {
            link_facts(
                &conn,
                from,
                to,
                LinkKind::Supersedes,
                LinkSource::User,
                None,
                1,
            )
            .unwrap()
        };
        assert_eq!(
            user(here, there),
            LinkOutcome::Rejected {
                reason: LinkRejection::CrossProject
            },
            "two unrelated projects"
        );
        // Project memory lists a fact this project borrows from another clone beside its
        // own, so the two link.
        let there_uid = uid_of(&conn, there).unwrap().unwrap();
        let borrowed = serde_json::json!({ P: { "/elsewhere/app": [there_uid] } });
        crate::knowledge::app_config::put(
            &conn,
            crate::knowledge::repo_memory::BORROWED,
            &borrowed.to_string(),
        )
        .unwrap();
        assert!(user(here, there).link_id().is_some());
        assert_eq!(status(&conn, there), FactStatus::Superseded);

        // Clones with the same portable id are one repo too.
        let other = "/third/app";
        let third = fact(
            &conn,
            session(&conn, "t", other, day(9, 1)),
            "decision",
            "third",
        );
        assert!(user(here, third).link_id().is_none());
        for key in [P, other] {
            conn.execute(
                "INSERT INTO projects (key, portable_id, refreshed_at)
                 VALUES (?1, 'github.com/o/app', 1)",
                [key],
            )
            .unwrap();
        }
        assert!(user(here, third).link_id().is_some());
    }

    /// One Undo restores a fact however many facts replaced it: every replacement link onto it
    /// is dismissed together (a hidden replacer's too, so unhiding it takes nothing back), each
    /// one stays undone, and a failure on one leaves every link as it was.
    #[test]
    fn restoring_a_fact_undoes_every_replacement_at_once() {
        let conn = temp_db();
        let a = fact(
            &conn,
            session(&conn, "A", P, day(10, 1)),
            "decision",
            BINCODE,
        );
        let b = fact(&conn, session(&conn, "B", P, day(10, 3)), "decision", JSON);
        let c = fact(
            &conn,
            session(&conn, "C", P, day(10, 5)),
            "decision",
            "Use protobuf",
        );
        let d = fact(
            &conn,
            session(&conn, "D", P, day(10, 6)),
            "decision",
            "Use CBOR",
        );
        for (from, source) in [
            (b, LinkSource::User),
            (c, LinkSource::Agent),
            (d, LinkSource::User),
        ] {
            let o = link_facts(
                &conn,
                from,
                a,
                LinkKind::Supersedes,
                source,
                None,
                day(11, 1),
            );
            assert!(matches!(o.unwrap(), LinkOutcome::Linked { .. }));
        }
        set_fact_hidden(&conn, d, true).unwrap();
        assert_eq!(status(&conn, a), FactStatus::Superseded);
        // The pairwise Undo: the fact stays replaced by the others.
        assert_eq!(
            unlink_facts(&conn, c, a, Some(LinkKind::Supersedes), day(11, 2)).unwrap(),
            1
        );
        assert_eq!(status(&conn, a), FactStatus::Superseded);

        let active =
            "SELECT COUNT(*) FROM fact_links WHERE kind = 'supersedes' AND dismissed_at IS NULL";
        let b_uid = uid_of(&conn, b).unwrap().unwrap();
        conn.execute_batch(&format!(
            "CREATE TEMP TRIGGER refuse_one BEFORE UPDATE OF dismissed_at ON fact_links
             WHEN OLD.from_uid = '{b_uid}' BEGIN SELECT RAISE(ABORT, 'refused'); END;"
        ))
        .unwrap();
        assert!(restore_fact(&conn, a, day(11, 3)).is_err());
        assert_eq!(count(&conn, active), 2, "all or nothing");
        conn.execute_batch("DROP TRIGGER refuse_one").unwrap();

        assert_eq!(restore_fact(&conn, a, day(11, 3)).unwrap(), 2);
        assert_eq!(status(&conn, a), FactStatus::Current);
        assert_eq!(count(&conn, active), 0);
        set_fact_hidden(&conn, d, false).unwrap();
        assert_eq!(status(&conn, a), FactStatus::Current);
        let again = link_facts(
            &conn,
            c,
            a,
            LinkKind::Supersedes,
            LinkSource::Agent,
            None,
            day(11, 4),
        );
        assert!(matches!(
            again.unwrap(),
            LinkOutcome::Rejected {
                reason: LinkRejection::Dismissed
            }
        ));
        assert_eq!(restore_fact(&conn, a, day(11, 5)).unwrap(), 0);
        assert_eq!(restore_fact(&conn, 9_999, day(11, 5)).unwrap(), 0);
    }

    /// The user's Undo sticks: the link stays as a dismissed row, every reader treats it as
    /// inactive, a re-distill of the session that declared it (and an agent or a repo import
    /// asserting the pair, either way round) is a no-op, and only a user link revives it.
    #[test]
    fn an_undone_link_stays_undone_until_the_user_links_it_again() {
        let mut conn = temp_db();
        let ta = session(&conn, "A", P, day(10, 1));
        let tb = session(&conn, "B", P, day(10, 3));
        let a_out = || Distilled {
            decisions: vec![BINCODE.into()],
            ..Default::default()
        };
        let b_out = |refs: &HashMap<String, String>| Distilled {
            decisions: vec![JSON.into()],
            links: vec![link("d1", &refs[BINCODE], "supersedes")],
            ..Default::default()
        };
        distill(&mut conn, ta, |_| a_out(), day(11, 1));
        distill(&mut conn, tb, b_out, day(11, 2));
        let (bin, json) = (id_of(&conn, BINCODE), id_of(&conn, JSON));
        assert_eq!(status(&conn, bin), FactStatus::Superseded);

        assert_eq!(
            unlink_facts(&conn, json, bin, Some(LinkKind::Supersedes), day(11, 3)).unwrap(),
            1
        );
        assert_eq!(status(&conn, bin), FactStatus::Current);
        let dismissed = "SELECT COUNT(*) FROM fact_links WHERE dismissed_at IS NOT NULL";
        assert_eq!(count(&conn, dismissed), 1, "kept, not deleted");
        assert_eq!(status(&conn, json), FactStatus::Current);
        let json_truth = &fact_truth(&conn, &[&uid_of(&conn, json).unwrap().unwrap()]).unwrap();
        assert!(json_truth.values().all(|t| t.replaces.is_empty()));

        // Re-distilling either session declares nothing back.
        distill(&mut conn, tb, b_out, day(11, 4));
        distill(&mut conn, ta, |_| a_out(), day(11, 5));
        let (bin, json) = (id_of(&conn, BINCODE), id_of(&conn, JSON));
        assert_eq!(status(&conn, bin), FactStatus::Current);
        assert_eq!(count(&conn, dismissed), 1);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM fact_links"), 1);
        let mem = get_project_memory_facts(&conn, P, 10).unwrap();
        assert_eq!(mem.decisions.len(), 2);
        // The distiller is shown the item again, as it is current.
        let (block, _) = distill_memory_context(&conn, tb).unwrap();
        assert!(block.contains(BINCODE), "{block}");

        // An agent, the distiller the other way round and a repo import are no-ops too.
        let rejected = |o: LinkOutcome| match o {
            LinkOutcome::Rejected { reason } => reason,
            other => panic!("stored {other:?}"),
        };
        let at = day(11, 6);
        assert_eq!(
            rejected(
                link_facts(
                    &conn,
                    json,
                    bin,
                    LinkKind::Supersedes,
                    LinkSource::Agent,
                    None,
                    at
                )
                .unwrap()
            ),
            LinkRejection::Dismissed
        );
        assert_eq!(
            rejected(
                link_facts(
                    &conn,
                    bin,
                    json,
                    LinkKind::Contradicts,
                    LinkSource::Distill,
                    Some(ta),
                    at
                )
                .unwrap()
            ),
            LinkRejection::Dismissed
        );
        let (bin_uid, json_uid) = (
            uid_of(&conn, bin).unwrap().unwrap(),
            uid_of(&conn, json).unwrap().unwrap(),
        );
        assert_eq!(
            rejected(
                link_uids(
                    &conn,
                    &json_uid,
                    &bin_uid,
                    LinkKind::Supersedes,
                    LinkSource::Repo,
                    None,
                    at,
                    false
                )
                .unwrap()
            ),
            LinkRejection::Dismissed
        );
        // Repo memory neither publishes it nor treats it as the import's to remove.
        assert!(superseded_uids(&conn, &[&json_uid]).unwrap().is_empty());
        assert_eq!(
            supersedes_links_from(&conn, &json_uid).unwrap(),
            [(bin_uid.clone(), "dismissed".to_string())]
        );
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM fact_links"), 1);

        // The user links it again: the same row, active, theirs.
        let o = link_facts(
            &conn,
            json,
            bin,
            LinkKind::Supersedes,
            LinkSource::User,
            None,
            at,
        )
        .unwrap();
        let id = o.link_id().unwrap();
        assert_eq!(o.stored_kind(), Some(LinkKind::Supersedes));
        assert_eq!(count(&conn, dismissed), 0);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM fact_links"), 1);
        assert_eq!(status(&conn, bin), FactStatus::Superseded);
        let source: String = conn
            .query_row("SELECT source FROM fact_links WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(source, "user");

        // Undo by id; then the user may link the pair the other way (no cycle through it).
        assert!(unlink_link(&conn, id, at).unwrap());
        assert!(!unlink_link(&conn, id, at).unwrap(), "already undone");
        assert!(matches!(
            link_facts(
                &conn,
                bin,
                json,
                LinkKind::Supersedes,
                LinkSource::User,
                None,
                at
            )
            .unwrap(),
            LinkOutcome::Linked { .. }
        ));
        assert_eq!(status(&conn, json), FactStatus::Superseded);
        assert_eq!(status(&conn, bin), FactStatus::Current);

        // A dispute undone one way is revived by the user from either side (with the current
        // `bin`: a superseded fact disputes nothing).
        let z = fact(&conn, tb, "decision", ZSTD);
        let c = link_facts(
            &conn,
            z,
            bin,
            LinkKind::Contradicts,
            LinkSource::User,
            None,
            at,
        )
        .unwrap()
        .link_id()
        .unwrap();
        assert!(unlink_link(&conn, c, at).unwrap());
        assert_eq!(status(&conn, z), FactStatus::Current);
        let again = link_facts(
            &conn,
            bin,
            z,
            LinkKind::Contradicts,
            LinkSource::User,
            None,
            at,
        )
        .unwrap()
        .link_id();
        assert_eq!(again, Some(c));
        assert_eq!(status(&conn, z), FactStatus::Disputed);
    }

    /// What every distill caller runs, minus the LLM: load the input (transcript and existing
    /// memory), send its prompt, store the answer against the input's refs.
    #[test]
    fn distilling_through_the_callers_input_stores_the_declared_link() {
        let mut conn = temp_db();
        let ta = session(&conn, "A", P, day(10, 1));
        let tb = session(&conn, "B", P, day(10, 3));
        let bin = fact(&conn, ta, "decision", BINCODE);
        let input = crate::agent::DistillInput::load(&conn, tb, 10_000)
            .unwrap()
            .unwrap();
        let prompt = input.user_prompt();
        assert!(
            prompt.contains("<existing_memory session_started=\"2026-10-03\">"),
            "{prompt}"
        );
        assert!(
            prompt.contains(&format!("D1 (2026-10-01): {BINCODE}")),
            "{prompt}"
        );
        assert!(crate::agent::DistillInput::load(&conn, 9_999, 10_000)
            .unwrap()
            .is_none());

        let answer = format!(
            r#"{{"summary": "s", "decisions": ["{JSON}"], "gotchas": [],
                "links": [{{"from": "d1", "to": "D1", "kind": "supersedes"}}]}}"#
        );
        let d = crate::agent::parse_distilled(&answer);
        store_distilled(&mut conn, tb, &d, &input.refs, day(11, 1)).unwrap();
        assert_eq!(status(&conn, bin), FactStatus::Superseded);
        let row: (String, i64) = conn
            .query_row("SELECT source, thread_id FROM fact_links", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(row, ("distill".to_string(), tb));
    }

    #[test]
    fn a_link_two_sessions_distilled_survives_either_one_alone() {
        let mut conn = temp_db();
        let ta = session(&conn, "A", P, day(10, 1));
        let tb = session(&conn, "B", P, day(10, 3));
        let a_out = |links: Vec<DistillLink>| Distilled {
            decisions: vec![BINCODE.into()],
            links,
            ..Default::default()
        };
        distill(&mut conn, ta, |_| a_out(Vec::new()), day(11, 1));
        // B declares the reversal; then A is distilled again and declares it from its side.
        distill(
            &mut conn,
            tb,
            |refs| Distilled {
                decisions: vec![JSON.into()],
                links: vec![link("d1", &refs[BINCODE], "supersedes")],
                ..Default::default()
            },
            day(11, 2),
        );
        distill(
            &mut conn,
            ta,
            |refs| a_out(vec![link(&refs[JSON], "d1", "supersedes")]),
            day(11, 3),
        );
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM fact_links WHERE thread_id IS NULL"
            ),
            1,
            "owned by neither session"
        );
        // B is distilled again without repeating it: A still asserts it, so it holds.
        distill(
            &mut conn,
            tb,
            |_| Distilled {
                decisions: vec![JSON.into()],
                ..Default::default()
            },
            day(11, 4),
        );
        assert_eq!(status(&conn, id_of(&conn, BINCODE)), FactStatus::Superseded);
        let mem = get_project_memory_facts(&conn, P, 10).unwrap();
        let texts: Vec<&str> = mem.decisions.iter().map(|f| f.text.as_str()).collect();
        assert_eq!(texts, [JSON]);
    }

    #[test]
    fn a_reaffirmation_from_a_deleted_session_no_longer_ranks_the_fact() {
        let conn = temp_db();
        let old = fact(&conn, session(&conn, "o", P, day(10, 1)), "decision", "old");
        let new = fact(&conn, session(&conn, "n", P, day(10, 5)), "decision", "new");
        let by = session(&conn, "r", P, day(10, 8));
        let uid = uid_of(&conn, old).unwrap().unwrap();
        reaffirm_uid(&conn, &uid, by, day(12, 1)).unwrap();
        let order = |conn: &Connection| -> Vec<i64> {
            get_project_memory_facts(conn, P, 10)
                .unwrap()
                .decisions
                .iter()
                .map(|f| f.id)
                .collect()
        };
        assert_eq!(order(&conn), [old, new], "confirmed on 10-08");
        assert_eq!(
            fact_truth(&conn, &[&uid]).unwrap()[&uid].last_confirmed,
            Some(day(10, 8))
        );
        // The reaffirming thread is deleted (ON DELETE SET NULL): its distill time (12-01)
        // must not rank the fact, and the ordering agrees with what fact_truth reports.
        conn.execute("UPDATE fact_signals SET thread_id = NULL", [])
            .unwrap();
        assert_eq!(
            fact_truth(&conn, &[&uid]).unwrap()[&uid].last_confirmed,
            None
        );
        assert_eq!(order(&conn), [new, old]);
    }

    #[test]
    fn recording_with_replaces_writes_all_or_nothing() {
        let conn = temp_db();
        let old = fact(&conn, session(&conn, "o", P, day(10, 1)), "decision", "old");
        let record = |conn: &Connection| {
            record_fact_with_origin(
                conn,
                P,
                "decision",
                "the replacement",
                None,
                FactOrigin::Agent,
                Some(old),
                day(11, 1),
            )
        };
        // The link step fails (here: its table is gone): the fact is not left behind for a
        // retrying agent to record twice.
        conn.execute_batch("ALTER TABLE fact_links RENAME TO fact_links_gone")
            .unwrap();
        assert!(record(&conn).is_err());
        let recorded = |conn: &Connection| {
            count(
                conn,
                "SELECT COUNT(*) FROM facts WHERE text = 'the replacement'",
            )
        };
        assert_eq!(recorded(&conn), 0);
        // The same inside a caller's transaction (a savepoint), which stays usable.
        let tx = conn.unchecked_transaction().unwrap();
        assert!(record(&tx).is_err());
        assert_eq!(recorded(&tx), 0);
        tx.commit().unwrap();
        conn.execute_batch("ALTER TABLE fact_links_gone RENAME TO fact_links")
            .unwrap();
        assert!(record(&conn).unwrap().link.unwrap().link_id().is_some());
        assert_eq!(recorded(&conn), 1);
    }

    /// The distiller is shown no unpinned playbook (`MemoryQuery::playbooks`): they are a
    /// repo's newest gotchas and would take the slots of the ones a session may supersede or
    /// reaffirm. A pinned one is the user's and is listed.
    #[test]
    fn the_distiller_sees_no_unpinned_playbook() {
        use crate::export::digest_test_support as dts;
        let mut conn = dts::temp_db("distill_playbooks");
        let old = dts::session(&mut conn, "dp-old", P, "old", 90_000, &["go"]);
        let gotcha = "The staging proxy strips auth headers";
        dts::fact(&conn, old, "gotcha", gotcha, "llm");
        let fixer = dts::session(&mut conn, "dp-fix", P, "fix", 6_000, &["fix"]);
        let now = chrono::Utc::now().timestamp();
        for (origin, text) in [
            (
                "playbook:a@/proj/app",
                "error[E0308]: mismatched types: seen 2 times; fixed in abc1234",
            ),
            (
                "playbook:b@/proj/app",
                "error[E0061]: wrong number of arguments: seen 3 times; fixed in def5678",
            ),
        ] {
            crate::knowledge::upsert_playbook(&conn, origin, fixer, text, now).unwrap();
        }
        conn.execute(
            "UPDATE facts SET pinned = 1 WHERE origin_ref = 'playbook:b@/proj/app'",
            [],
        )
        .unwrap();
        let next = dts::session(&mut conn, "dp-next", P, "next", 60, &["next"]);
        let (block, _) = distill_memory_context(&conn, next).unwrap();
        assert!(block.contains(gotcha), "{block}");
        assert!(block.contains("wrong number of arguments"), "{block}");
        assert!(!block.contains("mismatched types"), "{block}");
    }

    /// A pass from before playbooks left the distiller's list retired one, superseded
    /// another and disputed two more (one from each side): re-distilling that thread still
    /// lists all four, so it declares them again, and none comes back as current. An
    /// untouched unpinned playbook stays unlisted.
    #[test]
    fn a_re_distill_still_sees_the_playbooks_its_last_pass_settled() {
        let mut conn = temp_db();
        let fixer = session(&conn, "fix", P, day(9, 1));
        const RETIRED: &str = "error[E0308]: mismatched types: seen 2 times; fixed in abc1234";
        const REPLACED: &str = "error[E0061]: wrong arguments: seen 3 times; fixed in def5678";
        const UNTOUCHED: &str = "error[E0425]: cannot find value: seen 4 times; fixed in 0a1b2c3";
        const DISPUTED: &str = "error[E0599]: no method named: seen 2 times; fixed in 1a2b3c4";
        const DISPUTING: &str = "error[E0277]: trait not satisfied: seen 5 times; fixed in 5d6e7f8";
        for (origin, text) in [
            ("a", RETIRED),
            ("b", REPLACED),
            ("c", UNTOUCHED),
            ("d", DISPUTED),
            ("e", DISPUTING),
        ] {
            let origin = format!("playbook:{origin}@{P}");
            crate::knowledge::upsert_playbook(&conn, &origin, fixer, text, day(9, 2)).unwrap();
        }
        let (retired, replaced) = (id_of(&conn, RETIRED), id_of(&conn, REPLACED));
        let (disputed, disputing) = (id_of(&conn, DISPUTED), id_of(&conn, DISPUTING));
        let tid = session(&conn, "T", P, day(10, 1));
        const NEWER: &str = "the config is passed by reference now";
        let newer = fact(&conn, tid, "gotcha", NEWER);
        flag_fact(
            &conn,
            retired,
            FlagKind::Stale,
            SignalSource::Distill,
            None,
            Some(tid),
            day(12, 1),
        )
        .unwrap();
        for (from, to, kind) in [
            (newer, replaced, LinkKind::Supersedes),
            (newer, disputed, LinkKind::Contradicts),
            (disputing, newer, LinkKind::Contradicts),
        ] {
            link_facts(
                &conn,
                from,
                to,
                kind,
                LinkSource::Distill,
                Some(tid),
                day(12, 1),
            )
            .unwrap();
        }
        let settled = |conn: &Connection| {
            assert_eq!(status(conn, retired), FactStatus::Flagged);
            assert_eq!(status(conn, replaced), FactStatus::Superseded);
            assert_eq!(status(conn, disputed), FactStatus::Disputed);
            assert_eq!(status(conn, disputing), FactStatus::Disputed);
        };
        settled(&conn);

        let (block, _) = distill_memory_context(&conn, tid).unwrap();
        let refs = refs_by_text(&block);
        for text in [RETIRED, REPLACED, DISPUTED, DISPUTING] {
            assert!(refs.contains_key(text), "{text} in {block}");
        }
        assert!(!block.contains(UNTOUCHED), "{block}");
        distill(
            &mut conn,
            tid,
            |refs| Distilled {
                gotchas: vec![NEWER.into()],
                links: vec![
                    link("g1", &refs[REPLACED], "supersedes"),
                    link("g1", &refs[DISPUTED], "contradicts"),
                    link(&refs[DISPUTING], "g1", "contradicts"),
                ],
                retires: vec![refs[RETIRED].clone()],
                ..Default::default()
            },
            day(12, 2),
        );
        settled(&conn);
    }

    #[test]
    fn the_distiller_sees_current_memory_of_its_project_only() {
        let mut conn = temp_db();
        let tid = session(&conn, "now", P, day(10, 10));
        let (block, refs) = distill_memory_context(&conn, tid).unwrap();
        assert!(block.is_empty() && refs.is_empty(), "no memory yet");
        assert_eq!(refs.session_date().as_deref(), Some("2026-10-10"));

        let old = session(&conn, "old", P, day(9, 1));
        let gone = fact(&conn, old, "decision", "replaced decision");
        let keep = fact(&conn, old, "decision", "kept decision");
        let long = format!(
            "token sk-ant-api03-AbCdEfGhIjKlMnOpQrStUv0123456789 then {}",
            "word ".repeat(80)
        );
        fact(&conn, old, "gotcha", &long);
        let flagged = fact(&conn, old, "gotcha", "flagged gotcha");
        flag_fact(
            &conn,
            flagged,
            FlagKind::Wrong,
            SignalSource::User,
            None,
            None,
            1,
        )
        .unwrap();
        link_facts(
            &conn,
            keep,
            gone,
            LinkKind::Supersedes,
            LinkSource::User,
            None,
            1,
        )
        .unwrap();
        let pinned = fact(&conn, old, "decision", "pinned decision");
        set_fact_pinned(&conn, pinned, true).unwrap();
        fact(&conn, tid, "decision", "this session's own decision");
        fact(
            &conn,
            session(&conn, "x", "/proj/other", day(9, 1)),
            "decision",
            "another project",
        );

        let (block, refs) = distill_memory_context(&conn, tid).unwrap();
        let lines: Vec<&str> = block.lines().collect();
        assert_eq!(lines[0], "D1 (2026-09-01): pinned decision", "{block}");
        assert_eq!(lines[1], "D2 (2026-09-01): kept decision");
        assert_eq!(lines.len(), 3, "{block}");
        assert!(lines[2].starts_with("G1 (2026-09-01): token "));
        assert!(!block.contains("sk-ant-api03"), "redacted: {block}");
        assert!(lines[2].ends_with("..."));
        assert!(lines[2].len() <= "G1 (2026-09-01): ".len() + DISTILL_CONTEXT_TEXT_CHARS);
        let d1 = refs.get("D1").unwrap();
        assert_eq!(
            (d1.id, d1.pinned, d1.kind.as_str()),
            (pinned, true, "decision")
        );
        assert!(refs.get("d1").is_none(), "refs are case-sensitive");
        assert_eq!(refs.len(), 3);
        // A thread of no project sees nothing.
        conn.execute(
            "UPDATE threads SET project_path = NULL, project_key = NULL WHERE id = ?1",
            [tid],
        )
        .unwrap();
        assert!(distill_memory_context(&conn, tid).unwrap().0.is_empty());
        let _ = &mut conn;
    }

    #[test]
    fn an_index_without_the_truth_tables_reads_every_fact_as_current() {
        let conn = temp_db();
        let t = session(&conn, "s", P, day(9, 1));
        let a = fact(&conn, t, "decision", "a");
        let b = fact(&conn, t, "decision", "b");
        link_facts(&conn, b, a, LinkKind::Supersedes, LinkSource::User, None, 1).unwrap();
        conn.execute_batch("DROP TABLE fact_links; DROP TABLE fact_signals;")
            .unwrap();
        assert!(!has_truth_tables(&conn));
        assert_eq!(status(&conn, a), FactStatus::Current);
        let mem = get_project_memory_facts(&conn, P, 10).unwrap();
        assert_eq!(mem.decisions.len(), 2);
        let (_, refs) = distill_memory_context(&conn, session(&conn, "n", P, day(10, 1))).unwrap();
        assert_eq!(refs.len(), 2);
    }

    /// A real session of `project` that started at `started` and was last active at `ended`.
    fn session_span(conn: &Connection, ext: &str, project: &str, started: i64, ended: i64) -> i64 {
        let tid = session(conn, ext, project, started);
        conn.execute(
            "UPDATE threads SET updated_at = ?2 WHERE id = ?1",
            params![tid, ended],
        )
        .unwrap();
        tid
    }

    fn live_ids(conn: &Connection) -> Vec<i64> {
        let mem = get_project_memory_facts(conn, P, 40).unwrap();
        mem.decisions
            .iter()
            .chain(&mem.gotchas)
            .map(|f| f.id)
            .collect()
    }

    /// The user (or an agent, a review, a repo import) linked a distilled fact of another
    /// session to an older one: re-distilling that session with the fact reworded keeps the
    /// fact the link names, so the older decision stays replaced and the link resolves. Same
    /// for a fact carrying an agent's flag.
    #[test]
    fn a_re_distill_keeps_the_facts_other_sources_linked_or_flagged() {
        let mut conn = temp_db();
        let ta = session(&conn, "A", P, day(10, 1));
        let tb = session(&conn, "B", P, day(10, 3));
        distill(
            &mut conn,
            ta,
            |_| Distilled {
                decisions: vec![BINCODE.into()],
                ..Default::default()
            },
            day(11, 1),
        );
        let b_out = |decision: &str, gotcha: &str| Distilled {
            decisions: vec![decision.into()],
            gotchas: vec![gotcha.into()],
            ..Default::default()
        };
        const G: &str = "the cache dir must be created before the first write";
        distill(&mut conn, tb, |_| b_out(JSON, G), day(11, 2));
        let (bin, json, g) = (id_of(&conn, BINCODE), id_of(&conn, JSON), id_of(&conn, G));
        assert!(link_facts(
            &conn,
            json,
            bin,
            LinkKind::Supersedes,
            LinkSource::User,
            None,
            day(11, 3)
        )
        .unwrap()
        .link_id()
        .is_some());
        assert!(matches!(
            flag_fact(
                &conn,
                g,
                FlagKind::Stale,
                SignalSource::Agent,
                None,
                None,
                day(11, 3)
            )
            .unwrap(),
            FlagOutcome::Flagged { .. }
        ));

        // B is distilled again and the model words both items differently.
        distill(
            &mut conn,
            tb,
            |_| {
                b_out(
                    "cache as JSON (bincode broke)",
                    "create the cache dir first",
                )
            },
            day(11, 4),
        );
        assert_eq!(status(&conn, bin), FactStatus::Superseded);
        assert_eq!(id_of(&conn, JSON), json, "the linked fact is kept");
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM fact_links lk JOIN facts f ON f.uid = lk.from_uid"
            ),
            1,
            "the link still resolves"
        );
        assert_eq!(id_of(&conn, G), g);
        assert_eq!(status(&conn, g), FactStatus::Flagged);
        assert!(!live_ids(&conn).contains(&bin));
        // Repeating the kept text maps to the kept fact, never a duplicate.
        distill(&mut conn, tb, |_| b_out(JSON, G), day(11, 5));
        assert_eq!(
            count(
                &conn,
                &format!("SELECT COUNT(*) FROM facts WHERE text = '{JSON}'")
            ),
            1
        );
    }

    /// The user's Undo of a distilled link survives a re-distill that rewords the session's
    /// own item: the guard is the declaring session and the listed fact, not the exact pair.
    #[test]
    fn an_undone_distilled_link_survives_a_reworded_re_distill() {
        let mut conn = temp_db();
        let ta = session(&conn, "A", P, day(10, 1));
        let tb = session(&conn, "B", P, day(10, 3));
        distill(
            &mut conn,
            ta,
            |_| Distilled {
                decisions: vec![BINCODE.into()],
                ..Default::default()
            },
            day(11, 1),
        );
        let b_out = |text: &str| {
            let text = text.to_string();
            move |refs: &HashMap<String, String>| Distilled {
                decisions: vec![text.clone()],
                links: vec![link("d1", &refs[BINCODE], "supersedes")],
                ..Default::default()
            }
        };
        distill(&mut conn, tb, b_out(JSON), day(11, 2));
        let bin = id_of(&conn, BINCODE);
        assert_eq!(status(&conn, bin), FactStatus::Superseded);
        assert_eq!(
            unlink_facts(
                &conn,
                id_of(&conn, JSON),
                bin,
                Some(LinkKind::Supersedes),
                day(11, 3)
            )
            .unwrap(),
            1
        );
        distill(
            &mut conn,
            tb,
            b_out("serialize the cache as JSON"),
            day(11, 4),
        );
        assert_eq!(status(&conn, bin), FactStatus::Current);
        assert!(live_ids(&conn).contains(&bin));
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM fact_links WHERE dismissed_at IS NOT NULL"
            ),
            1
        );
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM fact_links WHERE dismissed_at IS NULL"
            ),
            0
        );
    }

    /// A session can reverse what was decided while it ran: the direction check compares the
    /// replaced fact with the session's last activity, not its start.
    #[test]
    fn a_session_supersedes_a_decision_recorded_while_it_ran() {
        let mut conn = temp_db();
        let start = day(10, 1);
        const R: &str = "store sessions in redis";
        let r = record_fact_with_origin(
            &conn,
            P,
            "decision",
            R,
            None,
            FactOrigin::Agent,
            None,
            start + 1800,
        )
        .unwrap()
        .id;
        let reverse = |refs: &HashMap<String, String>| Distilled {
            decisions: vec!["store sessions in postgres, not redis".into()],
            links: vec![link("d1", &refs[R], "supersedes")],
            ..Default::default()
        };
        // A session that ended before R was recorded cannot replace it.
        let before = session_span(&conn, "early", P, start - 7200, start - 3600);
        distill(&mut conn, before, reverse, day(11, 1));
        assert_eq!(status(&conn, r), FactStatus::Current);
        // One that began before R and went on after it can.
        let t = session_span(&conn, "T", P, start, start + 3600);
        distill(&mut conn, t, reverse, day(11, 2));
        assert_eq!(status(&conn, r), FactStatus::Superseded);
        assert!(!live_ids(&conn).contains(&r));

        // The distiller is told a session spans days.
        let long = session_span(&conn, "long", P, day(10, 2), day(10, 6));
        let input = crate::agent::DistillInput::load(&conn, long, 10_000)
            .unwrap()
            .unwrap();
        assert!(
            input.user_prompt().contains(
                "<existing_memory session_started=\"2026-10-02\" session_ended=\"2026-10-06\">"
            ),
            "{}",
            input.user_prompt()
        );
        let one_day = crate::agent::DistillInput::load(&conn, t, 10_000)
            .unwrap()
            .unwrap();
        assert!(!one_day.user_prompt().contains("session_ended"));
    }

    /// An older session distilled late (a backlog) cannot retire a decision made after it.
    #[test]
    fn a_backlog_distill_never_retires_a_newer_decision() {
        let mut conn = temp_db();
        let old = session(&conn, "old", P, day(9, 1));
        let older = fact(&conn, old, "decision", "use tabs in the config files");
        let ta = session_span(&conn, "A", P, day(10, 1), day(10, 1) + 3600);
        let tb = session(&conn, "B", P, day(10, 5));
        const D: &str = "deploy with the blue-green script";
        distill(
            &mut conn,
            tb,
            |_| Distilled {
                decisions: vec![D.into()],
                ..Default::default()
            },
            day(11, 1),
        );
        distill(
            &mut conn,
            ta,
            |refs| Distilled {
                decisions: vec!["something else".into()],
                retires: vec![
                    refs[D].clone(),
                    refs["use tabs in the config files"].clone(),
                ],
                ..Default::default()
            },
            day(11, 2),
        );
        let d = id_of(&conn, D);
        assert_eq!(status(&conn, d), FactStatus::Current);
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM fact_signals WHERE kind = 'stale'"
            ),
            1,
            "only the older item"
        );
        assert!(live_ids(&conn).contains(&d));
        assert_eq!(status(&conn, older), FactStatus::Flagged);
    }

    /// Hiding the middle of a chain does not revive the start while something later still
    /// stands over it; hiding a replacement that was still in force does.
    #[test]
    fn hiding_a_replaced_or_retired_replacement_keeps_what_it_replaced_out() {
        let conn = temp_db();
        let t = session(&conn, "s", P, day(9, 1));
        let f = |text: &str| fact(&conn, t, "decision", text);
        let user = |from, to| {
            link_facts(
                &conn,
                from,
                to,
                LinkKind::Supersedes,
                LinkSource::User,
                None,
                1,
            )
            .unwrap()
            .link_id()
            .unwrap()
        };
        let (a, b, c) = (f("a"), f("b"), f("c"));
        user(b, a);
        user(c, b);
        set_fact_hidden(&conn, b, true).unwrap();
        assert_eq!(status(&conn, a), FactStatus::Superseded);
        assert!(!live_ids(&conn).contains(&a));
        let other = session(&conn, "o", P, day(10, 1));
        let (block, _) = distill_memory_context(&conn, other).unwrap();
        assert!(!block.lines().any(|l| l.ends_with(": a")), "{block}");
        // C hidden too: nothing stands over A any more.
        set_fact_hidden(&conn, c, true).unwrap();
        assert_eq!(status(&conn, a), FactStatus::Current);

        // E replaced D, then a later session retired E as stale; hiding E keeps D out.
        let (d, e) = (f("d"), f("e"));
        user(e, d);
        flag_fact(
            &conn,
            e,
            FlagKind::Stale,
            SignalSource::Agent,
            None,
            None,
            2,
        )
        .unwrap();
        set_fact_hidden(&conn, e, true).unwrap();
        assert_eq!(status(&conn, d), FactStatus::Superseded);
        assert!(!live_ids(&conn).contains(&d));
        // A replacement flagged wrong and hidden is the user rejecting it: H comes back.
        let (h, k) = (f("h"), f("k"));
        user(k, h);
        flag_fact(
            &conn,
            k,
            FlagKind::Wrong,
            SignalSource::Agent,
            None,
            None,
            2,
        )
        .unwrap();
        set_fact_hidden(&conn, k, true).unwrap();
        assert_eq!(status(&conn, h), FactStatus::Current);
        assert!(live_ids(&conn).contains(&h));
    }

    #[test]
    fn a_superseded_counterpart_disputes_nothing() {
        let conn = temp_db();
        let t = session(&conn, "s", P, day(9, 1));
        let (x, y, z) = (
            fact(&conn, t, "decision", "x"),
            fact(&conn, t, "decision", "y"),
            fact(&conn, t, "decision", "z"),
        );
        let user = |from, to, kind| {
            link_facts(&conn, from, to, kind, LinkSource::User, None, 1)
                .unwrap()
                .link_id()
                .unwrap()
        };
        user(y, x, LinkKind::Contradicts);
        assert_eq!(status(&conn, y), FactStatus::Disputed);
        user(z, x, LinkKind::Supersedes);
        let uid = uid_of(&conn, y).unwrap().unwrap();
        let ty = &fact_truth(&conn, &[&uid]).unwrap()[&uid];
        assert_eq!(ty.status, FactStatus::Current);
        assert!(ty.disputed_by.is_empty());
        // Either side: X's dispute with Y is gone too, as X is history now.
        assert_eq!(status(&conn, x), FactStatus::Superseded);
        assert_eq!(
            unlink_facts(&conn, z, x, Some(LinkKind::Supersedes), 2).unwrap(),
            1
        );
        assert_eq!(status(&conn, y), FactStatus::Disputed);
    }

    /// A fact the user edited is curated like a pinned one: a machine flag (a distilled
    /// `retires`, an agent's flag_fact) is stored and shown, but the fact stays injected.
    #[test]
    fn machine_flags_never_hide_an_edited_fact() {
        let mut conn = temp_db();
        let old = session(&conn, "old", P, day(9, 1));
        let e = fact(&conn, old, "decision", "use the v1 api");
        const FIXED: &str = "use the v2 api for uploads";
        crate::knowledge::edit_fact(&conn, e, FIXED).unwrap();
        let t = session(&conn, "new", P, day(10, 1));
        distill(
            &mut conn,
            t,
            |refs| Distilled {
                decisions: vec!["unrelated".into()],
                retires: vec![refs[FIXED].clone()],
                ..Default::default()
            },
            day(11, 1),
        );
        flag_fact(
            &conn,
            e,
            FlagKind::Wrong,
            SignalSource::Agent,
            Some("no longer holds"),
            None,
            day(11, 2),
        )
        .unwrap();
        let mem = get_project_memory_facts(&conn, P, 40).unwrap();
        let f = mem.decisions.iter().find(|f| f.id == e).expect("injected");
        assert_eq!(f.status, FactStatus::Flagged);
        assert_eq!(f.flags.len(), 2);
        let block = crate::export::agent_memory_md_budgeted(
            P,
            &mem,
            None,
            &crate::export::MemoryBudget::AGENTS_FILE,
        );
        assert!(block.contains(FIXED), "{block}");
        // Still listed to the distiller, and the user's Keep clears both flags.
        let (block, _) = distill_memory_context(&conn, session(&conn, "n", P, day(10, 9))).unwrap();
        assert!(block.contains(FIXED), "{block}");
        assert_eq!(resolve_fact_flags(&conn, e, day(11, 3)).unwrap(), 2);
        assert_eq!(status(&conn, e), FactStatus::Current);
    }

    #[test]
    fn a_uid_asked_twice_reports_its_links_and_flags_once() {
        let conn = temp_db();
        let t = session(&conn, "s", P, day(9, 1));
        let (a, b) = (
            fact(&conn, t, "decision", "a"),
            fact(&conn, t, "decision", "b"),
        );
        link_facts(&conn, b, a, LinkKind::Supersedes, LinkSource::User, None, 1).unwrap();
        flag_fact(&conn, b, FlagKind::Stale, SignalSource::User, None, None, 1).unwrap();
        let uid = uid_of(&conn, b).unwrap().unwrap();
        let t = &fact_truth(&conn, &[&uid, &uid]).unwrap()[&uid];
        assert_eq!((t.flags.len(), t.replaces.len()), (1, 1));
    }

    #[test]
    fn cut_words_backs_off_to_a_word_boundary() {
        assert_eq!(cut_words("short   text\nhere", 50), "short text here");
        assert_eq!(cut_words("alpha beta gamma delta", 15), "alpha beta...");
        let one = "x".repeat(40);
        assert_eq!(cut_words(&one, 10).chars().count(), 10);
    }
}
