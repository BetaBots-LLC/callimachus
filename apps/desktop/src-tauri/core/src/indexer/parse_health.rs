//! Parse health per source: the canary for an agent that changed its transcript format.
//!
//! Every indexer drops malformed lines and record types it does not know (`common`'s
//! counters say how many). For one odd line that is right; when an agent ships a new
//! format it is silent data loss, and permanent once the agent's own cleanup deletes the
//! originals (89% of the author's main Claude sessions exist only in the index). So each
//! pass's counts are judged against the source's own history, kept in `app_config` under
//! `parse_health.<source>`, and a jump shows up in Settings > Index and `cal doctor`.
//!
//! The rule calibrates itself, because only a few sources can be checked by hand: some
//! sources are noisy by nature (10 of the 41 Copilot chat session files on the author's
//! machine are the newer JSONL form, which this indexer cannot read: 10 malformed records
//! against 11 good ones on a first pass, measured 2026-11-09; each is counted again only
//! when it changes), and a fixed threshold would either warn forever there or miss a real
//! change elsewhere. Measured the same day over that machine's whole history, every other
//! source read 0% unknown or malformed (Claude Code: 689,185 records in 4,370 files; Cursor:
//! 137,963 in 2,079), and judging each of the 2,141 Claude transcripts of 50 records or
//! more on its own against the full pass warned on none. Only passes of at least
//! [`MIN_LINES`] records or [`MIN_FILES`] files are judged or kept. The watcher indexes an
//! active session a few lines at a time and a new file one at a time, so its batches under
//! both floors are summed per source in the process ([`add_pending`]) and judged as one pass
//! once the sum reaches a floor; until then they cost nothing here (no write, no log line).
//! The file floor is for the JSON-document sources (Continue, Copilot, Cline, Roo, Kilo: one
//! session per file), where a renamed container leaves one unknown record per file, far
//! under 50. A pass is drift when, against the sums of the source's last [`KEEP_PASSES`]
//! kept passes:
//! - it has [`MIN_LINES`] records and its unknown-plus-malformed share is more than
//!   [`DRIFT_POINTS`] above that of the kept passes of [`MIN_LINES`] records, or
//! - at least [`ZERO_FILES_MIN`] of its files, and at least half of them, held records but
//!   yielded no message, while the kept passes' share of such files (over at least
//!   [`MIN_FILES`] files) is under half, and this pass's share is more than [`DRIFT_POINTS`]
//!   above it (a full pass over Claude Code's 4,357 transcripts here meets 278 legitimate
//!   ones, 6.4%: a count alone would warn on every reindex). An empty file (a Cursor composer
//!   without a bubble, a session with no request) is not one of them (`common::count_file`):
//!   Cursor's 807 unused composers of 2,079 are only ever all parsed by a full reindex, and
//!   would read as a format change against a history of watcher batches over active ones.
//!
//! A source's first judged pass only seeds its record: with nothing to compare to, a
//! warning would only reflect how noisy the source is. A drift stays until a full pass
//! reads clean against it ([`Drift::cleared_by`]) or the user dismisses it, and a dismissal
//! quiets the same change for [`DISMISS_SECS`]. Counts and type names only; never text from
//! the files. No telemetry.

use super::common::ParseStats;
use crate::knowledge::app_config;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

/// `app_config` key prefix; the source kind follows.
pub const KEY_PREFIX: &str = "parse_health.";
/// Fewest records a pass needs to be judged or kept (or [`MIN_FILES`] files).
pub const MIN_LINES: u64 = 50;
/// Fewest files a pass of under [`MIN_LINES`] records needs to be judged or kept, and the
/// fewest the kept passes need before the files rule compares against them.
pub const MIN_FILES: u64 = 5;
/// Judged passes kept per source.
pub const KEEP_PASSES: usize = 8;
/// How far above its own history a pass's share must be to count as drift.
pub const DRIFT_POINTS: f64 = 0.20;
/// Fewest zero-message files in one pass for the second rule, which also needs them to be
/// at least half of the pass's files. At 3 of any number, a catch-up over 8 of Claude Code's
/// transcripts (6.4% of them yield no message legitimately) tripped it about once in a
/// hundred passes by chance; a real change of the whole shape turns nearly every file of a
/// pass into one.
pub const ZERO_FILES_MIN: u64 = MIN_FILES;
/// Unknown type names a drift keeps, most frequent first.
const DRIFT_KINDS: usize = 8;
/// How long a dismissed warning keeps the same change quiet, counted from when the
/// dismissed one was found. Past it the warning comes back: a dismissal is not a promise
/// to stay silent about data loss forever.
pub const DISMISS_SECS: i64 = 30 * 86_400;
/// How close to the baseline it was raised against a full pass's share must come back for
/// a share drift to end: half the margin that raised it, so a share hovering at the line
/// does not end a warning whose cause is still there.
const CLEAR_POINTS: f64 = DRIFT_POINTS / 2.0;

