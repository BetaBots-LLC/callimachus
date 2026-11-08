//! `file_edits`: which files a thread read, edited or wrote, one row per tool call and path,
//! with the message and its time. Read from the tool calls every source stores as
//! `Name: {input}` assistant rows (`tool_name` set): Claude Code's Read / Edit / Write /
//! MultiEdit / NotebookEdit and the paths its Bash commands name, Codex's `apply_patch` and
//! shell, Gemini and Qwen's file tools, Cline (and Roo, Kilo) file tools, Cursor's and Copilot
//! CLI's. A path in any other tool call is a `mention`. Paths named in prose stay in
//! `file_mentions` (the per-thread summary); sources that store no tool calls (OpenCode,
//! Continue, Goose, VS Code Copilot chat) add no rows. Threads indexed before 0034 are filled
//! once from their stored messages by [`backfill_file_edits_once`] (and, for Codex, from the
//! `apply_patch` calls in the rollout files, which older indexes never stored).

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// What a tool call did to a path; the strings are 0034's CHECK values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Op {
    Read,
    Edit,
    Write,
    Mention,
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Read => "read",
            Op::Edit => "edit",
            Op::Write => "write",
            Op::Mention => "mention",
        }
    }

    /// Which rows the per-thread cap keeps first: edits and writes (what H3 ranks against
    /// commits), then reads, then mentions.
    fn keep_rank(self) -> u8 {
        match self {
            Op::Edit | Op::Write => 0,
            Op::Read => 1,
            Op::Mention => 2,
        }
    }
}

/// Rows per thread. A long Claude Code session reads and edits a few hundred files; the cap
/// only bounds a runaway loop (the same Bash command over thousands of paths). Past it, the
/// edits and writes are kept over the reads, and the newest over the oldest.
pub const MAX_ROWS_PER_THREAD: usize = 2_000;
/// Paths taken from one shell or generic tool call (a `git add` of a whole tree). A patch is
/// exempt: every file it names was edited.
const MAX_PATHS_PER_CALL: usize = 20;

/// A tool name reduced to its verb: the last `__` segment (MCP tools), lowercased, a version
/// suffix (`_v2`) dropped and `_` / `-` removed, so `StrReplace` (Cursor), `str_replace`,
/// `edit_file_v2` and `mcp__fs__read_file` all match one spelling.
fn verb_of(tool_name: &str) -> String {
    let last = tool_name.rsplit("__").next().unwrap_or(tool_name);
    let mut v = last.trim().to_ascii_lowercase();
    if let Some(i) = v.rfind(['_', '-']) {
        let suffix = &v[i + 1..];
        if suffix.len() >= 2
            && suffix.starts_with('v')
            && suffix[1..].bytes().all(|b| b.is_ascii_digit())
        {
            v.truncate(i);
        }
    }
    v.retain(|c| c != '_' && c != '-');
    v
}

/// Verbs (see [`verb_of`]) of the tools that run a shell command.
fn is_shell_verb(verb: &str) -> bool {
    matches!(
        verb,
        "bash"
            | "shell"
            | "runshellcommand"
            | "runshell"
            | "executecommand"
            | "runterminalcmd"
            | "runinterminal"
            | "execcommand"
            | "localshell"
            | "powershell"
    )
}

/// The (path, op) pairs one tool call names, in order, deduplicated. `tool_name` is the row's
/// `tool_name`; `text` is the stored `Name: {input}` (possibly capped mid-JSON, see
/// `claude::cap_tool_text`).
pub fn edits_of(tool_name: &str, text: &str) -> Vec<(String, Op)> {
    let name = tool_name.trim();
    let args = text
        .strip_prefix(name)
        .and_then(|r| r.strip_prefix(':'))
        .unwrap_or(text)
        .trim();
    let verb = verb_of(name);
    let verb = verb.as_str();
    let json: Option<Value> = serde_json::from_str(args).ok();
    let mut out: Vec<(String, Op)> = Vec::new();
    let is_shell = is_shell_verb(verb);

    // A patch in Codex's format, carried by `apply_patch` or a shell call (not a Write whose
    // content happens to quote one). Not capped per call: a refactor patch over 30 files
    // edited all 30.
    if (verb == "applypatch" || is_shell) && args.contains("*** Begin Patch") {
        let body = json
            .as_ref()
            .map(|v| flatten_strings(v).join("\n"))
            .unwrap_or_else(|| args.to_string());
        for (path, op) in patch_paths(&body) {
            push(&mut out, &path, op);
        }
        if !out.is_empty() {
            return out;
        }
    }

    let file_op = match verb {
        "read" | "readfile" | "view" | "notebookread" | "readmanyfiles" | "openfile" | "cat" => {
            Some(Op::Read)
        }
        "edit" | "multiedit" | "notebookedit" | "editnotebook" | "replace" | "replaceinfile"
        | "editfile" | "searchreplace" | "strreplace" | "applydiff" | "insertcontent"
        | "deletefile" | "delete" | "patch" | "applypatch" => Some(Op::Edit),
        "write" | "writefile" | "writetofile" | "create" | "createfile" | "notebookwrite" => {
            Some(Op::Write)
        }
        // Anthropic's text editor tool: the command decides.
        "strreplaceeditor" | "strreplacebasededittool" => {
            match json
                .as_ref()
                .and_then(|v| v.get("command"))
                .and_then(Value::as_str)
            {
                Some("view") => Some(Op::Read),
                Some("create") => Some(Op::Write),
                _ => Some(Op::Edit),
            }
        }
        _ => None,
    };

    if let Some(op) = file_op {
        match &json {
            Some(v) => {
                for p in path_args(v) {
                    push(&mut out, &p, op);
                }
            }
            // Capped mid-JSON (a Write of a big file): the path leads the input.
            None => {
                for key in PATH_KEYS {
                    if let Some(p) = json_string_after(args, key) {
                        push(&mut out, &p, op);
                        break;
                    }
                }
            }
        }
    } else if is_shell {
        let command = match &json {
            Some(Value::Object(o)) => o
                .get("command")
                .or_else(|| o.get("cmd"))
                .map(|c| match c {
                    Value::Array(parts) => {
                        let argv: Vec<&str> = parts.iter().filter_map(Value::as_str).collect();
                        // Codex runs most commands as `bash -lc "<script>"`: the script is
                        // the command.
                        match argv.as_slice() {
                            [sh, flag, script]
                                if matches!(*sh, "bash" | "sh" | "zsh")
                                    && flag.starts_with('-')
                                    && flag.contains('c') =>
                            {
                                script.to_string()
                            }
                            _ => argv.join(" "),
                        }
                    }
                    other => other.as_str().unwrap_or_default().to_string(),
                })
                .unwrap_or_default(),
            _ => json_string_after(args, "command").unwrap_or_else(|| args.to_string()),
        };
        for (p, op) in shell_paths(&command) {
            push(&mut out, &p, op);
        }
    } else if let Some(v) = &json {
        for p in path_args(v) {
            push(&mut out, &p, Op::Mention);
        }
    }
    out.truncate(MAX_PATHS_PER_CALL);
    out
}

/// The working directory a shell call names for itself (Codex's `workdir`, a `cwd` or
/// `directory` argument), when it is absolute. Relative paths in that call resolve against
/// it, not against where the session started.
pub fn call_cwd(tool_name: &str, text: &str) -> Option<String> {
    if !is_shell_verb(&verb_of(tool_name)) {
        return None;
    }
    ["workdir", "cwd", "directory"]
        .into_iter()
        .find_map(|key| json_string_after(text, key))
        .map(|d| d.trim().to_string())
        .filter(|d| d.starts_with('/') || super::common::is_drive_path(d))
}