/// One judged pass, summed.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PassSums {
    pub lines: u64,
    /// Unknown plus malformed records.
    pub bad: u64,
    pub files: u64,
    /// Files that held records but yielded no message.
    pub zero: u64,
}

impl PassSums {
    fn of(stats: &ParseStats) -> Self {
        Self {
            lines: stats.lines,
            bad: stats.malformed_lines + stats.unknown(),
            files: stats.files,
            zero: stats.files_with_zero_messages,
        }
    }

    /// Big enough to judge and keep: [`MIN_LINES`] records or [`MIN_FILES`] files.
    fn big_enough(&self) -> bool {
        self.lines >= MIN_LINES || self.files >= MIN_FILES
    }

    /// The sums of `passes`, None when there is none.
    fn sum<'a>(passes: impl Iterator<Item = &'a PassSums>) -> Option<PassSums> {
        passes.fold(None, |acc, p| {
            let mut s = acc.unwrap_or_default();
            s.add(p);
            Some(s)
        })
    }

    fn add(&mut self, other: &PassSums) {
        self.lines += other.lines;
        self.bad += other.bad;
        self.files += other.files;
        self.zero += other.zero;
    }

    /// Unknown plus malformed, as a share of the records (at most 1: a record can carry
    /// several unknown parts).
    pub fn bad_share(&self) -> f64 {
        share(self.bad, self.lines)
    }

    /// Files that yielded no message, as a share of the files.
    pub fn zero_share(&self) -> f64 {
        share(self.zero, self.files)
    }
}

fn share(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        (part as f64 / whole as f64).min(1.0)
    }
}

/// The latest pass of a source worth keeping: a full pass, or one of at least 50 records or
/// 5 files.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LastPass {
    /// Unix seconds.
    pub at: i64,
    /// A pass over every file of the source (a reindex, `cal index`, the launch catch-up),
    /// not a watcher batch.
    pub full: bool,
    pub stats: ParseStats,
}

/// A pass that read like a format change.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Drift {
    /// When the pass ran (unix seconds).
    pub at: i64,
    /// Its unknown-plus-malformed share of records (0 to 1).
    pub share: f64,
    /// The same share over the source's passes before it (of at least 50 records; 0 when
    /// there is none yet).
    pub baseline: f64,
    pub malformed: u64,
    /// Its unknown type names, most frequent first.
    pub kinds: Vec<String>,
    /// Files it parsed, and how many of them yielded no message.
    pub files: u64,
    pub zero_files: u64,
    /// Which rule fired: the share of unknown and malformed records, the files without
    /// messages, or both.
    pub by_share: bool,
    pub by_zero_files: bool,
    /// The user dismissed the warning. The same change found again within 30 days of this
    /// one stays dismissed; a different one (new type names, another rule), or the same one
    /// found later, raises it again.
    #[serde(default)]
    pub dismissed: bool,
}

impl Drift {
    fn same_change(&self, other: &Drift) -> bool {
        let mut a = self.kinds.clone();
        let mut b = other.kinds.clone();
        a.sort();
        b.sort();
        a == b && self.by_share == other.by_share && self.by_zero_files == other.by_zero_files
    }

    /// Whether a full pass reads clean against the line this drift was raised over: back
    /// near its baseline for the share rule, under half of its files without a message for
    /// the files rule. "No drift against the history" alone is not that: the drifting passes
    /// are kept, so within a few passes a change that is still there reads as the source's
    /// usual (against a 2% baseline, a type at 45% of records stops warning once five of the
    /// eight kept passes carry it).
    fn cleared_by(&self, pass: &PassSums) -> bool {
        let share_ok = !self.by_share
            || (pass.lines >= MIN_LINES && pass.bad_share() <= self.baseline + CLEAR_POINTS);
        let files_ok =
            !self.by_zero_files || (pass.files >= MIN_FILES && pass.zero * 2 < pass.files);
        share_ok && files_ok
    }
}

/// What `parse_health.<source>` holds.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    /// The last 8 judged passes ([`KEEP_PASSES`]), oldest first.
    #[serde(default)]
    pub passes: Vec<PassSums>,
    #[serde(default)]
    pub last: Option<LastPass>,
    #[serde(default)]
    pub drift: Option<Drift>,
}

impl Health {
    /// The sums of the kept passes.
    pub fn rolling(&self) -> PassSums {
        let mut sum = PassSums::default();
        for p in &self.passes {
            sum.add(p);
        }
        sum
    }

    /// Fold one pass in: judge it against the history, then keep it when it is big enough.
    /// Returns the drift it found, if any (also stored, unless the user dismissed the same
    /// change less than [`DISMISS_SECS`] before). A full pass that is judged, finds none and
    /// reads clean against the stored drift ends it: the user updated, and a warning about a
    /// cause that is gone would stay up until dismissed.
    fn record(&mut self, stats: &ParseStats, full: bool, now: i64) -> Option<Drift> {
        let pass = PassSums::of(stats);
        let found = judge(&self.passes, &pass, stats, now);
        match &found {
            Some(drift) => {
                let quiet = self.drift.as_ref().is_some_and(|d| {
                    d.dismissed && d.same_change(drift) && drift.at - d.at < DISMISS_SECS
                });
                // A quiet one leaves the dismissed record as it is, so its age keeps counting
                // from when it was found.
                if !quiet {
                    self.drift = Some(drift.clone());
                }
            }
            None if full
                && pass.big_enough()
                && self.drift.as_ref().is_some_and(|d| d.cleared_by(&pass)) =>
            {
                self.drift = None;
            }
            None => {}
        }
        if pass.big_enough() {
            self.passes.push(pass);
            let excess = self.passes.len().saturating_sub(KEEP_PASSES);
            self.passes.drain(..excess);
        }
        self.last = Some(LastPass {
            at: now,
            full,
            stats: stats.clone(),
        });
        found
    }
}

/// The drift in `pass` against the kept `passes`, if any. Each rule judges a pass big enough
/// for it against the kept passes big enough for it; with none there is nothing to compare
/// to, so a source's first pass only seeds its record.
fn judge(passes: &[PassSums], pass: &PassSums, stats: &ParseStats, now: i64) -> Option<Drift> {
    if !pass.big_enough() {
        return None;
    }
    // A share of a few records is noise: this rule is over passes of MIN_LINES records only.
    let by_lines = PassSums::sum(passes.iter().filter(|p| p.lines >= MIN_LINES));
    let by_share = pass.lines >= MIN_LINES
        && by_lines
            .as_ref()
            .is_some_and(|b| pass.bad_share() > b.bad_share() + DRIFT_POINTS);
    let all = PassSums::sum(passes.iter()).filter(|b| b.files >= MIN_FILES);
    let by_zero_files = pass.zero >= ZERO_FILES_MIN
        && pass.zero * 2 >= pass.files
        && all.as_ref().is_some_and(|b| {
            b.zero_share() < 0.5 && pass.zero_share() > b.zero_share() + DRIFT_POINTS
        });
    if !by_share && !by_zero_files {
        return None;
    }
    let mut kinds: Vec<(&String, &u64)> = stats.unknown_kinds.iter().collect();
    kinds.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    Some(Drift {
        at: now,
        share: pass.bad_share(),
        baseline: by_lines.as_ref().map_or(0.0, PassSums::bad_share),
        malformed: stats.malformed_lines,
        kinds: kinds
            .into_iter()
            .take(DRIFT_KINDS)
            .map(|(k, _)| k.clone())
            .collect(),
        files: pass.files,
        zero_files: pass.zero,
        by_share,
        by_zero_files,
        dismissed: false,
    })
}

fn key(source: &str) -> String {
    format!("{KEY_PREFIX}{source}")
}

/// The stored health of `source` (empty when unset or unreadable).
pub fn load(conn: &Connection, source: &str) -> Health {
    app_config::get(conn, &key(source))
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default()
}

/// Every source with a stored record, by kind. Read-only, so `cal doctor` and the app's
/// read pool can call it; `app_config` is in every schema.
pub fn all(conn: &Connection) -> Vec<(String, Health)> {
    let Ok(mut stmt) =
        conn.prepare("SELECT key, value FROM app_config WHERE key >= ?1 AND key < ?2 ORDER BY key")
    else {
        return Vec::new();
    };
    // A key range rather than LIKE, whose `_` would match any character.
    let hi = format!("{}\u{10FFFF}", KEY_PREFIX);
    let rows = stmt.query_map([KEY_PREFIX, hi.as_str()], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    });
    let Ok(rows) = rows else {
        return Vec::new();
    };
    rows.filter_map(Result::ok)
        .filter_map(|(k, v)| {
            let source = k.strip_prefix(KEY_PREFIX)?.to_string();
            Some((source, serde_json::from_str(&v).ok()?))
        })
        .collect()
}