/// Add (path, op) unless it is empty, absurdly long or already there.
fn push(out: &mut Vec<(String, Op)>, path: &str, op: Op) {
    let p = path.trim();
    if !p.is_empty() && p.len() <= 400 && !out.iter().any(|(q, o)| q == p && *o == op) {
        out.push((p.to_string(), op));
    }
}

/// Input keys that hold a file path, across the sources' tool schemas.
const PATH_KEYS: &[&str] = &[
    "file_path",
    "notebook_path",
    "path",
    "filePath",
    "absolute_path",
    "target_file",
    "file",
    "filename",
];

/// The path-valued arguments of a tool input: the [`PATH_KEYS`], and `paths` / `files` arrays
/// (Gemini's read_many_files), at the top level or in MultiEdit-style `edits` items.
fn path_args(v: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(o) = v.as_object() else {
        return out;
    };
    for key in PATH_KEYS {
        if let Some(p) = o.get(*key).and_then(Value::as_str) {
            out.push(p.to_string());
        }
    }
    for key in ["paths", "files", "file_paths"] {
        if let Some(arr) = o.get(key).and_then(Value::as_array) {
            out.extend(arr.iter().filter_map(Value::as_str).map(str::to_string));
        }
    }
    out
}

/// Every string inside a JSON value (a shell call's argv, an apply_patch input).
fn flatten_strings(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a.iter().flat_map(flatten_strings).collect(),
        Value::Object(o) => o.values().flat_map(flatten_strings).collect(),
        _ => Vec::new(),
    }
}

/// The JSON string value right after `"key":` in possibly truncated JSON text.
fn json_string_after(text: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let at = text.find(&needle)? + needle.len();
    let rest = text[at..].trim_start().strip_prefix(':')?.trim_start();
    if !rest.starts_with('"') {
        return None;
    }
    // Find the closing quote, skipping escaped ones, then let serde unescape the literal.
    let bytes = rest.as_bytes();
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return serde_json::from_str(&rest[..=i]).ok(),
            _ => i += 1,
        }
    }
    None
}

/// Files a Codex-format patch touches: `*** Update File:` and `*** Delete File:` edit,
/// `*** Add File:` and `*** Move to:` write.
fn patch_paths(patch: &str) -> Vec<(String, Op)> {
    let mut out = Vec::new();
    for line in patch.lines() {
        let line = line.trim();
        let hit = [
            ("*** Update File:", Op::Edit),
            ("*** Delete File:", Op::Edit),
            ("*** Add File:", Op::Write),
            ("*** Move to:", Op::Write),
        ]
        .into_iter()
        .find_map(|(marker, op)| {
            line.strip_prefix(marker)
                .map(|p| (p.trim().to_string(), op))
        });
        if let Some(hit) = hit {
            out.push(hit);
        }
    }
    out
}

/// Programs whose path arguments are read. Codex reads files mostly with `sed -n 'a,bp'`,
/// `rg`, `grep` and `awk` (a `sed` with `-i` is an edit, checked first).
const READERS: &[&str] = &[
    "cat", "head", "tail", "less", "more", "bat", "nl", "wc", "view", "type", "diff", "sed", "rg",
    "grep", "egrep", "awk", "jq",
];

/// The tag of a heredoc this command line opens (`<<EOF`, `<<-'EOF'`, `<<"END"`), or None.
/// A here-string (`<<<`) and a shift inside arithmetic (`$((1<<3))`) open none, and the tag
/// must be a word: anything else would hide every later line of the command as body.
fn heredoc_tag(line: &str) -> Option<String> {
    let mut from = 0;
    while let Some(off) = line[from..].find("<<") {
        let at = from + off;
        from = at + 2;
        let after = &line[at + 2..];
        if after.starts_with('<') {
            from = at + 3;
            continue;
        }
        let before = &line[..at];
        if before.ends_with('<') || before.matches("((").count() > before.matches("))").count() {
            continue;
        }
        let rest = after
            .trim_start_matches('-')
            .trim_start()
            .trim_start_matches(['\'', '"', '\\']);
        let tag: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if tag.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
            return Some(tag);
        }
    }
    None
}

/// The paths a shell command names, each with what the command does to it: a redirect or
/// `tee` target is written, a `sed -i` / `perl -i` target edited, a reader's argument read,
/// and anything else mentioned. Path-like tokens only (the indexer's `extract_paths`: a slash
/// plus an extension, or a known code extension), so `cd`, flags and globs drop out.
fn shell_paths(command: &str) -> Vec<(String, Op)> {
    let mut out: Vec<(String, Op)> = Vec::new();
    // Only the command lines, not a heredoc body (a commit message, a file being written).
    let mut lines = Vec::new();
    let mut heredoc: Option<String> = None;
    for line in command.lines() {
        if let Some(end) = &heredoc {
            if line.trim() == end {
                heredoc = None;
            }
            continue;
        }
        lines.push(line);
        heredoc = heredoc_tag(line);
    }
    let joined = lines.join("\n");
    for segment in joined.split(['\n', ';', '|', '&']) {
        let words: Vec<&str> = segment.split_whitespace().collect();
        // The program: the first word that is not an env assignment (`A=b cmd`).
        let program = words
            .iter()
            .find(|w| !w.contains('='))
            .map(|w| w.rsplit('/').next().unwrap_or(w))
            .unwrap_or("");
        let in_place = (program == "sed" || program == "perl")
            && words
                .iter()
                .any(|w| w.starts_with("-i") || (w.starts_with("-p") && w.contains('i')));
        let default = if in_place {
            Op::Edit
        } else if READERS.contains(&program) {
            Op::Read
        } else {
            Op::Mention
        };
        let mut next_is_target = false;
        for w in &words {
            let (redirect, rest) = match w.strip_prefix(">>").or_else(|| w.strip_prefix('>')) {
                Some(r) => (true, r),
                None => (false, *w),
            };
            let target = next_is_target || (redirect && !rest.is_empty());
            // `> file` (the target is the next word), `tee [-a] file`.
            next_is_target =
                (redirect && rest.is_empty()) || *w == "tee" || (*w == "-a" && next_is_target);
            for p in path_tokens(rest) {
                let op = if target { Op::Write } else { default };
                if !out.iter().any(|(q, o)| *q == p && *o == op) {
                    out.push((p, op));
                }
            }
        }
    }
    out
}

/// The paths in one shell word, by the indexer's `extract_paths` rule, with the leading dots
/// it trims put back (`.github/ci.yml`, `./src/a.rs`, `../x.rs` keep their meaning) and
/// shell variables and globs dropped (`$OUT/a.txt`, `src/*.rs` name no one file).
fn path_tokens(word: &str) -> Vec<String> {
    let mut out = Vec::new();
    for p in crate::indexer::extract_paths(word) {
        if p.contains(['$', '*', '?', '{', '}']) {
            continue;
        }
        let Some(at) = word.find(&p) else {
            out.push(p);
            continue;
        };
        let dots = word[..at].bytes().rev().take_while(|b| *b == b'.').count();
        out.push(format!("{}{p}", &word[at - dots..at]));
    }
    out
}

/// The checkout a thread's paths are stored relative to: the nearest ancestor of `cwd` with a
/// `.git` (a directory, or a linked worktree's file), lexically, so it keeps the spelling the
/// agent used. Never above the home directory: a dotfiles repo in ~ is the root only of a
/// session that ran in ~ itself. Falls back to `cwd` when no ancestor has one, and when `cwd`
/// is gone from this machine: walking up from a removed worktree (`<repo>/.claude/worktrees/x`
/// after its merge) would find the enclosing repo and store `.claude/worktrees/x/src/a.rs`
/// where the rows written while it existed say `src/a.rs`.
pub fn checkout_root(cwd: &str) -> String {
    checkout_root_with_home(cwd, dirs::home_dir().as_deref())
}