/// One source's open warning, for Settings > Index.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SourceDrift {
    pub source: String,
    #[serde(flatten)]
    pub drift: Drift,
}

/// The drifts nobody dismissed yet.
pub fn open_drifts(conn: &Connection) -> Vec<SourceDrift> {
    all(conn)
        .into_iter()
        .filter_map(|(source, h)| {
            let drift = h.drift.filter(|d| !d.dismissed)?;
            Some(SourceDrift { source, drift })
        })
        .collect()
}

/// Dismiss `source`'s warning (a no-op when it has none).
pub fn dismiss(conn: &Connection, source: &str) -> anyhow::Result<()> {
    let mut health = load(conn, source);
    if let Some(d) = health.drift.as_mut() {
        if !d.dismissed {
            d.dismissed = true;
            app_config::put(conn, &key(source), &serde_json::to_string(&health)?)?;
        }
    }
    Ok(())
}

/// Watcher batches too small to judge alone, summed per (index file, source) in this
/// process. While the app runs, files reach the watcher one or two at a time: judged one
/// batch at a time, none ever reached a floor, and the canary only saw launch catch-ups and
/// manual reindexes (by then the batches' files were stamped and never parsed again). A
/// static rather than an `app_config` row: no write per batch. The sums are lost when the
/// process exits, which only leaves those files unjudged.
static PENDING: Mutex<BTreeMap<(String, String), ParseStats>> = Mutex::new(BTreeMap::new());

/// Add a watcher batch too small to judge to `source`'s pending sum on `conn`'s index, and
/// return what is due for judging: the whole sum once it holds [`MIN_FILES`] files, or its
/// records alone once they reach [`MIN_LINES`]. In that second case the file counts carry
/// over to the next sum: Claude Code's watcher meets a new transcript (one file) among
/// hundreds of appended lines, so a sum cut at its records would never hold enough files
/// for the files rule.
fn add_pending(conn: &Connection, source: &str, stats: &ParseStats) -> Option<ParseStats> {
    let key = (
        conn.path().unwrap_or_default().to_string(),
        source.to_string(),
    );
    let mut pending = PENDING.lock().unwrap_or_else(PoisonError::into_inner);
    let sum = pending.entry(key).or_default();
    sum.merge(stats);
    if sum.files >= MIN_FILES {
        return Some(std::mem::take(sum));
    }
    if sum.lines < MIN_LINES {
        return None;
    }
    let files = ParseStats {
        files: sum.files,
        files_with_zero_messages: sum.files_with_zero_messages,
        ..ParseStats::default()
    };
    let mut due = std::mem::replace(sum, files);
    due.files = 0;
    due.files_with_zero_messages = 0;
    Some(due)
}

/// After a pass over `source` that collected `stats` (`common::collect_parse_stats`): log one
/// line, fold the pass into the source's record and log a drift. Only for a pass that parsed
/// something. A watcher batch under [`MIN_LINES`] records and [`MIN_FILES`] files is added to
/// the source's pending sum instead ([`add_pending`]) and costs no write and no log line
/// until that sum is judged as one pass. Best effort: a failed write keeps the old record.
pub fn after_pass(conn: &Connection, source: &str, stats: &ParseStats, full: bool) {
    after_pass_at(conn, source, stats, full, chrono::Utc::now().timestamp());
}

fn after_pass_at(conn: &Connection, source: &str, stats: &ParseStats, full: bool, now: i64) {
    if stats.is_empty() {
        return;
    }
    if !full && !PassSums::of(stats).big_enough() {
        if let Some(due) = add_pending(conn, source, stats) {
            record_pass(conn, source, &due, false, now);
        }
        return;
    }
    record_pass(conn, source, stats, full, now);
}