fn checkout_root_with_home(cwd: &str, home: Option<&std::path::Path>) -> String {
    let cwd = cwd.trim().trim_end_matches(['/', '\\']);
    let start = std::path::Path::new(cwd);
    if !start.is_dir() {
        return cwd.to_string();
    }
    let mut dir = start;
    loop {
        if home == Some(dir) && dir != start {
            break;
        }
        if dir.join(".git").exists() {
            return dir.to_string_lossy().into_owned();
        }
        match dir.parent() {
            Some(p) if !p.as_os_str().is_empty() => dir = p,
            _ => break,
        }
    }
    cwd.to_string()
}

/// Where a thread's tool-call paths are anchored: its checkout root, and its cwd relative to
/// that root ("" when the session ran at the root).
#[derive(Debug, Clone, Default)]
pub struct Anchor {
    root: String,
    cwd_rel: String,
}

impl Anchor {
    /// The anchor for a session that ran in `cwd` (None: no project, paths stay as written).
    /// Probes the disk (see [`checkout_root`]), so callers resolve it before they open a write
    /// transaction: a cwd on a sleeping volume must not stall while holding the lock.
    pub fn for_cwd(cwd: Option<&str>) -> Self {
        let Some(cwd) = cwd.map(str::trim).filter(|c| !c.is_empty()) else {
            return Self::default();
        };
        Self::with_root(&checkout_root(cwd), cwd)
    }

    fn with_root(root: &str, cwd: &str) -> Self {
        let root = slashes(root);
        let cwd = slashes(cwd.trim_end_matches(['/', '\\']));
        let cwd_rel = strip_root(&cwd, &root).unwrap_or("").to_string();
        Self { root, cwd_rel }
    }

    /// `raw` as stored: relative to the checkout root when it lies inside it (an absolute
    /// path under the root, or a relative one, which the agent resolved against its cwd,
    /// `../` segments included while they stay inside the checkout), otherwise as written.
    /// Separators become `/` and a leading `./` goes.
    pub fn normalize(&self, raw: &str) -> String {
        let p = slashes(raw.trim());
        let p = p.strip_prefix("./").unwrap_or(&p).to_string();
        if self.root.is_empty() {
            return p;
        }
        if let Some(rest) = strip_root(&p, &self.root) {
            return lexical(rest).unwrap_or_else(|| p.clone());
        }
        let absolute = p.starts_with('/') || p.starts_with('~') || super::common::is_drive_path(&p);
        if absolute {
            return p;
        }
        let joined = if self.cwd_rel.is_empty() {
            p.clone()
        } else {
            format!("{}/{p}", self.cwd_rel)
        };
        // A `../` that climbs above the checkout names a file outside it: keep it as written.
        lexical(&joined).unwrap_or(p)
    }
}

/// `p` below `root` ("" for the root itself), or None. Windows paths compare without case
/// (`c:/Users/x/app` from a URI and `C:/Users/x/app` are one directory).
fn strip_root<'a>(p: &'a str, root: &str) -> Option<&'a str> {
    let head = p.get(..root.len())?;
    let same = if super::common::is_drive_path(root) {
        head.eq_ignore_ascii_case(root)
    } else {
        head == root
    };
    if !same {
        return None;
    }
    let rest = &p[root.len()..];
    if rest.is_empty() {
        Some("")
    } else {
        rest.strip_prefix('/')
    }
}

/// A relative path with `.` and `..` segments resolved, or None when it climbs above its
/// start.
fn lexical(rel: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

fn slashes(p: &str) -> String {
    p.replace('\\', "/")
}

/// The anchors one thread's tool calls resolve against: the thread's own (its recorded
/// project path) and one per working directory a call ran in, all probed up front so the
/// write transaction does no disk I/O.
#[derive(Debug, Clone, Default)]
pub struct Anchors {
    thread: Anchor,
    by_cwd: HashMap<String, Anchor>,
}

impl Anchors {
    /// Resolve the thread's anchor and one for each per-call cwd in `call_cwds` (see
    /// [`ToolRow::cwd`] and [`call_cwd`]).
    pub fn resolve<'a>(
        thread_cwd: Option<&str>,
        call_cwds: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        let thread = Anchor::for_cwd(thread_cwd);
        let mut by_cwd = HashMap::new();
        for cwd in call_cwds {
            if !by_cwd.contains_key(cwd) {
                by_cwd.insert(cwd.to_string(), Anchor::for_cwd(Some(cwd)));
            }
        }
        Self { thread, by_cwd }
    }

    /// The cwds `rows` ran in, for [`Anchors::resolve`].
    pub fn cwds_of<'a>(rows: &'a [ToolRow<'a>]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for r in rows {
            let Some(name) = r.tool_name else {
                continue;
            };
            for c in call_cwd(name, r.text)
                .into_iter()
                .chain(r.cwd.map(str::to_string))
            {
                if !out.contains(&c) {
                    out.push(c);
                }
            }
        }
        out
    }

    /// The anchor of one call: the cwd the call names, else the one its source recorded for
    /// the message, else the thread's. A cwd [`Anchors::resolve`] did not see falls back to
    /// the thread's rather than probing the disk here.
    fn for_row(&self, r: &ToolRow) -> &Anchor {
        let named = r.tool_name.and_then(|n| call_cwd(n, r.text));
        named
            .as_deref()
            .or(r.cwd)
            .and_then(|c| self.by_cwd.get(c))
            .unwrap_or(&self.thread)
    }
}

/// The paths a thread already mentions (an append's seed), off idx_file_edits_thread.
pub(crate) const MENTIONED_SQL: &str =
    "SELECT path FROM file_edits WHERE thread_id = ?1 AND op = 'mention'";

/// One tool-call row, for [`derive`]: a message just written, or (the Codex backfill) a call
/// read from a rollout file with no stored message (`id` None).
pub struct ToolRow<'a> {
    pub id: Option<i64>,
    pub tool_name: Option<&'a str>,
    pub text: &'a str,
    pub ts: Option<i64>,
    /// The working directory the source recorded for this message (Claude Code writes one
    /// per line), when it has one.
    pub cwd: Option<&'a str>,
}

/// One `file_edits` row to write.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EditRow {
    message_id: Option<i64>,
    path: String,
    op: Op,
    ts: Option<i64>,
}

/// The rows `rows` yield, in order: one per call and (path, op), a mention only the first
/// time the thread names it (`mentioned` holds the paths it already did).
fn plan(anchors: &Anchors, rows: &[ToolRow], mentioned: &mut HashSet<String>) -> Vec<EditRow> {
    let mut out = Vec::new();
    let mut seen: HashSet<(String, Op)> = HashSet::new();
    for r in rows {
        let Some(name) = r.tool_name else {
            continue;
        };
        let anchor = anchors.for_row(r);
        seen.clear();
        for (raw, op) in edits_of(name, r.text) {
            let path = anchor.normalize(&raw);
            if path.is_empty() || !seen.insert((path.clone(), op)) {
                continue;
            }
            // A mention is kept once per thread (its first sighting): shell commands name
            // the same few files over and over (`package.json` 700 times across 500
            // sessions), and two thirds of the rows were repeats that say nothing a first
            // sighting does not.
            if op == Op::Mention && !mentioned.insert(path.clone()) {
                continue;
            }
            out.push(EditRow {
                message_id: r.id,
                path,
                op,
                ts: r.ts,
            });
        }
    }
    out
}

/// At most `room` of `rows`, in their original order: edits and writes first, then reads,
/// then mentions, the newest of each first. A thread that read hundreds of files early (a
/// runaway read loop, a long compacted session) must not crowd out its late edits, the rows
/// nearest the commits H3 ranks.
fn keep_most_useful(rows: Vec<EditRow>, room: usize) -> Vec<EditRow> {
    if rows.len() <= room {
        return rows;
    }
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by_key(|&i| {
        let r = &rows[i];
        (
            r.op.keep_rank(),
            std::cmp::Reverse(r.ts.unwrap_or(i64::MIN)),
            std::cmp::Reverse(i),
        )
    });
    let keep: HashSet<usize> = order.into_iter().take(room).collect();
    rows.into_iter()
        .enumerate()
        .filter(|(i, _)| keep.contains(i))
        .map(|(_, r)| r)
        .collect()
}

const INSERT_SQL: &str =
    "INSERT INTO file_edits (thread_id, message_id, path, op, ts) VALUES (?1, ?2, ?3, ?4, ?5)";

/// Over the cap after an append: drop the least useful rows (mentions, then reads, the oldest
/// first) down to [`MAX_ROWS_PER_THREAD`].
const TRIM_SQL: &str = "DELETE FROM file_edits WHERE rowid IN (
        SELECT rowid FROM file_edits WHERE thread_id = ?1
        ORDER BY CASE op WHEN 'mention' THEN 0 WHEN 'read' THEN 1 ELSE 2 END,
                 ts IS NOT NULL, ts, rowid
        LIMIT ?2)";

fn insert_rows(tx: &Connection, thread_id: i64, rows: &[EditRow]) -> rusqlite::Result<()> {
    let mut ins = tx.prepare_cached(INSERT_SQL)?;
    for r in rows {
        ins.execute(params![
            thread_id,
            r.message_id,
            r.path,
            r.op.as_str(),
            r.ts
        ])?;
    }
    Ok(())
}

/// Write the `file_edits` rows for `rows` (a whole thread after a full replace, which first
/// clears the thread's rows, or an appended tail), keeping the thread within
/// [`MAX_ROWS_PER_THREAD`] (see [`keep_most_useful`]). Returns the rows written.
pub fn derive(
    tx: &Connection,
    thread_id: i64,
    anchors: &Anchors,
    rows: &[ToolRow],
    appended: bool,
) -> rusqlite::Result<usize> {
    if !appended {
        tx.execute("DELETE FROM file_edits WHERE thread_id = ?1", [thread_id])?;
    }
    if !rows.iter().any(|r| r.tool_name.is_some()) {
        return Ok(0);
    }
    let mut mentioned: HashSet<String> = HashSet::new();
    if appended {
        let mut stmt = tx.prepare_cached(MENTIONED_SQL)?;
        for p in stmt.query_map([thread_id], |r| r.get::<_, String>(0))? {
            mentioned.insert(p?);
        }
    }
    let planned = keep_most_useful(plan(anchors, rows, &mut mentioned), MAX_ROWS_PER_THREAD);
    if planned.is_empty() {
        return Ok(0);
    }
    insert_rows(tx, thread_id, &planned)?;
    if appended {
        let total: i64 = tx.query_row(
            "SELECT COUNT(*) FROM file_edits WHERE thread_id = ?1",
            [thread_id],
            |r| r.get(0),
        )?;
        let over = total - MAX_ROWS_PER_THREAD as i64;
        if over > 0 {
            tx.execute(TRIM_SQL, params![thread_id, over])?;
        }
    }
    Ok(planned.len())
}

// ---- the one-time backfill ------------------------------------------------------------

/// Set by 0034 on an index that already held threads: their `file_edits` are filled from the
/// stored messages by [`backfill_file_edits_once`], which clears it when a pass completes.
/// A Reindex never re-parses an unchanged transcript (each source's mtime and size cursor),
/// and resetting the cursors would churn every message id and re-embed the whole history.
pub const BACKFILL_PENDING: &str = "file_edits.backfill_pending";

/// Most threads per backfill write batch.
const BACKFILL_BATCH: usize = 50;
/// A batch stops gathering once its reads and derivation have run this long, so the GUI
/// connection (shared with star, tag and Remember) is never held much longer.
const BACKFILL_BATCH_TIME: std::time::Duration = std::time::Duration::from_millis(200);
/// How often a paused backfill checks whether the writer job it yields to is done.
const BACKFILL_PAUSE_POLL: std::time::Duration = std::time::Duration::from_secs(2);
/// How often the backfill logs its progress.
const BACKFILL_LOG_EVERY: std::time::Duration = std::time::Duration::from_secs(10);

/// What a backfill pass did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BackfillReport {
    /// Threads that had no rows when the pass started.
    pub threads: usize,
    /// Threads the pass wrote rows for.
    pub filled: usize,
    pub rows: usize,
}

/// Fill `file_edits` for the threads indexed before 0034, once: runs [`backfill_file_edits`]
/// when 0034 set [`BACKFILL_PENDING`] and clears it after a full pass (an interrupted pass
/// runs again next time and skips the threads that already have rows). Returns None when
/// nothing was pending. For the watcher (after its launch catch-up, like the TODO backfill)
/// and `cal index`; `pause` says a writer job (reindex, embed build, rewrite) is running, and
/// the backfill waits between batches while it does.
pub fn backfill_file_edits_once(
    db: &crate::db::Db,
    pause: &dyn Fn() -> bool,
) -> anyhow::Result<Option<BackfillReport>> {
    let lock = || db.0.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"));
    if !crate::knowledge::app_config::get_flag(&*lock()?, BACKFILL_PENDING)? {
        return Ok(None);
    }
    let report = backfill_file_edits(db, pause)?;
    crate::knowledge::app_config::delete(&*lock()?, BACKFILL_PENDING)?;
    Ok(Some(report))
}

/// One gathered thread, ready to write.
struct Planned {
    thread_id: i64,
    rows: Vec<EditRow>,
}