/// Log, judge and store one pass of `source`.
fn record_pass(conn: &Connection, source: &str, stats: &ParseStats, full: bool, now: i64) {
    app_log!("[parse] {source}: {}", summary_line(stats));
    let mut health = load(conn, source);
    let found = health.record(stats, full, now);
    if let Some(d) = &found {
        app_log!(
            "[parse] {source}: possible transcript format change: {:.0}% of records unknown or \
             malformed (usually {:.0}%), {} of {} files without messages{}",
            d.share * 100.0,
            d.baseline * 100.0,
            d.zero_files,
            d.files,
            if d.kinds.is_empty() {
                String::new()
            } else {
                format!("; unknown types: {}", d.kinds.join(", "))
            }
        );
    }
    if let Ok(json) = serde_json::to_string(&health) {
        let _ = app_config::put(conn, &key(source), &json);
    }
}

/// `N records, M malformed, K unknown (a 3, b 1), F files (Z without messages)`.
pub fn summary_line(stats: &ParseStats) -> String {
    let mut kinds: Vec<(&String, &u64)> = stats.unknown_kinds.iter().collect();
    kinds.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let named: Vec<String> = kinds
        .iter()
        .take(DRIFT_KINDS)
        .map(|(k, n)| format!("{k} {n}"))
        .collect();
    format!(
        "{} records, {} malformed, {} unknown{}, {} files ({} without messages)",
        stats.lines,
        stats.malformed_lines,
        stats.unknown(),
        if named.is_empty() {
            String::new()
        } else {
            format!(" ({})", named.join(", "))
        },
        stats.files,
        stats.files_with_zero_messages
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn pass(lines: u64, unknown: u64, files: u64, zero: u64) -> ParseStats {
        ParseStats {
            lines,
            malformed_lines: 0,
            unknown_kinds: if unknown > 0 {
                BTreeMap::from([("brand_new".to_string(), unknown)])
            } else {
                BTreeMap::new()
            },
            files,
            files_with_zero_messages: zero,
        }
    }

    /// A source that is always 40% unknown (stable noise) never warns, from its first pass
    /// on.
    #[test]
    fn a_stable_noisy_source_never_warns() {
        let mut h = Health::default();
        for i in 0..20 {
            assert_eq!(
                h.record(&pass(1000, 400 + i, 100, 30), true, i as i64),
                None
            );
        }
        assert_eq!(h.passes.len(), KEEP_PASSES, "only the last passes are kept");
        assert!(h.drift.is_none());
    }

    /// A jump in unknown records against the source's own history warns and names the type.
    #[test]
    fn a_jump_in_unknown_records_warns() {
        let mut h = Health::default();
        for i in 0..5 {
            assert!(h.record(&pass(1000, 20, 100, 5), true, i).is_none());
        }
        let d = h.record(&pass(200, 90, 10, 0), false, 9).expect("drift");
        assert!(d.by_share && !d.by_zero_files);
        assert_eq!(d.kinds, vec!["brand_new".to_string()]);
        assert!((d.share - 0.45).abs() < 1e-9 && (d.baseline - 0.02).abs() < 1e-9);
        assert_eq!(h.drift.as_ref().map(|d| d.at), Some(9));
    }

    /// Files without messages: a full pass with its usual share is fine however many there
    /// are; a pass where most files yield nothing warns.
    #[test]
    fn files_without_messages_warn_only_against_the_usual_share() {
        let mut h = Health::default();
        h.record(&pass(684_000, 0, 4357, 278), true, 1);
        assert!(h.record(&pass(684_000, 0, 4357, 278), true, 2).is_none());
        let d = h.record(&pass(300, 0, 6, 5), false, 3).expect("drift");
        assert!(d.by_zero_files && !d.by_share);
        // Two files without messages are not enough to say anything.
        assert!(h.record(&pass(300, 0, 2, 2), false, 4).is_none());
        // A source where most files yield nothing anyway never warns on them.
        let mut empty = Health::default();
        for i in 0..4 {
            empty.record(&pass(500, 0, 10, 6), true, i);
        }
        assert!(empty.record(&pass(500, 0, 10, 10), true, 5).is_none());
    }

    /// A JSON-document source (one session per file) whose container an update renamed: each
    /// file is one unknown record and yields nothing, far under MIN_LINES records, and is
    /// still judged, on its files.
    #[test]
    fn a_renamed_container_in_a_small_document_source_warns() {
        let mut h = Health::default();
        for i in 0..3 {
            assert!(h.record(&pass(40, 0, 8, 0), true, i).is_none());
        }
        assert_eq!(
            h.passes.len(),
            3,
            "kept on their files though under MIN_LINES"
        );
        let d = h.record(&pass(6, 6, 6, 6), true, 9).expect("drift");
        assert!(
            d.by_zero_files && !d.by_share,
            "6 records: too few for the share rule"
        );
        assert_eq!(d.kinds, vec!["brand_new".to_string()]);
        assert_eq!(
            d.baseline, 0.0,
            "no pass of MIN_LINES records to compare to"
        );
        // A smaller batch of the same stays under both floors.
        let mut small = h.clone();
        small.drift = None;
        assert!(small.record(&pass(4, 4, 4, 4), false, 10).is_none());
    }

    /// A few files without messages in a small pass are chance, not a format change: Claude
    /// Code's real share is 6.4%, and sessions opened and closed without a prompt cluster.
    /// The rule needs ZERO_FILES_MIN of them and at least half of the pass's files.
    #[test]
    fn the_files_rule_needs_five_files_and_half_the_pass() {
        let mut h = Health::default();
        h.record(&pass(684_000, 0, 4357, 278), true, 1);
        h.record(&pass(684_000, 0, 4357, 278), true, 2);
        assert!(h.record(&pass(300, 0, 8, 3), false, 3).is_none(), "3 of 8");
        assert!(h.record(&pass(300, 0, 4, 4), false, 4).is_none(), "4 of 4");
        assert!(
            h.record(&pass(300, 0, 12, 5), false, 5).is_none(),
            "5 of 12"
        );
        let d = h.record(&pass(300, 0, 6, 5), false, 6).expect("5 of 6");
        assert!(d.by_zero_files && !d.by_share);
    }

    /// The canary sees what the running app's watcher sees: batches of one file each, too
    /// small to judge alone, add up per index and source until they reach a floor.
    #[test]
    fn small_watcher_batches_add_up_to_a_judged_pass() {
        let t = crate::db::test_support::TestIndex::new("parse_health_pending");
        let conn = &*t;
        // A document source's history: whole sessions that each yield messages.
        for at in 0..3 {
            after_pass_at(conn, "cline", &pass(40, 0, 8, 0), true, at);
        }
        assert!(load(conn, "cline").drift.is_none());
        // An update renames the container: each turn rewrites one task file, one unknown
        // record that yields nothing, one watcher batch at a time.
        for at in 10..16 {
            after_pass_at(conn, "cline", &pass(1, 1, 1, 1), false, at);
        }
        let h = load(conn, "cline");
        let d = h
            .drift
            .expect("six one-file batches raise the files warning");
        assert!(d.by_zero_files && d.kinds == vec!["brand_new".to_string()]);
        assert_eq!(
            (d.files, d.zero_files),
            (5, 5),
            "judged once the sum had 5 files"
        );
        assert!(!h.last.unwrap().full);

        // Another index's batches never add into this one's sum.
        let other = crate::db::test_support::TestIndex::new("parse_health_pending_other");
        for at in 0..4 {
            after_pass_at(&other, "cline", &pass(1, 1, 1, 1), false, at);
        }
        assert!(all(&other).is_empty());
    }

    /// Clean batches under both floors write nothing (no app_config row, no log line) until
    /// their sum reaches one; Claude Code's new transcripts, one file among many appended
    /// lines, still add up to a judgement of files.
    #[test]
    fn small_clean_batches_write_nothing_until_their_sum_is_judged() {
        let t = crate::db::test_support::TestIndex::new("parse_health_quiet");
        let conn = &*t;
        for at in 0..4 {
            after_pass_at(conn, "codex", &pass(10, 0, 1, 0), false, at);
        }
        assert!(all(conn).is_empty(), "40 records, 4 files: nothing written");
        after_pass_at(conn, "codex", &pass(10, 0, 1, 0), false, 5);
        let h = load(conn, "codex");
        assert_eq!(
            h.passes,
            vec![PassSums {
                lines: 50,
                bad: 0,
                files: 5,
                zero: 0
            }]
        );

        // Records reach their floor first: they are judged, and the files wait for theirs.
        after_pass_at(conn, "claude_code", &pass(30, 0, 1, 0), false, 6);
        after_pass_at(conn, "claude_code", &pass(30, 0, 1, 1), false, 7);
        let h = load(conn, "claude_code");
        assert_eq!(
            h.passes,
            vec![PassSums {
                lines: 60,
                bad: 0,
                files: 0,
                zero: 0
            }]
        );
        for at in 8..11 {
            after_pass_at(conn, "claude_code", &pass(1, 0, 1, 1), false, at);
        }
        let h = load(conn, "claude_code");
        assert_eq!(h.passes.len(), 2);
        assert_eq!(
            h.passes[1],
            PassSums {
                lines: 3,
                bad: 0,
                files: 5,
                zero: 4
            }
        );
    }

    /// A drift ends when a full pass reads clean again (the user updated), but not when the
    /// history has merely absorbed the change: the drifting passes are kept, so a pass that
    /// still carries it soon reads as "no drift" against them.
    #[test]
    fn a_full_clean_pass_clears_the_drift_and_an_absorbed_change_does_not() {
        let mut h = Health::default();
        for i in 0..5 {
            h.record(&pass(1000, 20, 100, 5), true, i);
        }
        assert!(h.record(&pass(1000, 450, 100, 5), false, 9).is_some());
        // The change keeps showing up until the history is used to it.
        let mut at = 10;
        while h.record(&pass(1000, 450, 100, 5), true, at).is_some() {
            at += 1;
        }
        assert!(h.drift.is_some(), "absorbed, not fixed: the warning stays");
        // A watcher batch that reads clean is not a verdict on the source.
        h.record(&pass(1000, 20, 100, 5), false, at + 1);
        assert!(h.drift.is_some());
        // A reindex after the update that taught Callimachus the type.
        assert!(h.record(&pass(1000, 20, 100, 5), true, at + 2).is_none());
        assert!(h.drift.is_none(), "the source reads clean again");
    }

    /// A dismissal keeps the same change quiet for 30 days after the dismissed one, then the
    /// warning comes back.
    #[test]
    fn a_dismissal_lasts_thirty_days() {
        let day = 86_400;
        let mut h = Health::default();
        h.record(&pass(1000, 0, 10, 0), true, 0);
        assert!(h.record(&pass(1000, 500, 10, 0), false, day).is_some());
        h.drift.as_mut().unwrap().dismissed = true;
        h.record(&pass(1000, 600, 10, 0), false, 29 * day);
        let d = h.drift.as_ref().unwrap();
        assert!(
            d.dismissed && d.at == day,
            "29 days on: still the dismissed one"
        );
        h.record(&pass(1000, 600, 10, 0), false, 32 * day);
        let d = h.drift.as_ref().unwrap();
        assert!(!d.dismissed && d.at == 32 * day, "31 days on: raised again");
    }

    /// The files rule needs a history of files: passes that only appended lines to known
    /// transcripts (Claude Code's watcher) say nothing about how often a new file yields no
    /// message, so a catch-up whose new files are a few without messages does not warn.
    #[test]
    fn the_files_rule_waits_for_a_history_of_files() {
        let mut h = Health::default();
        for i in 0..8 {
            assert!(h.record(&pass(200, 0, 0, 0), false, i).is_none());
        }
        assert!(h.record(&pass(200, 0, 3, 3), true, 9).is_none());
        assert!(
            h.record(&pass(200, 0, 3, 3), true, 10).is_none(),
            "3 files kept: still under MIN_FILES"
        );
    }

    /// The first judged pass only seeds the record; small passes are neither judged nor kept.
    #[test]
    fn the_first_pass_and_small_passes_never_warn() {
        let mut h = Health::default();
        assert!(h.record(&pass(49, 49, 3, 3), false, 1).is_none());
        assert!(
            h.passes.is_empty(),
            "under MIN_LINES and MIN_FILES: not kept"
        );
        assert!(
            h.record(&pass(1000, 900, 50, 40), true, 2).is_none(),
            "first"
        );
        assert_eq!(h.passes.len(), 1);
        assert!(h.record(&pass(49, 49, 3, 3), false, 3).is_none());
        assert_eq!(h.passes.len(), 1);
    }

    /// Dismissed stays dismissed while the same change keeps showing up, and a different
    /// change raises the warning again.
    #[test]
    fn a_dismissed_change_stays_dismissed_until_another_one() {
        let mut h = Health::default();
        h.record(&pass(1000, 0, 10, 0), true, 1);
        assert!(h.record(&pass(1000, 500, 10, 0), true, 2).is_some());
        h.drift.as_mut().unwrap().dismissed = true;
        h.record(&pass(1000, 600, 10, 0), true, 3);
        assert!(h.drift.as_ref().unwrap().dismissed, "the same type again");
        let mut other = pass(1000, 0, 10, 0);
        other.unknown_kinds.insert("another_new".into(), 900);
        h.record(&other, true, 4);
        let d = h.drift.as_ref().unwrap();
        assert!(!d.dismissed && d.at == 4 && d.kinds == vec!["another_new".to_string()]);
    }

    /// Stored under `parse_health.<source>`, read back by `all`, `open_drifts` and
    /// `dismiss`; watcher batches under MIN_LINES write nothing.
    #[test]
    fn health_round_trips_through_app_config() {
        let t = crate::db::test_support::TestIndex::new("parse_health");
        let conn = &*t;
        after_pass_at(conn, "codex", &pass(10, 10, 1, 1), false, 1);
        assert!(all(conn).is_empty(), "a small watcher batch writes nothing");
        after_pass_at(conn, "codex", &pass(100, 0, 4, 0), true, 2);
        after_pass_at(conn, "codex", &pass(100, 80, 4, 0), true, 3);
        after_pass_at(conn, "claude_code", &pass(100, 0, 4, 0), true, 3);
        let stored = all(conn);
        assert_eq!(
            stored.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>(),
            ["claude_code", "codex"]
        );
        let drifts = open_drifts(conn);
        assert_eq!(drifts.len(), 1);
        assert_eq!(drifts[0].source, "codex");
        dismiss(conn, "codex").unwrap();
        assert!(open_drifts(conn).is_empty());
        assert_eq!(load(conn, "codex").passes.len(), 2);
        // An unrelated key that shares the prefix's first letters is not read.
        app_config::put(conn, "parse_healthy", "{}").unwrap();
        assert_eq!(all(conn).len(), 2);
    }

    /// Calibration on this machine's real history, read-only, into a temp index (TMPDIR):
    /// one full pass per source prints its counts, then every Claude Code transcript of at
    /// least MIN_LINES records is judged on its own against that pass, as a watcher batch
    /// over that one file would be. A warning there would be a false alarm on today's data.
    /// `TMPDIR=<scratch> cargo test --release -p callimachus-core real_parse_health --
    /// --ignored --nocapture`
    #[test]
    #[ignore = "reads the real agent history"]
    fn real_parse_health() {
        let mut t = crate::db::test_support::TestIndex::new("real_parse_health");
        for source in crate::indexer::SOURCES {
            let started = std::time::Instant::now();
            let report = crate::indexer::scan_counted(&mut t, source.kind, true, |c| {
                (source.scan)(c, &mut || {})
            })
            .unwrap();
            let h = load(&t, source.kind);
            let Some(last) = h.last.as_ref() else {
                continue;
            };
            eprintln!(
                "{:<12} {} | unknown+malformed {:.2}%, files without messages {:.2}% | {} threads in {:.1?}",
                source.kind,
                summary_line(&last.stats),
                h.rolling().bad_share() * 100.0,
                last.stats.files_with_zero_messages as f64 * 100.0
                    / last.stats.files.max(1) as f64,
                report.threads_indexed,
                started.elapsed(),
            );
            assert!(h.drift.is_none(), "a first pass never warns");
        }
        // A second full pass over files that did not change reads (nearly) nothing: what a
        // launch catch-up meets. Files an agent wrote meanwhile are read again.
        for source in crate::indexer::SOURCES {
            let (_, again) = crate::indexer::common::collect_parse_stats(|| {
                crate::indexer::scan_counted(&mut t, source.kind, true, |c| {
                    (source.scan)(c, &mut || {})
                })
                .unwrap()
            });
            if !again.is_empty() {
                eprintln!("{:<12} again: {}", source.kind, summary_line(&again));
            }
            assert!(load(&t, source.kind).drift.is_none(), "{}", source.kind);
        }
        let Some(root) = crate::indexer::claude::projects_root() else {
            return;
        };
        let base = load(&t, crate::indexer::claude::KIND);
        let mut files = Vec::new();
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "jsonl") {
                    out.push(p);
                }
            }
        }
        walk(&root, &mut files);
        let (mut judged, mut warned) = (0, Vec::new());
        for f in &files {
            let (_, stats) = crate::indexer::common::collect_parse_stats(|| {
                crate::indexer::claude::parse_file(f, "x", None)
            });
            if stats.lines < MIN_LINES {
                continue;
            }
            judged += 1;
            let mut h = base.clone();
            if let Some(d) = h.record(&stats, false, 0) {
                warned.push((d.share, d.zero_files, d.kinds));
            }
        }
        eprintln!(
            "claude_code: {} transcripts judged one by one, {} would warn: {:?}",
            judged,
            warned.len(),
            warned.iter().take(10).collect::<Vec<_>>()
        );
        assert!(
            warned.is_empty(),
            "a transcript of today's format would raise a false alarm"
        );
    }
}