/// Derive `file_edits` for every thread that has none, from its stored tool-call messages
/// (and, for a Codex thread whose `apply_patch` calls were never stored, from its rollout
/// file), in batches: rows are read and derived without the write lock (the checkout probes
/// touch the disk), then each batch is written in one short IMMEDIATE transaction that
/// re-checks the thread still has no rows (the watcher may have indexed it meanwhile).
pub fn backfill_file_edits(
    db: &crate::db::Db,
    pause: &dyn Fn() -> bool,
) -> anyhow::Result<BackfillReport> {
    let lock = || db.0.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"));
    let started = std::time::Instant::now();
    let (thread_ids, rollouts) = {
        let conn = lock()?;
        let mut stmt = conn.prepare(
            "SELECT t.id FROM threads t
             WHERE NOT EXISTS (SELECT 1 FROM file_edits f WHERE f.thread_id = t.id)
             ORDER BY t.id",
        )?;
        let ids: Vec<i64> = stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        (ids, codex_rollouts(&conn)?)
    };
    let mut report = BackfillReport {
        threads: thread_ids.len(),
        ..Default::default()
    };
    let mut last_log = std::time::Instant::now();
    let mut next = 0;
    while next < thread_ids.len() {
        while pause() {
            std::thread::sleep(BACKFILL_PAUSE_POLL);
        }
        let batch_start = std::time::Instant::now();
        let mut batch: Vec<Planned> = Vec::new();
        while next < thread_ids.len()
            && batch.len() < BACKFILL_BATCH
            && batch_start.elapsed() < BACKFILL_BATCH_TIME
        {
            let id = thread_ids[next];
            next += 1;
            let rows = plan_stored_thread(db, id, &rollouts)?;
            if !rows.is_empty() {
                batch.push(Planned {
                    thread_id: id,
                    rows,
                });
            }
        }
        if !batch.is_empty() {
            let mut conn = lock()?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            for p in &batch {
                let free: bool = tx.query_row(
                    "SELECT EXISTS (SELECT 1 FROM threads WHERE id = ?1)
                        AND NOT EXISTS (SELECT 1 FROM file_edits WHERE thread_id = ?1)",
                    [p.thread_id],
                    |r| r.get(0),
                )?;
                if free {
                    insert_rows(&tx, p.thread_id, &p.rows)?;
                    report.filled += 1;
                    report.rows += p.rows.len();
                }
            }
            tx.commit()?;
        }
        if last_log.elapsed() >= BACKFILL_LOG_EVERY {
            last_log = std::time::Instant::now();
            app_log!(
                "[index] file edits backfill: {next}/{} sessions, {} rows, {:.0}s",
                report.threads,
                report.rows,
                started.elapsed().as_secs_f64()
            );
        }
    }
    app_log!(
        "[index] file edits backfill done: {} of {} sessions filled, {} rows in {:.1}s",
        report.filled,
        report.threads,
        report.rows,
        started.elapsed().as_secs_f64()
    );
    Ok(report)
}

/// The rows one stored thread yields (see [`backfill_file_edits`]). Reads under the
/// connection lock, derives without it.
fn plan_stored_thread(
    db: &crate::db::Db,
    thread_id: i64,
    rollouts: &HashMap<String, std::path::PathBuf>,
) -> anyhow::Result<Vec<EditRow>> {
    type Stored = (Option<i64>, String, String, Option<i64>);
    let (project_path, codex_id, stored): (Option<String>, Option<String>, Vec<Stored>) = {
        let conn = db.0.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
        let Some((project_path, kind, external_id)) = conn
            .query_row(
                "SELECT t.project_path, s.kind, t.external_id
                 FROM threads t JOIN sources s ON s.id = t.source_id WHERE t.id = ?1",
                [thread_id],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
        else {
            return Ok(Vec::new());
        };
        let mut stmt = conn.prepare_cached(
            "SELECT id, tool_name, text, ts FROM messages
             WHERE thread_id = ?1 AND tool_name IS NOT NULL ORDER BY seq",
        )?;
        let stored = stmt
            .query_map([thread_id], |r| {
                Ok((Some(r.get(0)?), r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<rusqlite::Result<Vec<Stored>>>()?;
        let codex = (kind == super::codex::KIND).then_some(external_id);
        (project_path, codex, stored)
    };
    let mut calls = stored;
    // An index from before 0034 skipped Codex's freeform tool calls, so its `apply_patch`
    // edits (the main way Codex edits files) exist only in the rollout file.
    if let Some(path) = codex_id
        .filter(|_| !calls.iter().any(|(_, name, _, _)| name == "apply_patch"))
        .and_then(|id| rollouts.get(&id))
    {
        if let Ok(extra) = super::codex::custom_tool_calls(path) {
            calls.extend(
                extra
                    .into_iter()
                    .map(|(ts, name, text)| (None, name, text, ts)),
            );
            // Rollout calls carry only a time: keep the thread's calls in time order so the
            // cap's "newest" means the same thing for both kinds.
            calls.sort_by_key(|c| c.3.unwrap_or(i64::MIN));
        }
    }
    if calls.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<ToolRow> = calls
        .iter()
        .map(|(id, name, text, ts)| ToolRow {
            id: *id,
            tool_name: Some(name.as_str()),
            text: text.as_str(),
            ts: *ts,
            cwd: None,
        })
        .collect();
    let cwds = Anchors::cwds_of(&rows);
    let anchors = Anchors::resolve(project_path.as_deref(), cwds.iter().map(String::as_str));
    Ok(keep_most_useful(
        plan(&anchors, &rows, &mut HashSet::new()),
        MAX_ROWS_PER_THREAD,
    ))
}

/// Codex rollout files the index has seen, by session id (the thread's `external_id`).
fn codex_rollouts(conn: &Connection) -> rusqlite::Result<HashMap<String, std::path::PathBuf>> {
    let mut stmt = conn.prepare("SELECT path FROM index_state WHERE source_kind = ?1")?;
    let paths: Vec<String> = stmt
        .query_map([super::codex::KIND], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(paths
        .into_iter()
        .map(std::path::PathBuf::from)
        .filter_map(|p| super::codex::rollout_session_id(&p).map(|id| (id, p)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ops(tool: &str, text: &str) -> Vec<(String, &'static str)> {
        edits_of(tool, text)
            .into_iter()
            .map(|(p, o)| (p, o.as_str()))
            .collect()
    }

    fn v(pairs: &[(&str, &'static str)]) -> Vec<(String, &'static str)> {
        pairs.iter().map(|(p, o)| (p.to_string(), *o)).collect()
    }

    #[test]
    fn claude_code_file_tools() {
        assert_eq!(
            ops(
                "Read",
                r#"Read: {"file_path":"/r/app/src/a.rs","limit":40}"#
            ),
            v(&[("/r/app/src/a.rs", "read")])
        );
        assert_eq!(
            ops(
                "Edit",
                r#"Edit: {"file_path":"/r/app/src/a.rs","old_string":"x","new_string":"y"}"#
            ),
            v(&[("/r/app/src/a.rs", "edit")])
        );
        assert_eq!(
            ops(
                "MultiEdit",
                r#"MultiEdit: {"file_path":"/r/b.ts","edits":[]}"#
            ),
            v(&[("/r/b.ts", "edit")])
        );
        assert_eq!(
            ops(
                "NotebookEdit",
                r#"NotebookEdit: {"notebook_path":"/r/n.ipynb","new_source":"x"}"#
            ),
            v(&[("/r/n.ipynb", "edit")])
        );
        // A Write capped mid-content is no longer JSON; its path still leads the input.
        let capped = format!(
            r#"Write: {{"file_path":"/r/app/big \"q\".rs","content":"{}"#,
            "x".repeat(100)
        );
        assert_eq!(
            ops("Write", &capped),
            v(&[("/r/app/big \"q\".rs", "write")])
        );
    }

    #[test]
    fn shell_commands_name_what_they_do() {
        assert_eq!(
            ops(
                "Bash",
                r#"Bash: {"command":"cat src/a.rs | head -5 > out/log.txt","description":"x"}"#
            ),
            v(&[("src/a.rs", "read"), ("out/log.txt", "write")])
        );
        assert_eq!(
            ops(
                "Bash",
                r#"Bash: {"command":"sed -i '' 's/a/b/' core/src/x.rs && cargo test"}"#
            ),
            v(&[("core/src/x.rs", "edit")])
        );
        assert_eq!(
            ops("Bash", r#"Bash: {"command":"git add src/a.rs src/b.rs"}"#),
            v(&[("src/a.rs", "mention"), ("src/b.rs", "mention")])
        );
        assert_eq!(
            ops(
                "Bash",
                r#"Bash: {"command":"echo hi | tee -a notes/todo.md"}"#
            ),
            v(&[("notes/todo.md", "write")])
        );
        // Leading dots survive; variables and globs name no one file.
        assert_eq!(
            ops(
                "Bash",
                r#"Bash: {"command":"cat .github/workflows/ci.yml ./src/a.rs $OUT/x.txt src/*.rs"}"#
            ),
            v(&[(".github/workflows/ci.yml", "read"), ("./src/a.rs", "read")])
        );
        // A heredoc body (a commit message) is not the command.
        let commit = r#"Bash: {"command":"git commit -F - <<'EOF'\nfix: touch lib/z.rs\nEOF"}"#;
        assert!(ops("Bash", commit).is_empty(), "{:?}", ops("Bash", commit));
        // Codex: argv arrays, Gemini: run_shell_command.
        assert_eq!(
            ops(
                "shell",
                r#"shell: {"command":["bash","-lc","cat README.md"],"workdir":"/r"}"#
            ),
            v(&[("README.md", "read")])
        );
        assert_eq!(
            ops(
                "run_shell_command",
                r#"run_shell_command: {"command":"wc -l src/a.py"}"#
            ),
            v(&[("src/a.py", "read")])
        );
    }

    #[test]
    fn codex_patches_and_other_sources() {
        let patch = "apply_patch: *** Begin Patch\n*** Update File: src/a.rs\n@@\n-x\n+y\n*** Add File: src/new.rs\n+fn f() {}\n*** Delete File: old.rs\n*** End Patch";
        assert_eq!(
            ops("apply_patch", patch),
            v(&[
                ("src/a.rs", "edit"),
                ("src/new.rs", "write"),
                ("old.rs", "edit")
            ])
        );
        // The same patch through a shell call's argv.
        let shell = r#"shell: {"command":["apply_patch","*** Begin Patch\n*** Update File: src/a.rs\n*** End Patch"]}"#;
        assert_eq!(ops("shell", shell), v(&[("src/a.rs", "edit")]));
        assert_eq!(
            ops(
                "write_file",
                r#"write_file: {"file_path":"a/b.go","content":"x"}"#
            ),
            v(&[("a/b.go", "write")])
        );
        assert_eq!(
            ops(
                "replace_in_file",
                r#"replace_in_file: {"path":"src/c.ts","diff":"..."}"#
            ),
            v(&[("src/c.ts", "edit")])
        );
        assert_eq!(
            ops(
                "read_many_files",
                r#"read_many_files: {"paths":["a.md","b/c.md"]}"#
            ),
            v(&[("a.md", "read"), ("b/c.md", "read")])
        );
        assert_eq!(
            ops(
                "str_replace_editor",
                r#"str_replace_editor: {"command":"view","path":"/r/x.py"}"#
            ),
            v(&[("/r/x.py", "read")])
        );
        // An unknown tool with a path argument mentions it; one without names nothing.
        assert_eq!(
            ops(
                "mcp__ide__getDiagnostics",
                r#"mcp__ide__getDiagnostics: {"uri":"x","path":"src/a.rs"}"#
            ),
            v(&[("src/a.rs", "mention")])
        );
        assert!(ops("WebSearch", r#"WebSearch: {"query":"sqlite fts5 a.rs"}"#).is_empty());
    }

    #[test]
    fn mentions_are_kept_once_per_thread_and_edits_per_call() {
        let p = std::env::temp_dir().join(format!("calli_fe_{}.db", std::process::id()));
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        let conn = crate::db::open(&p).unwrap();
        conn.execute(
            "INSERT INTO threads (source_id, external_id, title) VALUES (1, 't', 't')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        let git_add = r#"Bash: {"command":"git add src/a.rs"}"#;
        let edit = r#"Edit: {"file_path":"src/a.rs"}"#;
        let row = |id: i64, name: &'static str, text: &'static str| ToolRow {
            id: Some(id),
            tool_name: Some(name),
            text,
            ts: Some(id),
            cwd: None,
        };
        let a = Anchors::default();
        let first = [
            row(1, "Bash", git_add),
            row(2, "Edit", edit),
            row(3, "Bash", git_add),
            row(4, "Edit", edit),
        ];
        assert_eq!(derive(&conn, tid, &a, &first, false).unwrap(), 3);
        // An append seeds the thread's mentions; a full replace starts over.
        assert_eq!(
            derive(&conn, tid, &a, &[row(5, "Bash", git_add)], true).unwrap(),
            0
        );
        assert_eq!(derive(&conn, tid, &a, &first, false).unwrap(), 3);
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM file_edits WHERE thread_id = ?1",
                [tid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 3);
        let plan: String = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {MENTIONED_SQL}"))
            .unwrap()
            .query_map([tid], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join(" | ");
        assert!(plan.contains("idx_file_edits_thread"), "{plan}");
    }

    #[test]
    fn paths_anchor_to_the_checkout() {
        let a = Anchor {
            root: "/r/app".into(),
            cwd_rel: "apps/web".into(),
        };
        assert_eq!(a.normalize("/r/app/src/a.rs"), "src/a.rs");
        assert_eq!(a.normalize("src/page.tsx"), "apps/web/src/page.tsx");
        assert_eq!(a.normalize("./src/page.tsx"), "apps/web/src/page.tsx");
        assert_eq!(a.normalize("/elsewhere/x.rs"), "/elsewhere/x.rs");
        // `../` resolves while it stays inside the checkout, and is kept as written once it
        // climbs out.
        assert_eq!(a.normalize("../api/x.rs"), "apps/api/x.rs");
        assert_eq!(a.normalize("../../README.md"), "README.md");
        assert_eq!(a.normalize("../../../other/x.rs"), "../../../other/x.rs");
        assert_eq!(a.normalize("/r/app/src/../lib/a.rs"), "lib/a.rs");
        assert_eq!(a.normalize("/r/apple/x.rs"), "/r/apple/x.rs");
        let root = Anchor {
            root: "C:/r/app".into(),
            cwd_rel: String::new(),
        };
        assert_eq!(root.normalize("C:\\r\\app\\src\\a.rs"), "src/a.rs");
        // A lowercase drive letter (a file URI) is the same checkout.
        assert_eq!(root.normalize("c:/r/app/src/a.rs"), "src/a.rs");
        assert_eq!(Anchor::default().normalize("/abs/a.rs"), "/abs/a.rs");
        // A cwd with no checkout anywhere above it anchors on itself.
        let none = Anchor::for_cwd(Some("/no/such/dir/proj/"));
        assert_eq!(none.normalize("/no/such/dir/proj/lib/a.rs"), "lib/a.rs");
        assert_eq!(none.normalize("lib/a.rs"), "lib/a.rs");
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "calli_fe_{tag}_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    fn temp_db(tag: &str) -> Connection {
        let p = scratch(tag).join("index.db");
        crate::db::open(&p).unwrap()
    }

    #[test]
    fn verbs_match_whatever_the_case_or_version() {
        // Cursor's agent tools, as its transcripts store them.
        assert_eq!(
            ops(
                "StrReplace",
                r#"StrReplace: {"path":"/r/a.tsx","old_string":"x","new_string":"y"}"#
            ),
            v(&[("/r/a.tsx", "edit")])
        );
        assert_eq!(
            ops("Delete", r#"Delete: {"path":"/r/old.ts"}"#),
            v(&[("/r/old.ts", "edit")])
        );
        assert_eq!(
            ops(
                "Write",
                r#"Write: {"path":"/r/new.ts","contents":"export {}"}"#
            ),
            v(&[("/r/new.ts", "write")])
        );
        assert_eq!(
            ops(
                "EditNotebook",
                r#"EditNotebook: {"target_notebook":"x","path":"n.ipynb"}"#
            ),
            v(&[("n.ipynb", "edit")])
        );
        assert_eq!(
            ops("edit_file_v2", r#"edit_file_v2: {"path":"src/a.rs"}"#),
            v(&[("src/a.rs", "edit")])
        );
        assert_eq!(
            ops(
                "mcp__fs__Read-File",
                r#"mcp__fs__Read-File: {"path":"src/a.rs"}"#
            ),
            v(&[("src/a.rs", "read")])
        );
    }

    #[test]
    fn a_patch_keeps_every_file_it_touches() {
        let mut patch = String::from("*** Begin Patch\n");
        for i in 0..30 {
            patch.push_str(&format!("*** Update File: src/m{i}.rs\n@@\n-a\n+b\n"));
        }
        patch.push_str("*** End Patch");
        let got = ops("apply_patch", &format!("apply_patch: {patch}"));
        assert_eq!(got.len(), 30);
        assert!(got.iter().all(|(_, op)| *op == "edit"));
    }

    #[test]
    fn codex_readers_and_heredoc_lookalikes() {
        let sh = |cmd: &str| {
            ops(
                "shell",
                &format!(
                    "shell: {}",
                    serde_json::json!({"command": ["bash", "-lc", cmd]})
                ),
            )
        };
        assert_eq!(
            sh("sed -n '1,200p' src/lib.rs"),
            v(&[("src/lib.rs", "read")])
        );
        assert_eq!(
            sh("rg -n 'fn main' src/main.rs"),
            v(&[("src/main.rs", "read")])
        );
        assert_eq!(sh("grep -c TODO docs/a.md"), v(&[("docs/a.md", "read")]));
        assert_eq!(
            sh("awk '{print $1}' data/x.csv"),
            v(&[("data/x.csv", "read")])
        );
        // A here-string and an arithmetic shift open no heredoc: the rest of the command
        // still counts.
        assert_eq!(
            sh("cat <<< \"$x\"; sed -i s/a/b/ src/a.rs"),
            v(&[("src/a.rs", "edit")])
        );
        assert_eq!(
            sh("echo $((1<<3))\ncat src/b.rs"),
            v(&[("src/b.rs", "read")])
        );
        // A real heredoc still hides its body.
        assert_eq!(
            sh("cat > notes/x.md <<-\"END\"\nsee src/c.rs\nEND\nhead src/d.rs"),
            v(&[("notes/x.md", "write"), ("src/d.rs", "read")])
        );
    }

    #[test]
    fn checkout_roots_stop_at_home_and_at_a_missing_directory() {
        let home = scratch("home");
        std::fs::create_dir_all(home.join(".git")).unwrap(); // a dotfiles repo in ~
        let proj = home.join("scratch/proj");
        std::fs::create_dir_all(&proj).unwrap();
        let s = |p: &std::path::Path| p.to_string_lossy().into_owned();
        assert_eq!(
            checkout_root_with_home(&s(&proj), Some(&home)),
            s(&proj),
            "a project under ~ is not rooted at ~'s dotfiles repo"
        );
        assert_eq!(checkout_root_with_home(&s(&home), Some(&home)), s(&home));

        // A worktree inside the repo, removed after its merge: it anchors on itself, as it
        // did while it existed (its own `.git` file made it the root).
        let repo = home.join("code/app");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let wt = repo.join(".claude/worktrees/x");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: ../../../.git/worktrees/x\n").unwrap();
        let live = Anchor::for_cwd(Some(&s(&wt)));
        assert_eq!(live.normalize(&s(&wt.join("src/a.rs"))), "src/a.rs");
        std::fs::remove_dir_all(&wt).unwrap();
        let gone = Anchor::for_cwd(Some(&s(&wt)));
        assert_eq!(gone.normalize(&s(&wt.join("src/a.rs"))), "src/a.rs");
        assert_eq!(gone.normalize("src/a.rs"), "src/a.rs");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_call_resolves_against_its_own_working_directory() {
        let repo = scratch("cwd");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("apps/web")).unwrap();
        let root = repo.to_string_lossy().into_owned();
        let web = repo.join("apps/web").to_string_lossy().into_owned();
        let codex = format!(
            "shell: {}",
            serde_json::json!({"command": ["bash", "-lc", "cat src/page.tsx"], "workdir": web})
        );
        let claude = r#"Bash: {"command":"cat src/x.ts"}"#;
        let rows = [
            ToolRow {
                id: Some(1),
                tool_name: Some("shell"),
                text: &codex,
                ts: Some(1),
                cwd: None,
            },
            // Claude Code records the cwd per line: this call ran in apps/web too.
            ToolRow {
                id: Some(2),
                tool_name: Some("Bash"),
                text: claude,
                ts: Some(2),
                cwd: Some(&web),
            },
            ToolRow {
                id: Some(3),
                tool_name: Some("Bash"),
                text: claude,
                ts: Some(3),
                cwd: None,
            },
        ];
        let cwds = Anchors::cwds_of(&rows);
        assert_eq!(cwds, vec![web.clone()]);
        let anchors = Anchors::resolve(Some(&root), cwds.iter().map(String::as_str));
        let planned: Vec<(String, Op)> = plan(&anchors, &rows, &mut HashSet::new())
            .into_iter()
            .map(|r| (r.path, r.op))
            .collect();
        assert_eq!(
            planned,
            vec![
                ("apps/web/src/page.tsx".to_string(), Op::Read),
                ("apps/web/src/x.ts".to_string(), Op::Read),
                ("src/x.ts".to_string(), Op::Read),
            ]
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn the_cap_keeps_edits_over_reads_and_the_newest_first() {
        let conn = temp_db("cap");
        conn.execute(
            "INSERT INTO threads (source_id, external_id, title) VALUES (1, 'cap', 't')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        let reads: Vec<String> = (0..MAX_ROWS_PER_THREAD + 100)
            .map(|i| format!(r#"Read: {{"file_path":"src/r{i}.rs"}}"#))
            .collect();
        let edit = r#"Edit: {"file_path":"src/late.rs"}"#;
        let mut rows: Vec<ToolRow> = reads
            .iter()
            .enumerate()
            .map(|(i, t)| ToolRow {
                id: Some(i as i64),
                tool_name: Some("Read"),
                text: t,
                ts: Some(i as i64),
                cwd: None,
            })
            .collect();
        rows.push(ToolRow {
            id: Some(99_999),
            tool_name: Some("Edit"),
            text: edit,
            ts: Some(99_999),
            cwd: None,
        });
        let a = Anchors::default();
        assert_eq!(
            derive(&conn, tid, &a, &rows, false).unwrap(),
            MAX_ROWS_PER_THREAD
        );
        let count = |sql: &str| -> i64 { conn.query_row(sql, [tid], |r| r.get(0)).unwrap() };
        assert_eq!(
            count("SELECT COUNT(*) FROM file_edits WHERE thread_id = ?1 AND op = 'edit'"),
            1,
            "the late edit survives the cap"
        );
        assert_eq!(
            count("SELECT MIN(ts) FROM file_edits WHERE thread_id = ?1 AND op = 'read'"),
            101,
            "the oldest reads went"
        );
        // An append past the cap trims the oldest reads, not the new edit.
        let write = r#"Write: {"file_path":"src/new.rs","content":"x"}"#;
        let tail = [ToolRow {
            id: Some(100_000),
            tool_name: Some("Write"),
            text: write,
            ts: Some(100_000),
            cwd: None,
        }];
        assert_eq!(derive(&conn, tid, &a, &tail, true).unwrap(), 1);
        assert_eq!(
            count("SELECT COUNT(*) FROM file_edits WHERE thread_id = ?1"),
            MAX_ROWS_PER_THREAD as i64
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM file_edits WHERE thread_id = ?1 AND op <> 'read'"),
            2
        );
        assert_eq!(
            count("SELECT MIN(ts) FROM file_edits WHERE thread_id = ?1 AND op = 'read'"),
            102
        );
    }

    #[test]
    fn the_backfill_fills_old_threads_once_from_their_stored_messages() {
        use crate::indexer::{source_id, upsert_thread, ParsedMessage, ParsedThread};
        let dir = scratch("backfill");
        let mut conn = crate::db::open(&dir.join("index.db")).unwrap();
        let call = |name: &str, input: &str, ts: i64| ParsedMessage {
            role: "assistant".into(),
            text: format!("{name}: {input}"),
            tool_name: Some(name.into()),
            ts: Some(ts),
        };
        let claude = source_id(&conn, "claude_code").unwrap();
        let old = ParsedThread {
            external_id: "old".into(),
            project_path: Some("/no/such/app".into()),
            messages: vec![
                call("Read", r#"{"file_path":"/no/such/app/src/a.rs"}"#, 10),
                call("Edit", r#"{"file_path":"/no/such/app/src/a.rs"}"#, 11),
            ],
            ..Default::default()
        };
        upsert_thread(&mut conn, claude, &old).unwrap();
        let fresh = ParsedThread {
            external_id: "fresh".into(),
            messages: vec![call("Read", r#"{"file_path":"x/y.rs"}"#, 12)],
            ..Default::default()
        };
        upsert_thread(&mut conn, claude, &fresh).unwrap();

        // A Codex session indexed before 0034: its rollout's apply_patch was never stored.
        let codex = source_id(&conn, crate::indexer::codex::KIND).unwrap();
        let session = "019dc6b0-998d-7a31-a39d-1229e6b38072";
        let rollout = dir.join(format!("rollout-2026-04-25T15-09-15-{session}.jsonl"));
        std::fs::write(
            &rollout,
            concat!(
                r#"{"timestamp":"2026-04-25T22:09:39.000Z","type":"session_meta","payload":{"id":"019dc6b0-998d-7a31-a39d-1229e6b38072","cwd":"/no/such/codex"}}"#,
                "\n",
                r#"{"timestamp":"2026-04-25T22:09:41.000Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"command\":[\"bash\",\"-lc\",\"cat src/audit.rs\"]}"}}"#,
                "\n",
                r#"{"timestamp":"2026-04-25T22:09:42.000Z","type":"response_item","payload":{"type":"custom_tool_call","status":"completed","call_id":"c1","name":"apply_patch","input":"*** Begin Patch\n*** Update File: src/audit.rs\n@@\n-a\n+b\n*** End Patch\n"}}"#,
                "\n"
            ),
        )
        .unwrap();
        let codex_thread = ParsedThread {
            external_id: session.into(),
            project_path: Some("/no/such/codex".into()),
            messages: vec![call(
                "shell",
                r#"{"command":["bash","-lc","cat src/audit.rs"]}"#,
                1_777_154_981,
            )],
            ..Default::default()
        };
        upsert_thread(&mut conn, codex, &codex_thread).unwrap();
        crate::indexer::set_file_state(&conn, &rollout.to_string_lossy(), "codex", 1, 1).unwrap();

        // Back to an index from before 0034: no rows for the old threads, the flag set.
        conn.execute(
            "DELETE FROM file_edits WHERE thread_id IN
                (SELECT id FROM threads WHERE external_id IN ('old', ?1))",
            [session],
        )
        .unwrap();
        let fresh_rows = |conn: &Connection| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM file_edits f JOIN threads t ON t.id = f.thread_id
                 WHERE t.external_id = 'fresh'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(fresh_rows(&conn), 1);
        crate::knowledge::app_config::put_flag(&conn, BACKFILL_PENDING, true).unwrap();
        let db = crate::db::Db(std::sync::Mutex::new(conn));

        let report = backfill_file_edits_once(&db, &|| false).unwrap().unwrap();
        assert_eq!(report.threads, 2);
        assert_eq!(report.filled, 2);
        let conn = db.0.lock().unwrap();
        let rows = |ext: &str| -> Vec<(Option<i64>, String, String)> {
            conn.prepare(
                "SELECT f.message_id, f.path, f.op FROM file_edits f
                 JOIN threads t ON t.id = f.thread_id WHERE t.external_id = ?1 ORDER BY f.rowid",
            )
            .unwrap()
            .query_map([ext], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
        };
        let old_rows = rows("old");
        assert_eq!(
            old_rows
                .iter()
                .map(|(m, p, o)| (m.is_some(), p.as_str(), o.as_str()))
                .collect::<Vec<_>>(),
            vec![(true, "src/a.rs", "read"), (true, "src/a.rs", "edit")]
        );
        assert_eq!(
            rows(session)
                .iter()
                .map(|(m, p, o)| (m.is_some(), p.as_str(), o.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (true, "src/audit.rs", "read"),
                (false, "src/audit.rs", "edit")
            ],
            "the rollout's patch lands with no message"
        );
        assert_eq!(fresh_rows(&conn), 1, "a thread with rows is left alone");
        assert!(!crate::knowledge::app_config::get_flag(&conn, BACKFILL_PENDING).unwrap());
        drop(conn);
        assert_eq!(backfill_file_edits_once(&db, &|| false).unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_upgrade_flags_the_backfill_only_for_an_index_with_history() {
        let dir = scratch("upgrade");
        let fresh = crate::db::open(&dir.join("fresh.db")).unwrap();
        assert!(!crate::knowledge::app_config::get_flag(&fresh, BACKFILL_PENDING).unwrap());
        let mut old = Connection::open(dir.join("old.db")).unwrap();
        old.pragma_update(None, "foreign_keys", "ON").unwrap();
        let migrations = &*crate::db::migrations::MIGRATIONS;
        migrations.to_version(&mut old, 33).unwrap();
        old.execute(
            "INSERT INTO threads (source_id, external_id, title) VALUES (1, 'x', 'x')",
            [],
        )
        .unwrap();
        migrations.to_latest(&mut old).unwrap();
        assert!(crate::knowledge::app_config::get_flag(&old, BACKFILL_PENDING).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Measures the one-time backfill on a copy of a real index:
    /// `CALLIMACHUS_BACKFILL_DB=<copy>/index.db cargo test -- --ignored backfill_on_a_real_index --nocapture`.
    /// Its own variable, not `CALLIMACHUS_DB`: opening migrates the index, and the real one
    /// must never be upgraded past what the installed app can open.
    #[test]
    #[ignore]
    fn backfill_on_a_real_index() {
        let Some(path) = std::env::var_os("CALLIMACHUS_BACKFILL_DB") else {
            return;
        };
        let path = std::path::PathBuf::from(path);
        assert!(
            !path.to_string_lossy().contains("dev.shaller.callimachus"),
            "a copy, never the app's own index"
        );
        let t = std::time::Instant::now();
        let conn = crate::db::open(&path).unwrap();
        eprintln!("open + migrate: {:.2}s", t.elapsed().as_secs_f64());
        let db = crate::db::Db(std::sync::Mutex::new(conn));
        let t = std::time::Instant::now();
        let report = backfill_file_edits_once(&db, &|| false).unwrap();
        eprintln!("backfill: {report:?} in {:.1}s", t.elapsed().as_secs_f64());
        let conn = db.0.lock().unwrap();
        let by_op: Vec<(String, i64)> = conn
            .prepare("SELECT op, COUNT(*) FROM file_edits GROUP BY op ORDER BY op")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        eprintln!("rows by op: {by_op:?}");
    }
}
