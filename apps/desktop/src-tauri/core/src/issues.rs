//! Recurring-issue mining: find errors you keep hitting across every indexed agent session.
//! Two stages: an FTS candidate fetch (messages mentioning error-ish tokens) keeps the scan
//! cheap, then a precise per-line extractor pulls real error signatures and a normalizer
//! collapses the variable parts (paths, line:col, quoted identifiers, hashes) so the SAME error
//! recurring across runs groups together. Only Callimachus sees this across all your tools.

use crate::search::ProjectMatch;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, ToSql};
use serde::{Deserialize, Serialize};
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};

/// One recurring error, grouped by its normalized signature.
#[derive(Debug, Clone, Default, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct IssueCluster {
    /// A representative raw error line (the latest raw sighting: candidates are read newest
    /// first and the first line seen for a signature is kept).
    pub example: String,
    /// How many messages this error appeared in (≈ times you hit it).
    pub count: i64,
    /// Distinct threads it spans.
    pub threads: i64,
    pub first_seen: i64,
    pub last_seen: i64,
    /// The thread of the latest sighting, for the Today view's project label. Not part of
    /// the JSON that `cal issues`, MCP and the app's lists return.
    #[serde(skip_serializing)]
    pub last_thread_id: i64,
    /// The sessions that hit it, newest sighting first, at most [`THREAD_IDS_CAP`]. Not part
    /// of this shape's JSON either, which `cal issues --json` prints unversioned; [`Issue`]
    /// carries them.
    #[serde(skip)]
    #[schemars(skip)]
    pub thread_ids: Vec<i64>,
}

/// Session ids kept per cluster ([`IssueCluster::thread_ids`]).
pub const THREAD_IDS_CAP: usize = 10;

/// The commit that fixed a recurring error in one project: the latest session there that hit
/// it, then edited the file its error named and committed a change to that file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct IssueFix {
    pub sha: String,
    pub short_sha: String,
    /// The commit's subject line.
    pub subject: Option<String>,
    pub committed_at: i64,
    /// The session that hit the error and made the fix.
    pub thread_id: i64,
}

/// A recurring error with its sessions and, once a commit fixed it, the fix and the playbook
/// line: what `cal issues --json-version 1`, MCP `recurring_issues` and the app return.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Issue {
    #[serde(flatten)]
    pub cluster: IssueCluster,
    /// The sessions that hit it, newest sighting first (at most 10).
    pub thread_ids: Vec<i64>,
    /// The commit that fixed it, null while none is known. Scoped to a project, the newest fix
    /// made there (a fix in another project is left out); unscoped, the fix in the project of
    /// the latest sighting, else the newest in any project.
    pub fix: Option<IssueFix>,
    /// The playbook gotcha recorded for that fix in its project (`extractor = 'playbook'`),
    /// null when there is none or the user deleted it.
    pub playbook: Option<String>,
}

/// Pull distinct error signatures out of one message's text. High-precision: a line counts only
/// if it carries a strong error marker near its start (not just the word "error" in prose).
pub fn extract_errors(text: &str) -> Vec<String> {
    extract_errors_with_files(text)
        .into_iter()
        .map(|(line, _)| line)
        .collect()
}

/// Most files kept per error line (see [`extract_errors_with_files`]).
const FILES_PER_ERROR: usize = 4;

/// [`extract_errors`], each with the files it names: path tokens on the error line itself and
/// on the lines around it, where compilers and runtimes put the location (rustc's
/// `--> src/a.rs:3:5` on the next line, Python's `File "a.py", line 3` on the ones before, a
/// JS stack's `at f (a.js:1:2)` after). `:line:col` suffixes are stripped. The playbook
/// resolver matches these against the files a session edited.
///
/// Three kinds of line that carry an error marker are not errors: file content read with a
/// line-number gutter ([`behind_gutter`]), a source line that only names an error variable
/// ([`reads_as_code`]), and a Python traceback's header and frames. A traceback yields its
/// exception line instead (`KeyError: 'id'`), filed under its frames' files, the session's
/// own code innermost first ([`traceback_files`]): the header alone was the top recurring
/// error on a large index (403 sightings), one cluster that merged every Python error. A
/// pytest failure counts once: its short-summary line (`FAILED <node> - <message>`) shares
/// the `E` line's signature and adds the node's test file to it. It runs inside the
/// `recall-error` hook after every shell command, so it stays a single pass with no
/// allocation per plain line.
pub fn extract_errors_with_files(text: &str) -> Vec<(String, Vec<String>)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    // Each signature taken so far, with its place in `out`.
    let mut seen: HashMap<String, usize> = HashMap::new();
    // Inside a traceback: the header's indent, and its frames' files so far, outermost first.
    let mut traceback: Option<(usize, Vec<&str>)> = None;
    for (i, raw) in lines.iter().enumerate() {
        if behind_gutter(raw) {
            continue;
        }
        let line = raw.trim();
        let indent = raw.len() - raw.trim_start().len();
        // The exception line that closes a traceback counts even without a marker
        // (`http.client.InvalidURL: ...`), and is filed under the block's frames.
        let mut closes_traceback: Option<Vec<&str>> = None;
        if let Some((header_indent, frames)) = traceback.as_mut() {
            // Frames (`File "a.py", line 3, in f`), their source lines and 3.11's carets sit
            // deeper than the header; the first line that does not ends the block.
            if line.is_empty() || indent > *header_indent {
                if let Some(path) = python_frame_file(line) {
                    // Bounded: a recursion error prints a thousand frames.
                    if frames.len() == TRACEBACK_FRAMES {
                        frames.remove(0);
                    }
                    frames.push(path);
                }
                continue;
            }
            let frames = std::mem::take(frames);
            traceback = None;
            if is_python_exception(line) {
                closes_traceback = Some(frames);
            }
        }
        if starts_with_ci(line, TRACEBACK_HEADER) {
            traceback = Some((indent, Vec::new()));
            continue;
        }
        if line.len() < 6 || line.len() > 400 {
            continue;
        }
        if !(closes_traceback.is_some() || is_error_line(line)) || reads_as_code(line) {
            continue;
        }
        let key = normalize(line);
        // pytest's short summary repeats the `E` line of the failure above it ([`normalize`]
        // reads both as the message): it adds only its test file, which that line's
        // neighbours do not name. A summary cut to the terminal width ends in `...` and
        // repeats the one whole error it starts. When two or more start with it (two
        // `ValueError: invalid ...`), it repeats one of them, which one is unknown: it adds
        // nothing. When none does (`--tb=no` prints only summaries, themselves cut), it is an
        // error of its own: matching a cut key merged two different failures.
        let node_file = pytest_node_file(line);
        let cut = key.strip_suffix("...").filter(|_| node_file.is_some());
        let earlier = match (seen.get(&key), cut) {
            (Some(&at), _) => Some(at),
            (None, Some(cut)) => {
                let mut whole = seen
                    .iter()
                    .filter(|(k, _)| k.starts_with(cut) && !k.ends_with("..."))
                    .map(|(_, &at)| at);
                match (whole.next(), whole.next()) {
                    (Some(_), Some(_)) => continue,
                    (at, _) => at,
                }
            }
            (None, None) => None,
        };
        if let Some(at) = earlier {
            let files = &mut out[at].1;
            if let Some(f) = node_file.filter(|f| !files.contains(f)) {
                if files.len() < FILES_PER_ERROR {
                    files.push(f);
                }
            }
            continue;
        }
        seen.insert(key, out.len());
        let mut files: Vec<String> = match &closes_traceback {
            Some(frames) => traceback_files(frames),
            None => Vec::new(),
        };
        files.extend(node_file);
        let around = lines[i.saturating_sub(2)..(i + 4).min(lines.len())].iter();
        for tok in around.flat_map(|l| l.split_whitespace()) {
            if files.len() >= FILES_PER_ERROR {
                break;
            }
            if let Some(f) = file_token(tok) {
                if !files.contains(&f) {
                    files.push(f);
                }
            }
        }
        out.push((line.to_string(), files));
    }
    out
}

/// Python's traceback header, lowercase.
const TRACEBACK_HEADER: &str = "traceback (most recent call last)";

/// Whether `raw` is a line of file content behind a line-number gutter: Claude Code's Read
/// tool and `cat -n` print `   12\t...` (older Read output `   12→...`). Those are a file's
/// lines, not an error that happened: on a large index the source line
/// `error: error instanceof Error ? error.message : String(error),` read from files was a
/// recurring "error" with 43 sightings.
fn behind_gutter(raw: &str) -> bool {
    let rest = raw.trim_start_matches(' ');
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    (1..=7).contains(&digits)
        && (rest[digits..].starts_with('\t') || rest[digits..].starts_with('\u{2192}'))
}

/// Whether an error-marked line reads as source code rather than as a message: outside
/// quotes it has an arrow function (`onError: (error) => toast.error(...)`), an `instanceof`
/// test or a `? :` conditional, or it ends by opening a block or continuing a list
/// (`error: {`, `} catch (error) {`, `error: { code: "x", message: "y" },`). Diffs, grep
/// hits and pasted code carry these past the Read gutter. Quoted text is skipped, so a
/// TypeScript message about a type `'(e: Event) => void'` or rustc's `` expected `=>` ``
/// stays an error, and so does a message that ends by opening the payload it prints
/// (`MCP error -32602: Invalid arguments for tool lookup: [`, 9 sightings on a large index
/// that the opener alone dropped): its text before the last colon has a `: ` of its own and
/// no `{`, where a key opening a value (`error: {`, `"errors": [`, `return { error: {`) has
/// none and a nested key (`json({ error: e.message, details: {`) sits in an open object.
/// Node's print of an error object without its stack (`[Error: EACCES: permission denied,
/// open '/etc/x'] {`, then its `errno` and `code` lines) stays an error too, also behind
/// the REPL's `Uncaught ` or as the value of a logged object's key (`error: [Error: ...] {`).
/// An `echo` or `printf` command ([`is_print_command`]) is what a script or workflow file
/// read without a gutter runs, not what it prints.
fn reads_as_code(line: &str) -> bool {
    if is_print_command(line) {
        return true;
    }
    if let Some(rest) = line.strip_suffix(['{', '[']) {
        let rest = rest.trim_end();
        let printed = rest
            .strip_prefix("Uncaught ")
            .or_else(|| {
                let (key, value) = rest.split_once(": ")?;
                let is_key = !key.is_empty()
                    && key.bytes().all(|c| {
                        c.is_ascii_alphanumeric() || matches!(c, b'_' | b'$' | b'\'' | b'"' | b'-')
                    });
                is_key.then_some(value)
            })
            .unwrap_or(rest);
        let node_error =
            printed.starts_with('[') && printed.ends_with(']') && printed.contains(": ");
        match rest.strip_suffix(':') {
            Some(message) if message.contains(": ") && !message.contains('{') => {}
            _ if node_error => {}
            _ => return true,
        }
    } else if line.ends_with(['(', ',', ';']) {
        return true;
    }
    let b = line.as_bytes();
    let mut quote: Option<u8> = None;
    let mut question = false;
    for (i, &c) in b.iter().enumerate() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            b'"' | b'`' => quote = Some(c),
            // An apostrophe opens a quote only where a quote can start, so the one in
            // "can't" does not hide the rest of the line.
            b'\'' if i == 0 || !b[i - 1].is_ascii_alphanumeric() => quote = Some(c),
            b'=' if b.get(i + 1) == Some(&b'>') => return true,
            b'?' if i > 0 && b[i - 1] == b' ' && b.get(i + 1) == Some(&b' ') => question = true,
            b':' if question && b[i - 1] == b' ' && b.get(i + 1) == Some(&b' ') => return true,
            b' ' if b[i..].starts_with(b" instanceof ") => return true,
            _ => {}
        }
    }
    false
}

/// Whether `line` runs `echo` or `printf`: bare, or as a diff's added or removed line
/// (`+    echo "Error: ..." >&2`), a `set -x` trace (`+ echo ...`), a prompt (`$ echo ...`)
/// or a workflow step (`run: echo ...`, `- run: echo ...`). Each of these forms came out
/// as an error when only a bare `echo ` at the start was a command. `echo: write error:
/// Broken pipe` is the shell's own error.
fn is_print_command(line: &str) -> bool {
    let rest = line.trim_start_matches(['+', '-', ' ', '\t']);
    let rest = rest.strip_prefix("$ ").unwrap_or(rest).trim_start();
    let rest = rest.strip_prefix("run: ").unwrap_or(rest).trim_start();
    rest.starts_with("echo ") || rest.starts_with("printf ")
}

/// The file of a traceback frame line (`File "/app/a.py", line 3, in f`), as written.
fn python_frame_file(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("File \"")?;
    let end = rest.find('"')?;
    Some(&rest[..end])
}

/// Most frame files a traceback keeps while it is read (the innermost ones).
const TRACEBACK_FRAMES: usize = 32;

/// The files a traceback's exception line is filed under: the frames in the session's own
/// code, innermost first, else the innermost frame. An exception raised inside a library
/// (`json.loads` failing in `json/decoder.py`) has its innermost frame there, a file no
/// session edits, so filing it there alone left the resolver nothing to match.
fn traceback_files(frames: &[&str]) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    let own = frames.iter().rev().filter(|f| !is_library_frame(f));
    for f in own.filter_map(|f| file_token(f)) {
        if files.len() == FILES_PER_ERROR {
            break;
        }
        if !files.contains(&f) {
            files.push(f);
        }
    }
    if files.is_empty() {
        files.extend(frames.last().and_then(|f| file_token(f)));
    }
    files
}

/// Whether a traceback frame is the interpreter's or an installed package's: the standard
/// library (`/usr/lib/python3.12/json/decoder.py`, a venv's `lib/python3.12/`, Windows'
/// `\Lib\`), `site-packages` / `dist-packages`, or a frozen or generated module
/// (`<frozen runpy>`, `<string>`).
fn is_library_frame(path: &str) -> bool {
    path.starts_with('<')
        || [
            "site-packages",
            "dist-packages",
            "/lib/python",
            "\\lib\\python",
            "\\Lib\\",
        ]
        .iter()
        .any(|p| path.contains(p))
}

/// Builtin exceptions whose names end in none of `Error`, `Exception` or `Warning`.
const PYTHON_OTHER_EXCEPTIONS: &[&str] = &[
    "BaseExceptionGroup",
    "ExceptionGroup",
    "GeneratorExit",
    "KeyboardInterrupt",
    "StopAsyncIteration",
    "StopIteration",
    "SystemExit",
];

/// Whether `line` is the exception line that closes a traceback: an exception's name, then
/// `: ` and the message. The name ends in `Error`, `Exception` or `Warning` (`KeyError`,
/// `json.decoder.JSONDecodeError`, `UserWarning`; bare `Warning` is a console prefix), is
/// another builtin ([`PYTHON_OTHER_EXCEPTIONS`]), is a module's `error` (`re.error`), or is
/// a class in a lowercase module (`jinja2.exceptions.TemplateNotFound`,
/// `subprocess.TimeoutExpired`). Any capitalized word was taken before, and a traceback cut
/// off before its exception line (the head and tail of a long log are joined) then made
/// the next line an error: a server's `INFO:     Application shutdown complete.` or
/// `Status: 404`. A bare name (`AssertionError`) says nothing more than the header did and
/// is not taken.
fn is_python_exception(line: &str) -> bool {
    let Some((name, message)) = line.split_once(": ") else {
        return false;
    };
    if message.trim().is_empty()
        || name.len() > 120
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'.')
    {
        return false;
    }
    let (module, last) = match name.rsplit_once('.') {
        Some((module, last)) => (Some(module), last),
        None => (None, name),
    };
    let in_module = module.is_some_and(|m| {
        m.split('.')
            .all(|seg| seg.starts_with(|c: char| c.is_ascii_lowercase() || c == '_'))
    });
    let class = last.starts_with(|c: char| c.is_ascii_uppercase())
        && last.bytes().any(|c| c.is_ascii_lowercase());
    (last.ends_with("Error") || last.ends_with("Exception"))
        || (last.ends_with("Warning") && last != "Warning")
        || PYTHON_OTHER_EXCEPTIONS.contains(&last)
        || (in_module && (last == "error" || class))
}

/// Whether `hay` starts with `needle` (ASCII lowercase), ignoring ASCII case.
fn starts_with_ci(hay: &str, needle: &str) -> bool {
    hay.len() >= needle.len()
        && hay.as_bytes()[..needle.len()].eq_ignore_ascii_case(needle.as_bytes())
}

/// A token that names a source file, as written minus quotes, brackets and a `:line:col`
/// suffix: the last path segment has a short extension with a letter in it (`a.rs`,
/// `Cargo.toml`), and it is no URL. Prose like `e.g.` or a version `1.2.3` is not a file.
fn file_token(tok: &str) -> Option<String> {
    let t = tok.trim_matches(|c: char| {
        matches!(
            c,
            '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | '<' | '>'
        )
    });
    if t.len() < 3 || t.len() > 300 || t.contains("://") {
        return None;
    }
    let mut t = t.trim_end_matches(':');
    // `a.rs:12:5`, `a.rs:12`: the location, not part of the name.
    for _ in 0..2 {
        if let Some((head, tail)) = t.rsplit_once(':') {
            if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) {
                t = head;
            }
        }
    }
    // tsc's plain (non-TTY) location, `src/a.ts(3,5):`, which is what an agent's shell gets:
    // read as a name it made the extension `ts(3,5)` and TypeScript errors named no file.
    // The closing paren may already be trimmed above when nothing followed it.
    if let Some((head, tail)) = t.rsplit_once('(') {
        let at = tail.strip_suffix(')').unwrap_or(tail);
        if !at.is_empty() && at.bytes().all(|b| b.is_ascii_digit() || b == b',') {
            t = head;
        }
    }
    let name = t.rsplit(['/', '\\']).next()?;
    let (stem, ext) = name.rsplit_once('.')?;
    let ext_ok = (1..=8).contains(&ext.len())
        && ext.bytes().all(|b| b.is_ascii_alphanumeric())
        && ext.bytes().any(|b| b.is_ascii_alphabetic());
    let stem_ok = !stem.is_empty()
        && stem
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'+' | b'@'));
    // A duration (`0.03s` in pytest's closing `1 failed in 0.03s`), which filed a pytest
    // failure under the run's timing.
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let duration = digits(stem) && ext.strip_suffix('s').is_some_and(digits);
    (ext_ok && stem_ok && !duration && !t.contains(' ')).then(|| t.to_string())
}

/// Marker check. Looks at the first ~60 chars so a stray "error:" deep in a prose paragraph
/// doesn't match, while real errors (which lead the line) do. A Python traceback's header is
/// no marker ([`extract_errors_with_files`] takes the block's exception line instead).
fn is_error_line(line: &str) -> bool {
    // The first 64 chars, lowercased in a stack buffer: this runs on every line of every
    // candidate message and of every shell command's output in the `recall-error` hook,
    // where two Strings per line were most of its allocations. Char-safe: a byte slice
    // could land inside a multi-byte char (e.g. box-drawing `━` from cargo output) and panic.
    let head = match line.char_indices().nth(64) {
        Some((at, _)) => &line[..at],
        None => line,
    };
    let mut buf = [0u8; 256];
    let buf = &mut buf[..head.len()];
    buf.copy_from_slice(head.as_bytes());
    buf.make_ascii_lowercase();
    // ASCII lowercasing keeps it UTF-8.
    let Ok(head) = std::str::from_utf8(buf) else {
        return false;
    };
    head.starts_with("error[")
        || head.starts_with("error ")
        || head.starts_with("fatal:")
        || head.starts_with("fatal error")
        || head.starts_with("panic:")
        || has_marker(head, line, "error:") // error:, TypeError:/ValueError:/RuntimeError: ...
        || has_marker(head, line, "exception:")
        // tsc: `a.ts(3,5): error TS2322: ...` (plain) and `a.ts:3:5 - error TS2322: ...`
        // (pretty) carry no `error:`, and most of its messages no other marker either.
        || head.contains("): error ts")
        || head.contains(" - error ts")
        || head.contains("panicked at")
        || head.contains("command not found")
        || head.contains("undefined reference")
        || head.contains("cannot find")
        || head.contains("assertion failed")
        || head.contains("assertion `")
        || head.contains("no such file")
        || head.contains("segmentation fault")
        || pytest_failure(line)
}

/// Whether `head` holds `marker` (one ending in `:`) somewhere it is not directly followed by
/// another `:`. A Rust path (`EmbedError::Init`, `io::Error::new`) and a workflow script's
/// `echo "::error::..."` hold `error:` only as part of `error::`: on a large index such text
/// made a recurring "error" with a recorded fix. `foo::Error: bad input` is still one. The
/// next byte is read from `line`, of which `head` is a lowercased byte prefix: a path whose
/// `error:` ends the head (53 chars of prose, then `EmbedError::Init`) is no marker either.
fn has_marker(head: &str, line: &str, marker: &str) -> bool {
    head.match_indices(marker)
        .any(|(at, _)| line.as_bytes().get(at + marker.len()) != Some(&b':'))
}

/// Whether `line` is pytest's short summary of a failed test whose message is an error
/// (`FAILED tests/a.py::TestParseError::test_empty - ValueError: empty input`). The node id
/// comes first and a long one pushes the message past the head; a class named `...Error`
/// used to supply the marker by accident (`testparseerror::`), which [`has_marker`] no
/// longer takes. The verbose status line (`tests/a.py::TestParseError::test_empty FAILED`)
/// holds no message and is not taken.
fn pytest_failure(line: &str) -> bool {
    line.strip_prefix("FAILED ")
        .and_then(|rest| rest.split_once(" - "))
        .is_some_and(|(_, message)| is_error_line(message) || is_python_exception(message))
}

/// The test file of pytest's short-summary line (`FAILED tests/a.py::test_x - ...`): its
/// node id up to the first `::`. Only an error line reaches here, so a plain line allocates
/// nothing.
fn pytest_node_file(line: &str) -> Option<String> {
    let (node, _) = line.strip_prefix("FAILED ")?.split_once(" - ")?;
    file_token(node.split_once("::")?.0)
}

/// The error a pytest line reports: an exception line without pytest's `E` gutter
/// (`E       ValueError: empty input`), a short-summary line's message without its node id
/// (`FAILED tests/a.py::test_x - ValueError: empty input`), any other line as it is. One
/// failure printed both and mined two clusters.
fn pytest_message(line: &str) -> &str {
    let line = line.trim_start();
    if let Some(rest) = line.strip_prefix("E   ") {
        return rest.trim_start();
    }
    match line
        .strip_prefix("FAILED ")
        .and_then(|r| r.split_once(" - "))
    {
        Some((_, message)) => message,
        None => line,
    }
}

/// Collapse a raw error line to a recurrence key: lowercase, with paths, quoted identifiers,
/// line:col, and hashes/long numbers replaced by placeholders, so the same error with different
/// specifics groups together, and a pytest line read as its message ([`pytest_message`]).
/// Token-based so it stays dependency-free.
pub fn normalize(line: &str) -> String {
    pytest_message(line)
        .split_whitespace()
        .map(canon_token)
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn canon_token(tok: &str) -> String {
    let t = tok.trim_matches(|c: char| matches!(c, ',' | ';' | ')' | '(' | '[' | ']'));
    if t.is_empty() {
        return String::new();
    }
    // Path-like (has a separator) -> <path>; also catches file:line:col.
    if t.contains('/') || (t.contains('\\') && t.len() > 3) {
        return "<path>".into();
    }
    // Quoted identifier / string -> '…' (groups "named `foo`" with "named `bar`").
    if matches!(t.as_bytes()[0], b'`' | b'\'' | b'"') {
        return "'…'".into();
    }
    // Hash / hex / long number -> <n> (but keep short codes like an error number on its own).
    let core = t.strip_prefix("0x").unwrap_or(t);
    if core.len() >= 6 && core.chars().all(|c| c.is_ascii_hexdigit()) {
        return "<n>".into();
    }
    t.to_ascii_lowercase()
}

/// Chars of a long message's head / tail scanned for errors (see `recurring_issues`).
const TEXT_HEAD_CHARS: usize = 6_000;
const TEXT_TAIL_CHARS: usize = 2_000;

/// Cache key: (project filter, since bucketed to the hour, limit).
type IssuesKey = (Option<String>, i64, usize);

static ISSUES_CACHE: std::sync::LazyLock<
    crate::search::cache::AggCache<IssuesKey, Vec<IssueCluster>>,
> = std::sync::LazyLock::new(crate::search::cache::AggCache::new);

/// [`recurring_issues`], reused for up to 10 minutes while the thread set is unchanged
/// (see `search::cache`), with `project` an exact key ([`ProjectMatch::Exact`]). `since` is
/// bucketed to the hour so "now minus 180 days" callers share an entry. For the app's
/// Project page; MCP and `cal` read live, and so does Today (an error hit again in a session
/// that is still running leaves the thread set unchanged).
pub fn recurring_issues_cached(
    conn: &Connection,
    project: Option<&str>,
    since: i64,
    limit: usize,
) -> Result<Vec<IssueCluster>> {
    let since = since - since.rem_euclid(3600);
    let key = (
        project.filter(|p| !p.is_empty()).map(str::to_string),
        since,
        limit,
    );
    ISSUES_CACHE.get_or_try(crate::search::cache::index_version(conn)?, key, || {
        recurring_issues_matching(conn, project, ProjectMatch::Exact, since, limit)
    })
}

/// Which sessions' messages mining reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sightings {
    /// Top-level sessions' user and tool rows, plus subagents' tool rows filed under their
    /// parent session: on a large index 90% of error output lands in subagent transcripts.
    All,
    /// Top-level sessions only, for Today's live strip, which lists top-level sessions and
    /// must stay a fraction of a second: the subagent arm reads several times the text.
    TopLevel,
}

/// One candidate fetch of [`mine`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arm {
    /// Top-level sessions: tool output and the user's turns (a pasted log is an error the
    /// user hit too).
    TopLevel,
    /// Subagent transcripts, tool output only: their `user` turns are the orchestrator's
    /// prompts, full of quoted code and of errors quoted from other sessions.
    Subagents,
}

/// Most candidate rows each arm reads, newest first. On a 5.3 GB index the top-level arm
/// had 19k rows over 180 days, under its cap. Subagents had 120k, 37k of them over 8 KB, and
/// a long row is read whole before its head and tail are cut, so their cap is what bounds a
/// refresh (10 s at most): their newest 40k took 4.2 s to read in the sqlite3 shell, the
/// top-level arm 2.2 s. The newest 20k cover the last 25 days there, and mining both arms
/// took 8.2 s from a cold cache and 1.2 s warm in a release build (a project-scoped read
/// 5.8 s and 0.9 s), the resolve after it about a second.
const TOP_LEVEL_CANDIDATE_CAP: i64 = 40_000;
const SUBAGENT_CANDIDATE_CAP: i64 = 20_000;

/// The candidate fetch of one [`Arm`]: error-ish messages since `since`, as (text head +
/// tail, ts, thread_id, the thread's project key). The top-level arm comes newest first;
/// the subagent arm in id order (the newest [`SUBAGENT_CANDIDATE_CAP`] of them).
fn candidates_sql(
    conn: &Connection,
    project: Option<&str>,
    mode: ProjectMatch,
    since: i64,
    arm: Arm,
) -> (String, Vec<Box<dyn ToSql>>) {
    // Long tool output is read as its head and tail only: errors lead a command's output
    // or close it (the summary / final failure), and copying every multi-KB log body into
    // Rust is what made this view allocate 100+ MB. octet_length() reads only the record
    // header, so short rows skip the substr work entirely.
    let text = |m: &str| {
        format!(
            "CASE WHEN octet_length({m}.text) > {long}
                  THEN substr({m}.text, 1, {head}) || char(10) || substr({m}.text, -{tail})
                  ELSE {m}.text END",
            long = TEXT_HEAD_CHARS + TEXT_TAIL_CHARS,
            head = TEXT_HEAD_CHARS,
            tail = TEXT_TAIL_CHARS,
        )
    };
    // The role / ts / subagent / project filters run off the covering index that
    // fts_message_join names, so only surviving rows read the table: 385k page reads down
    // to 51k, ~15 s down to ~3 s from a cold cache on a large index.
    // Subagents of Claude Code only: theirs are the transcripts `subagent_parents` maps to a
    // parent session. OpenCode, Gemini CLI and Cursor mark subagents too, with no mapping,
    // so each of theirs would count as a session of its own.
    let (roles, sessions) = match arm {
        Arm::TopLevel => ("'user', 'tool'", "t.is_subagent = 0"),
        Arm::Subagents => (
            "'tool'",
            "t.is_subagent = 1
           AND t.source_id = (SELECT id FROM sources WHERE kind = 'claude_code')",
        ),
    };
    let mut filter = format!(
        "FROM messages_fts
         {join}
         JOIN threads t ON t.id = m.thread_id
         WHERE messages_fts MATCH ?1
           AND m.role IN ({roles})
           AND m.ts >= ?2
           AND {sessions}",
        join = crate::search::fts_message_join(conn),
    );
    let mut args: Vec<Box<dyn ToSql>> = vec![
        Box::new("error OR panicked OR exception OR traceback OR fatal".to_string()),
        Box::new(since),
    ];
    if let Some(p) = project.filter(|p| !p.is_empty()) {
        let cond = crate::search::project_condition("t", p, mode, &mut args);
        filter.push_str(&format!(" AND {cond}"));
    }
    let sql = match arm {
        Arm::TopLevel => {
            args.push(Box::new(TOP_LEVEL_CANDIDATE_CAP));
            format!(
                "SELECT {text}, m.ts, m.thread_id, COALESCE(t.project_key, t.project_path)
                 {filter} ORDER BY m.ts DESC LIMIT ?{n}",
                text = text("m"),
                n = args.len()
            )
        }
        // The newest ids first, off the covering index alone, then the text of those rows
        // only: sorting the rows with their text read every one of the 120k rows' text
        // (15 s in the sqlite3 shell on a large index for the newest 40k, 4.2 s this way),
        // and the outer query has no ORDER BY so the text is streamed, never sorted.
        Arm::Subagents => {
            args.push(Box::new(SUBAGENT_CANDIDATE_CAP));
            format!(
                "SELECT {text}, o.ts, o.thread_id, COALESCE(ot.project_key, ot.project_path)
                 FROM messages o JOIN threads ot ON ot.id = o.thread_id
                 WHERE o.id IN (SELECT m.id {filter} ORDER BY m.ts DESC LIMIT ?{n})",
                text = text("o"),
                n = args.len()
            )
        }
    };
    (sql, args)
}

/// Find recurring errors across messages since `since` (epoch secs), optionally scoped to a
/// project-path substring (`cal issues -p`, MCP). Returns clusters seen in >= 2 messages,
/// most-frequent first.
pub fn recurring_issues(
    conn: &Connection,
    project: Option<&str>,
    since: i64,
    limit: usize,
) -> Result<Vec<IssueCluster>> {
    recurring_issues_matching(conn, project, ProjectMatch::Substring, since, limit)
}

/// [`recurring_issues`] with the project matched as `mode` says. An unscoped call over the
/// standard window (`cal issues`, MCP with `project: "*"`, the app's all-projects list) reads
/// `issue_clusters` while the watcher keeps it fresh, instead of mining again.
pub fn recurring_issues_matching(
    conn: &Connection,
    project: Option<&str>,
    mode: ProjectMatch,
    since: i64,
    limit: usize,
) -> Result<Vec<IssueCluster>> {
    recurring_issues_from(conn, project, mode, since, limit, Sightings::All)
}

/// [`recurring_issues_matching`] over the sessions `sightings` names. Only
/// [`Sightings::All`] can be served from `issue_clusters`, which a refresh mines that way.
pub fn recurring_issues_from(
    conn: &Connection,
    project: Option<&str>,
    mode: ProjectMatch,
    since: i64,
    limit: usize,
    sightings: Sightings,
) -> Result<Vec<IssueCluster>> {
    if sightings == Sightings::All && project.is_none_or(str::is_empty) {
        let now = chrono::Utc::now().timestamp();
        if let Some(stored) = stored_clusters(conn, since, limit, now) {
            return Ok(stored);
        }
    }
    mine_clusters(conn, project, mode, since, limit, sightings)
}

/// The history `issue_clusters` covers: the 180 days `cal issues`, MCP and the Project page
/// mine.
pub const CLUSTER_WINDOW_SECS: i64 = 180 * 86_400;
/// Clusters kept in `issue_clusters`, most frequent first (the app lists 30, MCP up to 50,
/// Today 500).
pub const CLUSTER_TABLE_CAP: usize = 500;
/// How long a refresh answers [`recurring_issues`] while sessions keep landing; older, and
/// readers mine live again (unless nothing was indexed since, see [`CLUSTERS_IDLE_SECS`]).
pub const CLUSTERS_FRESH_SECS: i64 = 600;
/// How long a refresh stays good while nothing was indexed after it. The watcher refreshes
/// only after a batch wrote, so an idle machine kept a minutes-old table that every unscoped
/// `cal issues` and MCP call then ignored, mining live (0.7 s warm, about 4.7 s cold) though
/// nothing had changed. The only drift left is the window's far end moving on, and a day at
/// the end of 180 days does not move a cluster's rank; the watcher refreshes after a day.
pub const CLUSTERS_IDLE_SECS: i64 = 86_400;
/// app_config key: unix time of the last [`refresh_clusters`]. Stored apart from the rows so
/// a refresh that found no recurring errors is fresh too.
const CLUSTERS_REFRESHED_AT: &str = "issues.clusters_refreshed_at";
/// The extraction rules a refresh mined with. Bump it when a change to
/// [`extract_errors_with_files`], [`normalize`] or the mined rows moves signatures: the
/// stored rows then count as no refresh at all ([`refresh_state`]), so readers mine live
/// with the new rules and the next watcher tick or `cal index` re-mines, instead of serving
/// the old rows for up to a day ([`CLUSTERS_IDLE_SECS`]). 2: the read-gutter, code-line and
/// traceback rules, and subagent tool output.
const EXTRACTION_VERSION: u32 = 2;
/// app_config key: `<EXTRACTION_VERSION>@<refreshed_at>` of the last refresh this build's
/// rules wrote. Tied to that refresh's time, so a later refresh by an older build (a dev
/// build and the installed app share one index) leaves it behind and reads as stale. The
/// re-mine is an ordinary refresh, mined on a reader and stored in one short transaction:
/// one that yields to a writer job or dies with its process wrote nothing, and the next
/// tick starts it over.
const CLUSTERS_EXTRACTION: &str = "issues.clusters_extraction";
/// How far a caller's `since` may sit from the stored window's start and still be served
/// from it: `recurring_issues_cached` buckets `since` to the hour, and an hour at the far end
/// of 180 days does not move a cluster's rank.
const WINDOW_SLACK_SECS: i64 = 3_600;

/// app_config key: JSON `{ "<signature>": [thread ids, newest sighting first] }` for the
/// stored clusters ([`IssueCluster::thread_ids`]). Kept beside the table rather than in it:
/// phase 2 of Batch H adds no migration, and at most 500 x 10 ids it stays a small value.
const CLUSTER_THREADS: &str = "issues.cluster_threads";

/// Recompute the recurring error clusters over the last [`CLUSTER_WINDOW_SECS`] (every
/// project, the top [`CLUSTER_TABLE_CAP`]) and store them in `issue_clusters`, keyed by the
/// normalized signature, then resolve them per project ([`resolve`]) and record or refresh
/// each project's playbook gotcha for each resolved cluster
/// ([`crate::knowledge::upsert_playbook`]). A project's fix that is not found again is kept
/// (its sightings can fall out of the window or the candidate cap); a signature that no longer recurs stays in the table out of the
/// readers' sight (see [`store_clusters`]), and its playbook facts stay. Mining and resolving
/// run before the write lock is taken (about 0.7 s warm, 3.7 s cold on a 21k-thread index);
/// the write is one short transaction. Returns the clusters stored. The watcher and
/// `cal index` run it after new messages (see [`clusters_due`]); a caller that should not
/// mine on its writer runs [`mine_for_refresh`] on a reader and [`store_clusters`] on the
/// writer instead.
pub fn refresh_clusters(conn: &Connection) -> Result<usize> {
    refresh_clusters_at(conn, chrono::Utc::now().timestamp())
}

/// [`refresh_clusters`] at `now`.
pub(crate) fn refresh_clusters_at(conn: &Connection, now: i64) -> Result<usize> {
    let mined = mine_for_refresh(conn, now)?;
    store_clusters(conn, &mined, now)
}

/// What [`mine_for_refresh`] hands [`store_clusters`]: the clusters and the fixes
/// [`resolve`] found for them, per project.
pub struct MinedClusters {
    mined: Vec<Mined>,
    fixes: Fixes,
}

impl MinedClusters {
    /// How many clusters were mined.
    pub fn len(&self) -> usize {
        self.mined.len()
    }

    pub fn is_empty(&self) -> bool {
        self.mined.is_empty()
    }
}

/// The mining and resolving half of [`refresh_clusters`]: read-only, so it can run on a
/// pooled reader. Resolving runs `git show` in the fixing sessions' checkouts (bounded, see
/// [`GIT_SHOWS_PER_REFRESH`]).
pub fn mine_for_refresh(conn: &Connection, now: i64) -> Result<MinedClusters> {
    let mined = mine(
        conn,
        None,
        ProjectMatch::Exact,
        now - CLUSTER_WINDOW_SECS,
        CLUSTER_TABLE_CAP,
        Sightings::All,
    )?;
    let fixes = resolve(conn, &mined)?;
    Ok(MinedClusters { mined, fixes })
}

/// The write half of [`refresh_clusters`]: upsert the clusters (mined at `now`), merge their
/// fixes into every project's ([`CLUSTER_FIXES`]), refresh the playbooks and stamp the
/// refresh. A row this refresh did not see is kept, out of the readers' sight (they read only
/// the current refresh's rows): a fixed error stops recurring and loses rank, and it is
/// exactly the row the playbook resolver filled, so dropping it threw the resolution away
/// with it. Rows with `resolved_*` set are never dropped; the rest go once unseen for
/// [`CLUSTER_WINDOW_SECS`]. A fix of a signature this refresh did not mine and whose stored
/// example the current line rules reject is dropped, with its row's resolution and its
/// uncurated playbook facts.
pub fn store_clusters(conn: &Connection, clusters: &MinedClusters, now: i64) -> Result<usize> {
    let MinedClusters { mined, fixes } = clusters;
    let threads: HashMap<&str, &[i64]> = mined
        .iter()
        .map(|m| (m.signature.as_str(), m.cluster.thread_ids.as_slice()))
        .collect();
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    {
        let mut up = tx.prepare(
            "INSERT INTO issue_clusters
                (signature, example, count, threads, first_seen, last_seen, last_thread_id,
                 refreshed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(signature) DO UPDATE SET example = excluded.example,
                count = excluded.count, threads = excluded.threads,
                first_seen = excluded.first_seen, last_seen = excluded.last_seen,
                last_thread_id = excluded.last_thread_id, refreshed_at = excluded.refreshed_at",
        )?;
        for m in mined {
            let c = &m.cluster;
            up.execute(rusqlite::params![
                m.signature,
                c.example,
                c.count,
                c.threads,
                c.first_seen,
                c.last_seen,
                c.last_thread_id,
                now
            ])?;
        }
    }
    // This refresh's fixes over the ones earlier refreshes found: a fixed error stops
    // recurring, so the sighting that resolved it falls out of the window or the candidate
    // cap and is not found again. Read inside the write transaction, so two refreshes never
    // interleave their merges.
    let mut all = load_fixes(&tx);
    for (signature, per_project) in fixes {
        let kept = all.entry(signature.clone()).or_default();
        for (project, fix) in per_project {
            kept.insert(project.clone(), fix.clone());
        }
    }
    // Every stored fix's playbook ref, for telling a playbook fact's signature apart from its
    // project key, which can hold an `@` too ([`drop_generic_playbooks`]).
    let refs: HashMap<String, String> = all
        .iter()
        .flat_map(|(s, per)| per.keys().map(move |p| (playbook_ref(s, p), s.clone())))
        .collect();
    let mut unresolved: Vec<String> = Vec::new();
    // Signatures the current rules no longer produce (see below).
    let mut retired: HashSet<String> = HashSet::new();
    {
        // A fix whose commit link is gone (gitlink drops a link that stops matching, a thread
        // delete cascades) no longer has a commit to show.
        let mut link = tx.prepare(LINK_EXISTS_SQL)?;
        let mut latest = tx.prepare(LATEST_PROJECT_SQL)?;
        let mut example_of = tx.prepare(STORED_EXAMPLE_SQL)?;
        let mut set = tx.prepare(
            "UPDATE issue_clusters SET resolved_thread_id = ?2, resolved_sha = ?3
             WHERE signature = ?1",
        )?;
        let mut gone: Vec<String> = Vec::new();
        for (signature, per_project) in all.iter_mut() {
            per_project.retain(|_, fix| {
                link.exists(rusqlite::params![fix.thread_id, fix.sha])
                    .unwrap_or(false)
            });
            // The row's latest sighting picks the fix unscoped readers show; no row (deleted
            // by hand) drops the signature's fixes with it.
            let row: Option<Option<String>> =
                latest.query_row([signature], |r| r.get(0)).optional()?;
            let Some(latest_project) = row else {
                gone.push(signature.clone());
                continue;
            };
            // A fix recorded under older rules for a line they took and the current ones do
            // not (a grep or diff hit on a source line, `error: 'Unable to refresh the
            // token',`) is no fix of an error: never mined again, resolved and so never
            // pruned, its playbook stayed in project memory for good. Judged by the stored
            // example's line rules, not a re-run of the extractor: a traceback's exception
            // line without a marker only survives inside its block.
            if !threads.contains_key(signature.as_str()) {
                let example: Option<String> =
                    example_of.query_row([signature], |r| r.get(0)).optional()?;
                if example.is_some_and(|e| !still_an_error(&e)) {
                    retired.insert(signature.clone());
                    unresolved.push(signature.clone());
                    continue;
                }
            }
            match unscoped_fix(per_project, latest_project.as_deref()) {
                Some((_, fix)) => {
                    set.execute(rusqlite::params![signature, fix.thread_id, fix.sha])?;
                }
                None => unresolved.push(signature.clone()),
            }
        }
        for signature in &gone {
            all.remove(signature);
        }
        let mut clear = tx.prepare(
            "UPDATE issue_clusters SET resolved_thread_id = NULL, resolved_sha = NULL
             WHERE signature = ?1",
        )?;
        for signature in &unresolved {
            clear.execute([signature])?;
            all.remove(signature);
        }
    }
    tx.execute(PRUNE_CLUSTERS_SQL, [now, now - CLUSTER_WINDOW_SECS])?;
    crate::knowledge::app_config::put(&tx, CLUSTERS_REFRESHED_AT, &now.to_string())?;
    crate::knowledge::app_config::put(&tx, CLUSTERS_EXTRACTION, &extraction_stamp(now))?;
    crate::knowledge::app_config::put(&tx, CLUSTER_THREADS, &serde_json::to_string(&threads)?)?;
    crate::knowledge::app_config::put(&tx, CLUSTER_FIXES, &serde_json::to_string(&all)?)?;
    refresh_playbooks(&tx, &all, now)?;
    drop_generic_playbooks(&tx, &refs, &retired)?;
    tx.commit()?;
    Ok(mined.len())
}

/// A stored cluster's example line.
const STORED_EXAMPLE_SQL: &str = "SELECT example FROM issue_clusters WHERE signature = ?1";

/// Whether a stored example line still passes the line rules of
/// [`extract_errors_with_files`]: a marker or a traceback's exception line, no source line
/// and no read gutter.
fn still_an_error(example: &str) -> bool {
    (is_error_line(example) || is_python_exception(example))
        && !reads_as_code(example)
        && !behind_gutter(example)
}

/// One project's fix of one cluster: the session that hit the error and the linked commit
/// of its that changed the file the error named.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Resolution {
    pub thread_id: i64,
    pub sha: String,
    pub committed_at: i64,
}

/// Each cluster's fixes by signature, then by project key (`COALESCE(project_key,
/// project_path)` of the fixing session): what [`CLUSTER_FIXES`] stores. Sorted maps keep the
/// stored JSON and the playbook writes in a stable order.
pub(crate) type Fixes = BTreeMap<String, BTreeMap<String, Resolution>>;

/// app_config key: [`Fixes`] as JSON. Kept beside the table rather than in it (phase 2 of
/// Batch H adds no migration): one `resolved_*` pair per signature let a fix in another repo
/// take the cluster over, and with it the playbook fact and the first repo's "Last fixed in".
/// `issue_clusters.resolved_*` keep the fix unscoped readers show ([`unscoped_fix`]).
const CLUSTER_FIXES: &str = "issues.cluster_fixes";

/// The stored fixes; empty before the first refresh and on an unreadable value.
fn load_fixes(conn: &Connection) -> Fixes {
    crate::knowledge::app_config::get(conn, CLUSTER_FIXES)
        .ok()
        .flatten()
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

/// The fix an unscoped read shows: the one made in the project of the cluster's latest
/// sighting, where the error is live; else the most recent fix in any project.
fn unscoped_fix<'a>(
    per_project: &'a BTreeMap<String, Resolution>,
    latest_project: Option<&str>,
) -> Option<(&'a String, &'a Resolution)> {
    latest_project
        .and_then(|p| per_project.get_key_value(p))
        .or_else(|| {
            per_project
                .iter()
                .max_by_key(|(_, fix)| (fix.committed_at, fix.thread_id))
        })
}

/// Most `git show` runs one refresh makes for commits not seen before (cached after, see
/// [`COMMIT_FILES`]). On a 21k-thread index the first refresh showed 100 commits in about
/// 1.3 s of a 1.6 s resolve; a whole refresh takes 0.8 s once they are cached, so later ones
/// pay only for new commits.
pub(crate) const GIT_SHOWS_PER_REFRESH: usize = 200;
/// Commits after the edit checked per sighting: the fix lands soon after the edit, and a long
/// session's later commits would otherwise spend the whole budget on one sighting.
const COMMITS_PER_SIGHTING: usize = 6;
/// One `git show` past this is killed and counted as unknown (a sleeping network mount).
const GIT_SHOW_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
/// Commits whose files [`COMMIT_FILES`] holds; past it the cache starts over.
const COMMIT_FILES_CAP: usize = 8_192;

/// The files each commit changed, by full sha: a commit never changes, so the watcher's
/// refreshes (every few minutes) share one cache for the life of the process.
static COMMIT_FILES: std::sync::LazyLock<std::sync::Mutex<HashMap<String, Vec<String>>>> =
    std::sync::LazyLock::new(Default::default);

/// Tests seed the files of their made-up commits, which no checkout holds.
#[cfg(test)]
pub(crate) fn seed_commit_files(sha: &str, files: &[&str]) {
    COMMIT_FILES.lock().unwrap().insert(
        sha.to_string(),
        files.iter().map(|f| f.to_string()).collect(),
    );
}

/// [`COMMIT_FILES`] with one refresh's budget of `git show` runs.
struct CommitFiles {
    git_left: usize,
}

impl CommitFiles {
    /// The files `sha` changed (paths from the repo root), run in `dir` when not cached. None
    /// when that is unknown this time: no checkout on this machine, the budget is spent, or
    /// git timed out. A commit git cannot show there (history rewritten, a removed clone)
    /// changed nothing as far as the resolver can tell.
    fn files(&mut self, dir: Option<&str>, sha: &str) -> Option<Vec<String>> {
        if let Some(files) = COMMIT_FILES.lock().ok()?.get(sha) {
            return Some(files.clone());
        }
        if !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Some(Vec::new());
        }
        let dir = dir?;
        if self.git_left == 0 {
            return None;
        }
        self.git_left -= 1;
        let deadline = std::time::Instant::now() + GIT_SHOW_TIMEOUT;
        let args = [
            "-c",
            "core.quotePath=false",
            "show",
            "--no-renames",
            "--name-only",
            "--format=",
            sha,
        ];
        let files: Vec<String> = match crate::gitlink::run_git_until(dir, &args, deadline) {
            Ok(Some(out)) => out
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect(),
            Ok(None) => return None,
            Err(_) => Vec::new(),
        };
        let mut cache = COMMIT_FILES.lock().ok()?;
        if cache.len() >= COMMIT_FILES_CAP {
            cache.clear();
        }
        cache.insert(sha.to_string(), files.clone());
        Some(files)
    }
}

/// Find each cluster's fix in each project: per project, the latest sighting whose session,
/// after that sighting, edited (or wrote) a file that sighting's own error lines named, and
/// then has a linked commit (`thread_commits`, at or after that edit) that changed that file
/// (`git show`, see [`CommitFiles`]). The commit is the first such one: the fix, not later
/// work in the same session. A sighting is checked against its own files, not the cluster's:
/// one signature merges errors whose paths differ, across repos too. A cluster whose error
/// lines name no file, or that no linked session edited, has none, and so does one whose
/// signature only says that something failed ([`is_specific`]). Empty on an index without
/// `file_edits`.
pub(crate) fn resolve(conn: &Connection, mined: &[Mined]) -> Result<Fixes> {
    let mut out = Fixes::new();
    if !crate::db::has_table(conn, "file_edits") {
        return Ok(out);
    }
    // Only sessions with a linked commit can resolve anything: a few hundred on a large
    // index, read once, so the per-sighting lookups below skip every other session.
    let linked: HashSet<i64> = conn
        .prepare(LINKED_THREADS_SQL)?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    if linked.is_empty() {
        return Ok(out);
    }
    let mut edits_of = conn.prepare(THREAD_EDITS_SQL)?;
    let mut commits_of = conn.prepare(THREAD_COMMITS_SQL)?;
    let mut dirs_of = conn.prepare(THREAD_DIRS_SQL)?;
    let mut edits: HashMap<i64, Vec<(String, i64)>> = HashMap::new();
    let mut commits: HashMap<i64, Vec<(String, i64)>> = HashMap::new();
    let mut dirs: HashMap<i64, Option<String>> = HashMap::new();
    let mut git = CommitFiles {
        git_left: GIT_SHOWS_PER_REFRESH,
    };
    for m in mined.iter().filter(|m| is_specific(&m.signature)) {
        for s in &m.sightings {
            if s.files.is_empty() || !linked.contains(&s.thread) {
                continue;
            }
            let Some(project) = s.project.as_deref() else {
                continue;
            };
            if out
                .get(&m.signature)
                .is_some_and(|fixed| fixed.contains_key(project))
            {
                continue;
            }
            let thread_edits = match edits.entry(s.thread) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(e) => {
                    // The session's subagents edit for it, as their errors count for it.
                    let mut session = Vec::new();
                    for id in crate::indexer::session_thread_ids(conn, s.thread)? {
                        for edit in edits_of.query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))? {
                            session.push(edit?);
                        }
                    }
                    e.insert(session)
                }
            };
            let erroring = erroring_edits(&s.files, thread_edits, s.seen);
            let Some(edited_at) = erroring.iter().map(|(_, ts)| *ts).min() else {
                continue;
            };
            let thread_commits = match commits.entry(s.thread) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(e) => e.insert(
                    commits_of
                        .query_map([s.thread], |r| Ok((r.get(0)?, r.get(1)?)))?
                        .collect::<rusqlite::Result<Vec<_>>>()?,
                ),
            };
            let dir = match dirs.entry(s.thread) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(e) => e.insert(checkout_dir(&mut dirs_of, s.thread)?),
            };
            for (sha, at) in thread_commits
                .iter()
                .filter(|(_, at)| *at >= edited_at)
                .take(COMMITS_PER_SIGHTING)
            {
                // Unknown (no checkout, no budget left): this sighting stays open, and an
                // earlier refresh's fix for it is kept.
                let Some(changed) = git.files(dir.as_deref(), sha) else {
                    break;
                };
                let touched = changed
                    .iter()
                    .any(|c| erroring.iter().any(|(e, _)| commit_touches(c, e)));
                if touched {
                    out.entry(m.signature.clone()).or_default().insert(
                        project.to_string(),
                        Resolution {
                            thread_id: s.thread,
                            sha: sha.clone(),
                            committed_at: *at,
                        },
                    );
                    break;
                }
            }
        }
    }
    Ok(out)
}

/// A session's checkout on this machine, for `git show`: its working directory, else its
/// project key (a removed worktree's main checkout), whichever still exists.
fn checkout_dir(dirs_of: &mut rusqlite::Statement, thread: i64) -> Result<Option<String>> {
    let (path, key): (Option<String>, Option<String>) = dirs_of
        .query_row([thread], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?
        .unwrap_or_default();
    Ok([path, key]
        .into_iter()
        .flatten()
        .find(|d| std::path::Path::new(d).is_dir()))
}

/// The session's edits at or after `seen` of the files an error named, as `(path, ts)`. For
/// each named file only the edits sharing the longest path tail with it count: an error
/// naming `src/lib.rs` in a workspace whose session edited `src/lib.rs` and
/// `core/src/lib.rs` means the first.
fn erroring_edits<'a>(
    named: &[String],
    edits: &'a [(String, i64)],
    seen: i64,
) -> Vec<(&'a str, i64)> {
    let mut out: Vec<(&str, i64)> = Vec::new();
    for f in named {
        let scored: Vec<(usize, &(String, i64))> = edits
            .iter()
            .filter(|(_, ts)| *ts >= seen)
            .map(|e| (path_match_len(f, &e.0), e))
            .collect();
        let best = scored.iter().map(|(n, _)| *n).max().unwrap_or(0);
        if best == 0 {
            continue;
        }
        for (n, (path, ts)) in scored {
            if n == best && !out.contains(&(path.as_str(), *ts)) {
                out.push((path.as_str(), *ts));
            }
        }
    }
    out
}

/// Marker words that say an error happened, not which one.
const MARKER_WORDS: &[&str] = &[
    "error",
    "errors",
    "fatal",
    "exception",
    "panic",
    "panicked",
    "failed",
    "failure",
    "warning",
];

/// Whether a signature names what went wrong rather than only that something did: at least
/// three plain words beside the markers and placeholders. On a 21k-thread index the generic
/// headers (`error:`, `error during build:`, a Python `Traceback (most recent call last):`
/// whose cluster merges every Python error, CI's `Process completed with exit code 1.`)
/// resolved to whatever the session committed next, which is no playbook. So did a path
/// lookup that missed (`ugrep: warning: <path> no such file or directory`): agents probe
/// paths while exploring, and the next edit of that path fixes nothing.
pub(crate) fn is_specific(signature: &str) -> bool {
    // cargo's closing summaries (`could not compile '…' (lib) due to 2 previous errors`,
    // `aborting due to 2 previous errors`) and npm's (`npm err! code elifecycle`,
    // `command failed`) follow nearly every failed build, whichever error caused it. So do
    // the runners' (turbo's `error run failed: command exited 1`, 69 sightings on a large
    // index; CI's `process completed with exit code 1`), and `git apply`'s `<path> patch
    // does not apply` only says a hunk no longer matched (yet one of the seven fixes
    // recorded there was for it). cargo's `error: test failed, to rerun pass '-p <crate>
    // --lib'` closes every failing `cargo test` run (118 sightings there once subagents
    // counted, the third most frequent cluster).
    const GENERIC: &[&str] = &[
        "exit code",
        "no such file",
        "could not compile",
        "aborting due to",
        "previous error",
        "elifecycle",
        "command failed",
        "command exited",
        "patch does not apply",
        "to rerun pass",
    ];
    if signature.starts_with("traceback") || GENERIC.iter().any(|g| signature.contains(g)) {
        return false;
    }
    signature
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| {
            w.len() >= 2 && w.chars().all(|c| c.is_ascii_alphabetic()) && !MARKER_WORDS.contains(w)
        })
        .count()
        >= 3
}

/// Sessions with a linked commit (the covering thread index, no table read).
const LINKED_THREADS_SQL: &str = "SELECT DISTINCT thread_id FROM thread_commits";
/// One session's edits and writes, by `idx_file_edits_thread`.
const THREAD_EDITS_SQL: &str = "SELECT path, ts FROM file_edits
     WHERE thread_id = ?1 AND op IN ('edit', 'write') AND ts IS NOT NULL";
/// One session's linked commits, oldest first.
const THREAD_COMMITS_SQL: &str = "SELECT sha, committed_at FROM thread_commits
     WHERE thread_id = ?1 ORDER BY committed_at, sha";
/// One session's working directory and project key ([`checkout_dir`]).
const THREAD_DIRS_SQL: &str = "SELECT project_path, project_key FROM threads WHERE id = ?1";
/// Whether a stored fix's commit link still exists (the primary key).
const LINK_EXISTS_SQL: &str = "SELECT 1 FROM thread_commits WHERE thread_id = ?1 AND sha = ?2";
/// The project of a stored cluster's latest sighting: no row when the signature is not
/// stored, a NULL when that session is gone.
const LATEST_PROJECT_SQL: &str = "SELECT COALESCE(t.project_key, t.project_path)
     FROM issue_clusters c LEFT JOIN threads t ON t.id = c.last_thread_id
     WHERE c.signature = ?1";

/// A path split into its segments, `.` and empty ones dropped, either separator.
fn segments(p: &str) -> Vec<&str> {
    p.split(['/', '\\'])
        .filter(|s| !s.is_empty() && *s != ".")
        .collect()
}

/// How many trailing segments two paths share when the shorter one is a tail of the other
/// (`/repo/src/a.rs` and `src/a.rs`: 2), else 0. `file_edits` holds checkout-relative paths
/// (absolute ones outside the checkout) while errors print absolute, relative or bare names,
/// so a tail is the most two spellings of one file share. A bare name matches only a last
/// segment equal to it (`lib.rs` never matches `mylib.rs`).
fn path_match_len(a: &str, b: &str) -> usize {
    let (a, b) = (segments(a), segments(b));
    let n = a
        .iter()
        .rev()
        .zip(b.iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    if n > 0 && n == a.len().min(b.len()) {
        n
    } else {
        0
    }
}

/// Whether a file a commit changed (`changed`, from the repo root, as `git show` prints it)
/// is the file a session edited (`edited`, relative to the session's checkout, or absolute
/// outside it): the same path, `edited` relative to a directory below the root (its
/// checkout probe found no `.git` when it was indexed), or `edited` absolute and ending in
/// it. A root `README.md` is no match for an edited `docs/README.md`.
fn commit_touches(changed: &str, edited: &str) -> bool {
    let n = path_match_len(changed, edited);
    let (c, e) = (segments(changed).len(), segments(edited).len());
    let absolute = edited.starts_with(['/', '\\']) || edited.as_bytes().get(1) == Some(&b':');
    // `n` is at most the shorter length: `n == e` is the same path or `edited` below the root.
    n > 0 && (n == e || (n == c && absolute))
}

/// `facts.origin_ref` of a cluster's playbook gotcha in one project: one fact per signature
/// and project, so a fix in one repo never moves another repo's playbook
/// ([`crate::knowledge::upsert_playbook`] moves a fact only between sessions of one ref).
pub fn playbook_ref(signature: &str, project: &str) -> String {
    format!(
        "{}:{signature}@{project}",
        crate::knowledge::EXTRACTOR_PLAYBOOK
    )
}

/// The playbook line: "<error signature>: seen N times; fixed in <shortSha> (<subject>), see
/// thread <id>". Redacted (error lines can carry a token from a failing request) and capped.
pub fn playbook_text(
    signature: &str,
    count: i64,
    short_sha: &str,
    subject: Option<&str>,
    thread_id: i64,
) -> String {
    let fix = match subject.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => format!("{short_sha} ({})", cap_chars(s, 120)),
        None => short_sha.to_string(),
    };
    let text = format!(
        "{}: seen {count} times; fixed in {fix}, see thread {thread_id}",
        cap_chars(signature, 200)
    );
    crate::context::redact_secrets(&text).into_owned()
}

fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// A stored fix's commit with the cluster's count, for its playbook line.
const PLAYBOOK_SOURCE_SQL: &str = "SELECT tc.short_sha, tc.subject, c.count
     FROM thread_commits tc JOIN issue_clusters c ON c.signature = ?3
     WHERE tc.thread_id = ?1 AND tc.sha = ?2";

/// Record or refresh the playbook gotcha of every project's fix of every cluster (one fact
/// per signature and project, updated in place). Returns how many facts were written.
fn refresh_playbooks(conn: &Connection, fixes: &Fixes, now: i64) -> Result<usize> {
    let mut source = conn.prepare(PLAYBOOK_SOURCE_SQL)?;
    let mut written = 0;
    for (signature, per_project) in fixes {
        if !is_specific(signature) {
            continue;
        }
        for (project, fix) in per_project {
            let row: Option<(String, Option<String>, i64)> = source
                .query_row(rusqlite::params![fix.thread_id, fix.sha, signature], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .optional()?;
            let Some((short, subject, count)) = row else {
                continue;
            };
            let text = playbook_text(signature, count, &short, subject.as_deref(), fix.thread_id);
            let origin = playbook_ref(signature, project);
            if crate::knowledge::upsert_playbook(conn, &origin, fix.thread_id, &text, now)? {
                written += 1;
            }
        }
    }
    Ok(written)
}

/// The playbook gotchas a refresh may remove: every one, off the `origin_ref` index
/// (`playbook:` up to the next character, `;`), the user's own kept out. A pinned or edited
/// one is the user's, and a hidden one is a tombstone that keeps it from coming back.
const MACHINE_PLAYBOOKS_SQL: &str = "SELECT id, origin_ref FROM facts
     WHERE origin_ref >= 'playbook:' AND origin_ref < 'playbook;'
       AND extractor = 'playbook' AND pinned = 0 AND edited = 0 AND hidden = 0";

/// Delete the playbook gotchas of signatures that no longer name their error
/// ([`is_specific`]): ones written before a rule made them generic (`<path> patch does not
/// apply`, one of the seven fixes recorded on a large index) stayed in the repo's memory,
/// injected at session start and in the AGENTS block, though every list hid the cluster.
/// A signature that reads as specific again gets a new one on the next refresh. The facts
/// of the `retired` signatures, which the current rules no longer produce, go too. A
/// fact's signature is the one `refs` (each stored fix's [`playbook_ref`]) gives for its
/// `origin_ref`, else [`playbook_signature`]'s guess. Returns how many were deleted.
fn drop_generic_playbooks(
    conn: &Connection,
    refs: &HashMap<String, String>,
    retired: &HashSet<String>,
) -> Result<usize> {
    let generic: Vec<i64> = {
        let mut stmt = conn.prepare(MACHINE_PLAYBOOKS_SQL)?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        let mut ids = Vec::new();
        for row in rows {
            let (id, origin) = row?;
            let signature = refs
                .get(&origin)
                .map(String::as_str)
                .or_else(|| playbook_signature(&origin));
            if signature.is_some_and(|s| !is_specific(s) || retired.contains(s)) {
                ids.push(id);
            }
        }
        ids
    };
    let mut delete = conn.prepare("DELETE FROM facts WHERE id = ?1")?;
    for id in &generic {
        delete.execute([id])?;
    }
    Ok(generic.len())
}

/// The signature of a [`playbook_ref`], guessed: the project key follows the last `@`, as a
/// signature can hold an npm scope (`@scope/pkg`). A key can hold one too (a cloud-storage
/// folder named after an email address), which cuts the signature short and can make a
/// specific one read as generic, so a ref of a stored fix is looked up first.
fn playbook_signature(origin_ref: &str) -> Option<&str> {
    let rest = origin_ref
        .strip_prefix(crate::knowledge::EXTRACTOR_PLAYBOOK)?
        .strip_prefix(':')?;
    Some(rest.rsplit_once('@')?.0)
}

const PRUNE_CLUSTERS_SQL: &str = "DELETE FROM issue_clusters
     WHERE refreshed_at <> ?1 AND last_seen < ?2
       AND resolved_thread_id IS NULL AND resolved_sha IS NULL";

/// [`CLUSTERS_EXTRACTION`]'s value for a refresh at `at`.
fn extraction_stamp(at: i64) -> String {
    format!("{EXTRACTION_VERSION}@{at}")
}

/// The last refresh's time, and whether anything was indexed after it (a thread's
/// `last_indexed_at` past it, off 0034's index). None before the first refresh, after one
/// mined with other rules ([`EXTRACTION_VERSION`]), or on an index from before 0034 (a
/// read-only connection never migrates).
fn refresh_state(conn: &Connection) -> Option<(i64, bool)> {
    if !crate::db::has_table(conn, "issue_clusters") {
        return None;
    }
    let at: i64 = crate::knowledge::app_config::get(conn, CLUSTERS_REFRESHED_AT)
        .ok()??
        .parse()
        .ok()?;
    let stamp = crate::knowledge::app_config::get(conn, CLUSTERS_EXTRACTION).ok()??;
    if stamp != extraction_stamp(at) {
        return None;
    }
    let newest: Option<i64> = conn
        .query_row("SELECT MAX(last_indexed_at) FROM threads", [], |r| r.get(0))
        .ok()?;
    Some((at, newest.is_some_and(|n| n > at)))
}

/// Whether a refresh is due at `now`: none yet, something was indexed since the last one and
/// it is at least `min_age_secs` old, or it is a day old ([`CLUSTERS_IDLE_SECS`]) however
/// quiet the machine was. False on an index without the table.
pub fn clusters_due(conn: &Connection, now: i64, min_age_secs: i64) -> bool {
    if !crate::db::has_table(conn, "issue_clusters") {
        return false;
    }
    match refresh_state(conn) {
        None => true,
        Some((at, changed)) => {
            let age = now - at;
            age >= CLUSTERS_IDLE_SECS || (changed && age >= min_age_secs) || age < 0
        }
    }
}

/// The stored clusters, when the last refresh covers this request: `limit` is within the
/// table's cap, and either the refresh is under [`CLUSTERS_FRESH_SECS`] old with its window starting within
/// [`WINDOW_SLACK_SECS`] of `since`, or nothing was indexed since it and its window start
/// sits within [`CLUSTERS_IDLE_SECS`] of `since`. None otherwise, and on an index from before
/// 0034, so the caller mines live.
fn stored_clusters(
    conn: &Connection,
    since: i64,
    limit: usize,
    now: i64,
) -> Option<Vec<IssueCluster>> {
    if limit > CLUSTER_TABLE_CAP {
        return None;
    }
    let (at, changed) = refresh_state(conn)?;
    let age = now - at;
    let drift = (since - (at - CLUSTER_WINDOW_SECS)).abs();
    let recent = (0..CLUSTERS_FRESH_SECS).contains(&age) && drift <= WINDOW_SLACK_SECS;
    let idle = !changed && age >= 0 && drift <= CLUSTERS_IDLE_SECS;
    if !recent && !idle {
        return None;
    }
    let mut threads: HashMap<String, Vec<i64>> =
        crate::knowledge::app_config::get(conn, CLUSTER_THREADS)
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default();
    let read = || -> rusqlite::Result<Vec<IssueCluster>> {
        let mut stmt = conn.prepare_cached(STORED_CLUSTERS_SQL)?;
        let rows = stmt
            .query_map(rusqlite::params![limit as i64, at], |r| {
                Ok(IssueCluster {
                    example: r.get(0)?,
                    count: r.get(1)?,
                    threads: r.get(2)?,
                    first_seen: r.get(3)?,
                    last_seen: r.get(4)?,
                    last_thread_id: r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                    thread_ids: Vec::new(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    };
    let mut rows = read().ok()?;
    for c in &mut rows {
        c.thread_ids = threads.remove(&normalize(&c.example)).unwrap_or_default();
    }
    Some(rows)
}

/// The last refresh's rows only (older rows are kept for their resolution, see
/// [`store_clusters`]). The table holds the top [`CLUSTER_TABLE_CAP`] plus what fell out of
/// it over the window, a few thousand rows at most, so the scan and sort are small.
const STORED_CLUSTERS_SQL: &str =
    "SELECT example, count, threads, first_seen, last_seen, last_thread_id FROM issue_clusters
     WHERE refreshed_at = ?2 ORDER BY count DESC, last_seen DESC LIMIT ?1";

/// [`recurring_issues_matching`] with each cluster's fix and playbook ([`with_fixes`]), only
/// the clusters that name their error ([`names_the_error`]): what `cal issues`, MCP
/// `recurring_issues` and the report list.
pub fn recurring_issues_with_fixes(
    conn: &Connection,
    project: Option<&str>,
    mode: ProjectMatch,
    since: i64,
    limit: usize,
) -> Result<Vec<Issue>> {
    listed_issues(conn, project, mode, since, limit, false)
}

/// [`recurring_issues_with_fixes`], or with `all` the generic clusters too
/// (`cal issues --all`).
pub fn listed_issues(
    conn: &Connection,
    project: Option<&str>,
    mode: ProjectMatch,
    since: i64,
    limit: usize,
    all: bool,
) -> Result<Vec<Issue>> {
    let clusters = if all {
        recurring_issues_matching(conn, project, mode, since, limit)?
    } else {
        // Filtered before the limit, from as many as the table keeps: mining ranks every
        // cluster anyway, and the stored read is a few hundred rows.
        let ranked =
            recurring_issues_matching(conn, project, mode, since, limit.max(CLUSTER_TABLE_CAP))?;
        specific_clusters(ranked, limit)
    };
    Ok(with_fixes(conn, clusters, project, mode))
}

/// Whether a cluster names what went wrong ([`is_specific`] of its signature). The listings
/// (`cal issues`, MCP, the report, Today, the Project page) show only these: on a large
/// index 32% of the stored clusters failed it, 11 of the top 30 (`error:`, CI's exit-code
/// line, cargo's `could not compile`). The table keeps the rest, so counts stay honest and
/// `cal issues --all` lists them.
pub fn names_the_error(cluster: &IssueCluster) -> bool {
    is_specific(&normalize(&cluster.example))
}

/// The first `limit` of `clusters` that name their error, in order.
pub fn specific_clusters(clusters: Vec<IssueCluster>, limit: usize) -> Vec<IssueCluster> {
    clusters
        .into_iter()
        .filter(names_the_error)
        .take(limit)
        .collect()
}

/// A fix's commit (`?1` thread, `?2` sha) and its project's visible playbook text (`?3`, the
/// origin_ref).
const FIX_DETAIL_SQL: &str = "SELECT tc.short_sha, tc.subject, tc.committed_at,
        (SELECT f.text FROM facts f WHERE f.origin_ref = ?3 AND f.hidden = 0
         ORDER BY f.id LIMIT 1)
     FROM thread_commits tc WHERE tc.thread_id = ?1 AND tc.sha = ?2";
/// A thread's project key.
const PROJECT_OF_SQL: &str =
    "SELECT COALESCE(project_key, project_path) FROM threads WHERE id = ?1";

/// Attach each cluster's fix (its commit and subject) and that project's playbook gotcha.
/// With a `project`, the newest fix made in a project in that scope: a fix made in another
/// repo is left out, its commit is not in this one. Without, the fix of the project of the
/// cluster's latest sighting, else the newest anywhere ([`unscoped_fix`]). Clusters come back
/// without fixes before the first refresh and on any read error: the list itself still
/// answers.
pub fn with_fixes(
    conn: &Connection,
    clusters: Vec<IssueCluster>,
    project: Option<&str>,
    mode: ProjectMatch,
) -> Vec<Issue> {
    let project = project.filter(|p| !p.is_empty());
    // A fix's stored project key is in scope as the SQL scope selects it.
    let scope = project.map(|p| crate::scope::ProjectScope::with_match(p, mode));
    let fixes = load_fixes(conn);
    let mut detail = (!fixes.is_empty())
        .then(|| conn.prepare(FIX_DETAIL_SQL).ok())
        .flatten();
    let mut project_of = (!fixes.is_empty() && project.is_none())
        .then(|| conn.prepare(PROJECT_OF_SQL).ok())
        .flatten();
    clusters
        .into_iter()
        .map(|cluster| {
            let signature = normalize(&cluster.example);
            let chosen = fixes.get(&signature).and_then(|per_project| match &scope {
                Some(scope) => per_project
                    .iter()
                    .filter(|(key, _)| scope.matches(key))
                    .max_by_key(|(_, fix)| (fix.committed_at, fix.thread_id)),
                None => {
                    let latest: Option<String> = project_of.as_mut().and_then(|stmt| {
                        stmt.query_row([cluster.last_thread_id], |r| r.get(0))
                            .ok()
                            .flatten()
                    });
                    unscoped_fix(per_project, latest.as_deref())
                }
            });
            let found = chosen.and_then(|(key, fix)| {
                detail.as_mut().and_then(|stmt| {
                    stmt.query_row(
                        rusqlite::params![fix.thread_id, fix.sha, playbook_ref(&signature, key)],
                        |r| {
                            Ok((
                                IssueFix {
                                    sha: fix.sha.clone(),
                                    short_sha: r.get(0)?,
                                    subject: r.get(1)?,
                                    committed_at: r.get(2)?,
                                    thread_id: fix.thread_id,
                                },
                                r.get::<_, Option<String>>(3)?,
                            ))
                        },
                    )
                    .ok()
                })
            });
            let (fix, playbook) = match found {
                Some((fix, playbook)) => (Some(fix), playbook),
                None => (None, None),
            };
            Issue {
                thread_ids: cluster.thread_ids.clone(),
                cluster,
                fix,
                playbook,
            }
        })
        .collect()
}

/// A playbook line for the error-recall hook ([`playbooks_for`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybookHit {
    pub signature: String,
    pub text: String,
}

/// A stored cluster's count.
const CLUSTER_COUNT_SQL: &str = "SELECT count FROM issue_clusters WHERE signature = ?1";
/// A fix's commit (`?1` thread, `?2` sha) with its playbook fact (`?3`), hidden or not.
const PLAYBOOK_HIT_SQL: &str = "SELECT tc.short_sha, tc.subject, f.text, COALESCE(f.hidden, 0)
     FROM thread_commits tc LEFT JOIN facts f ON f.origin_ref = ?3
     WHERE tc.thread_id = ?1 AND tc.sha = ?2
     ORDER BY f.id LIMIT 1";

/// The playbook lines for these error signatures whose fix was made in `project` (an exact
/// key): the fact's text (the user may have edited it), or the line rebuilt when the fact is
/// missing. A playbook the user deleted (hidden) is never offered. Errors (a table of a shape
/// this binary does not know) come back as an error the hook swallows; an index without
/// `issue_clusters` has none.
pub fn playbooks_for(
    conn: &Connection,
    signatures: &[String],
    project: &str,
) -> Result<Vec<PlaybookHit>> {
    if signatures.is_empty() || !crate::db::has_table(conn, "issue_clusters") {
        return Ok(Vec::new());
    }
    // Prepared before the fixes are read, so a table of another shape is an error rather
    // than a line built from a guess.
    let mut count_of = conn.prepare(CLUSTER_COUNT_SQL)?;
    let mut hit = conn.prepare(PLAYBOOK_HIT_SQL)?;
    let fixes = load_fixes(conn);
    let mut out = Vec::new();
    // A fix stored before its signature counted as generic (`patch does not apply`) is no
    // playbook either.
    for signature in signatures.iter().filter(|s| is_specific(s)) {
        let Some(fix) = fixes.get(signature).and_then(|p| p.get(project)) else {
            continue;
        };
        let Some(count) = count_of
            .query_row([signature], |r| r.get::<_, i64>(0))
            .optional()?
        else {
            continue;
        };
        let row = hit
            .query_row(
                rusqlite::params![fix.thread_id, fix.sha, playbook_ref(signature, project)],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((short, subject, text, hidden)) = row else {
            continue;
        };
        if hidden != 0 {
            continue;
        }
        let text = text.unwrap_or_else(|| {
            playbook_text(signature, count, &short, subject.as_deref(), fix.thread_id)
        });
        out.push(PlaybookHit {
            signature: signature.clone(),
            text,
        });
    }
    Ok(out)
}

/// The mining itself: the FTS candidate fetch, then extraction and grouping in Rust.
fn mine_clusters(
    conn: &Connection,
    project: Option<&str>,
    mode: ProjectMatch,
    since: i64,
    limit: usize,
    sightings: Sightings,
) -> Result<Vec<IssueCluster>> {
    Ok(mine(conn, project, mode, since, limit, sightings)?
        .into_iter()
        .map(|m| m.cluster)
        .collect())
}

/// Files kept per sighting for the resolver ([`Sighting::files`]).
const FILES_PER_SIGHTING: usize = 8;

/// One mined cluster with what the playbook resolver needs beyond the public shape.
pub(crate) struct Mined {
    pub cluster: IssueCluster,
    /// [`normalize`] of the example: the `issue_clusters` key.
    pub signature: String,
    /// Each session's latest sighting, newest first.
    pub sightings: Vec<Sighting>,
}

/// One session's sightings of a cluster.
pub(crate) struct Sighting {
    /// The session: a subagent's sightings are its parent session's.
    pub thread: i64,
    /// Its latest sighting (epoch secs).
    pub seen: i64,
    /// The session's project key (`COALESCE(project_key, project_path)`): fixes are per
    /// project.
    pub project: Option<String>,
    /// Files this session's own error lines of the cluster named (see
    /// [`extract_errors_with_files`]), as written. Not the cluster's: one signature merges
    /// errors in different files and repos.
    pub files: Vec<String>,
}

/// A cluster while [`mine`] reads candidates.
struct Acc {
    example: String,
    count: i64,
    /// session -> its latest sighting, its earliest, and the files its error lines named.
    threads: HashMap<i64, (i64, i64, Vec<String>)>,
    first_seen: i64,
    last_seen: i64,
    last_thread_id: i64,
}

/// File one candidate message's errors under `session`, but those `copied(signature)`
/// leaves out. The example and the last session follow the newest sighting, whichever arm
/// read it first.
fn accumulate(
    clusters: &mut HashMap<String, Acc>,
    text: &str,
    ts: i64,
    session: i64,
    copied: impl Fn(&str) -> bool,
) {
    for (err, files) in extract_errors_with_files(text) {
        let signature = normalize(&err);
        if copied(&signature) {
            continue;
        }
        let acc = match clusters.entry(signature) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => e.insert(Acc {
                example: err.clone(),
                count: 0,
                threads: HashMap::new(),
                first_seen: ts,
                last_seen: ts,
                last_thread_id: session,
            }),
        };
        acc.count += 1;
        let (latest, earliest, named) = acc.threads.entry(session).or_insert((ts, ts, Vec::new()));
        *latest = (*latest).max(ts);
        *earliest = (*earliest).min(ts);
        add_files(named, files);
        acc.first_seen = acc.first_seen.min(ts);
        if ts > acc.last_seen {
            acc.last_seen = ts;
            acc.last_thread_id = session;
            acc.example = err;
        }
    }
}

/// Add `files` to a session's named files, each once, up to [`FILES_PER_SIGHTING`].
fn add_files(named: &mut Vec<String>, files: Vec<String>) {
    for f in files {
        if named.len() < FILES_PER_SIGHTING && !named.contains(&f) {
            named.push(f);
        }
    }
}

/// Fold the clusters `later` read into `clusters` as if its rows had been read after theirs:
/// counts add up, each session keeps its latest sighting and its files in that order, and
/// the example and last session move only to a strictly newer sighting.
fn merge_clusters(clusters: &mut HashMap<String, Acc>, later: HashMap<String, Acc>) {
    for (signature, b) in later {
        let a = match clusters.entry(signature) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => {
                e.insert(b);
                continue;
            }
        };
        a.count += b.count;
        for (session, (seen, first, files)) in b.threads {
            let (latest, earliest, named) =
                a.threads
                    .entry(session)
                    .or_insert((seen, first, Vec::new()));
            *latest = (*latest).max(seen);
            *earliest = (*earliest).min(first);
            add_files(named, files);
        }
        a.first_seen = a.first_seen.min(b.first_seen);
        if b.last_seen > a.last_seen {
            a.last_seen = b.last_seen;
            a.last_thread_id = b.last_thread_id;
            a.example = b.example;
        }
    }
}

/// One candidate row: (text head + tail, ts, thread id, the thread's project key).
type Candidate = (String, i64, i64, Option<String>);

fn candidate(r: &rusqlite::Row<'_>) -> rusqlite::Result<Candidate> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
}

/// [`mine_clusters`] keeping every session's latest sighting and the files it named.
fn mine(
    conn: &Connection,
    project: Option<&str>,
    mode: ProjectMatch,
    since: i64,
    limit: usize,
    sightings: Sightings,
) -> Result<Vec<Mined>> {
    let mut clusters: HashMap<String, Acc> = HashMap::new();
    let mut projects: HashMap<i64, Option<String>> = HashMap::new();

    // Subagents are read first, apart. A subagent's final report comes back to its parent
    // as a top-level tool row that quotes its output, so one error a subagent hit counted
    // twice in one session and passed the `count >= 2` gate: after the re-mine of a real
    // index, 11 of 38 recorded fixes came from a single session. A parent's sighting of a
    // signature at or after its subagents' first one is taken for that copy and left out;
    // a parent that really hits it again later is undercounted, which is accepted. They
    // are folded in afterwards as if read after the top-level rows, as they were before.
    let mut subagents: HashMap<String, Acc> = HashMap::new();
    if sightings == Sightings::All {
        // Counts, projects and the resolver's commit windows are the parent session's.
        let parents = crate::indexer::subagent_parents(conn)?;
        let mut project_of = conn.prepare_cached(PROJECT_OF_SQL)?;
        let (sql, args) = candidates_sql(conn, project, mode, since, Arm::Subagents);
        let arg_refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let mut stmt = conn.prepare(&sql)?;
        for row in stmt.query_map(arg_refs.as_slice(), candidate)? {
            let (text, ts, subagent, own_project) = row?;
            let session = match parents.get(&subagent) {
                Some(&parent) => {
                    if let Entry::Vacant(e) = projects.entry(parent) {
                        e.insert(
                            project_of
                                .query_row([parent], |r| r.get(0))
                                .optional()?
                                .flatten(),
                        );
                    }
                    parent
                }
                None => {
                    projects.entry(subagent).or_insert(own_project);
                    subagent
                }
            };
            accumulate(&mut subagents, &text, ts, session, |_| false);
        }
    }

    let (sql, args) = candidates_sql(conn, project, mode, since, Arm::TopLevel);
    let arg_refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    for row in stmt.query_map(arg_refs.as_slice(), candidate)? {
        let (text, ts, thread_id, thread_project) = row?;
        projects.entry(thread_id).or_insert(thread_project);
        let copied = |signature: &str| {
            subagents
                .get(signature)
                .and_then(|a| a.threads.get(&thread_id))
                .is_some_and(|&(_, first, _)| ts >= first)
        };
        accumulate(&mut clusters, &text, ts, thread_id, copied);
    }
    merge_clusters(&mut clusters, subagents);

    let mut out: Vec<Mined> = clusters
        .into_iter()
        .filter(|(_, a)| a.count >= 2) // "recurring" = seen at least twice
        .map(|(signature, a)| {
            let mut sightings: Vec<Sighting> = a
                .threads
                .into_iter()
                .map(|(thread, (seen, _, files))| Sighting {
                    thread,
                    seen,
                    project: projects.get(&thread).cloned().flatten(),
                    files,
                })
                .collect();
            sightings.sort_by(|x, y| y.seen.cmp(&x.seen).then(y.thread.cmp(&x.thread)));
            Mined {
                cluster: IssueCluster {
                    example: a.example,
                    count: a.count,
                    threads: sightings.len() as i64,
                    first_seen: a.first_seen,
                    last_seen: a.last_seen,
                    last_thread_id: a.last_thread_id,
                    thread_ids: sightings
                        .iter()
                        .take(THREAD_IDS_CAP)
                        .map(|s| s.thread)
                        .collect(),
                },
                signature,
                sightings,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.cluster
            .count
            .cmp(&a.cluster.count)
            .then(b.cluster.last_seen.cmp(&a.cluster.last_seen))
    });
    out.truncate(limit);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_catches_real_errors_skips_prose() {
        let text = "Here's the build output:\n\
            error[E0599]: no method named `frobnicate` found for struct `Widget`\n\
            we should handle that error gracefully later\n\
            thread 'main' panicked at src/lib.rs:42:5\n\
            TypeError: Cannot read properties of undefined (reading 'x')\n\
            just some normal text about error handling in general";
        let errs = extract_errors(text);
        assert!(errs.iter().any(|e| e.starts_with("error[E0599]")));
        assert!(errs.iter().any(|e| e.contains("panicked at")));
        assert!(errs.iter().any(|e| e.starts_with("TypeError:")));
        // Prose mentioning "error" mid-sentence is NOT captured.
        assert!(!errs.iter().any(|e| e.contains("gracefully")));
        assert!(!errs.iter().any(|e| e.contains("normal text")));
    }

    #[test]
    fn normalize_groups_same_error_with_different_specifics() {
        // Same error, different identifier + path + line -> same key.
        let a = "error[E0599]: no method named `foo` found at /Users/me/src/a.rs:12:5";
        let b = "error[E0599]: no method named `bar` found at /home/x/src/b.rs:99:1";
        assert_eq!(normalize(a), normalize(b));
        // A genuinely different error does NOT collapse to the same key.
        let c = "error[E0277]: the trait bound `T: Clone` is not satisfied";
        assert_ne!(normalize(a), normalize(c));
    }

    #[test]
    fn finds_errors_at_both_ends_of_long_output() {
        let p = std::env::temp_dir().join(format!("calli_issues_{}.db", std::process::id()));
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
        let filler = "compiling some crate\n".repeat(2_000); // ~42 KB
        let body = format!(
            "error[E0425]: cannot find value `x` in this scope\n{filler}\
             error: could not compile `app` due to 1 previous error"
        );
        for seq in 0..2 {
            conn.execute(
                "INSERT INTO messages (thread_id, seq, role, text, ts) VALUES (?1, ?2, 'tool', ?3, 100)",
                rusqlite::params![tid, seq, body],
            )
            .unwrap();
        }
        let clusters = recurring_issues(&conn, None, 0, 10).unwrap();
        assert!(clusters
            .iter()
            .any(|c| c.example.starts_with("error[E0425]")));
        assert!(clusters
            .iter()
            .any(|c| c.example.contains("could not compile")));
        // The cached variant returns the same answer.
        let cached = recurring_issues_cached(&conn, None, 0, 10).unwrap();
        assert_eq!(cached.len(), clusters.len());
    }

    fn plan(conn: &Connection, sql: &str, args: &[Box<dyn ToSql>]) -> String {
        let refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let rows = stmt
            .query_map(refs.as_slice(), |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        rows.join(" | ")
    }

    /// The candidate scan must stay FTS-driven, with each hit's filters answered by the
    /// covering index (not a table read per hit), and must still run on an index that
    /// predates the covering index. The subagent arm picks its newest ids off that index
    /// alone and reads the text of those rows only, by rowid, never sorting it.
    #[test]
    fn candidate_scan_uses_the_covering_index_and_falls_back_without_it() {
        let p = std::env::temp_dir().join(format!("calli_issues_plan_{}.db", std::process::id()));
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        let conn = crate::db::open(&p).unwrap();
        for (project, mode) in [
            (None, ProjectMatch::Substring),
            (Some("proj"), ProjectMatch::Substring),
            (Some("/r/proj"), ProjectMatch::Exact),
        ] {
            let (sql, args) = candidates_sql(&conn, project, mode, 0, Arm::TopLevel);
            let top = plan(&conn, &sql, &args);
            assert!(top.starts_with("SCAN messages_fts VIRTUAL TABLE"), "{top}");
            assert!(
                top.contains("SEARCH m USING INDEX idx_messages_cover (id=?)"),
                "{top}"
            );
            let (sql, args) = candidates_sql(&conn, project, mode, 0, Arm::Subagents);
            let sub = plan(&conn, &sql, &args);
            assert!(
                sub.starts_with("SEARCH o USING INTEGER PRIMARY KEY (rowid=?)"),
                "{sub}"
            );
            assert!(sub.contains("LIST SUBQUERY"), "{sub}");
            assert!(sub.contains("SCAN messages_fts VIRTUAL TABLE"), "{sub}");
            // Covering: the subquery reads only id, thread_id, role and ts, so the ids come
            // off the index with no table read.
            assert!(
                sub.contains("SEARCH m USING COVERING INDEX idx_messages_cover (id=?)"),
                "{sub}"
            );
            // The Claude Code source id is looked up once, by its unique kind.
            assert!(
                sub.contains("SCALAR SUBQUERY") && sub.contains("SEARCH sources USING"),
                "{sub}"
            );
            assert!(
                sub.contains("SEARCH ot USING INTEGER PRIMARY KEY (rowid=?)"),
                "{sub}"
            );
            // The one sort is the subquery's, over ids: the text is streamed.
            assert_eq!(sub.matches("TEMP B-TREE").count(), 1, "{sub}");
        }

        conn.execute_batch("DROP INDEX idx_messages_cover").unwrap();
        for arm in [Arm::TopLevel, Arm::Subagents] {
            let (sql, _) = candidates_sql(&conn, None, ProjectMatch::Substring, 0, arm);
            assert!(!sql.contains("INDEXED BY"), "{sql}");
        }
        conn.execute(
            "INSERT INTO threads (source_id, external_id, title) VALUES (1, 't', 't')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        for seq in 0..2 {
            conn.execute(
                "INSERT INTO messages (thread_id, seq, role, text, ts)
                 VALUES (?1, ?2, 'tool', 'error: linker failed', 100)",
                rusqlite::params![tid, seq],
            )
            .unwrap();
        }
        assert_eq!(recurring_issues(&conn, None, 0, 10).unwrap().len(), 1);
    }

    /// The app's Project page scopes to an exact key, so a sibling repo whose path starts
    /// with it (`/r/app-web` beside `/r/app`) stays out; `cal issues -p` keeps the substring.
    #[test]
    fn the_app_scope_matches_the_project_key_exactly() {
        let p = std::env::temp_dir().join(format!("calli_issues_exact_{}.db", std::process::id()));
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        let conn = crate::db::open(&p).unwrap();
        for (ext, path, err) in [
            ("a", "/r/app", "error: linker failed"),
            ("w", "/r/app-web", "fatal: bad object HEAD"),
        ] {
            conn.execute(
                "INSERT INTO threads (source_id, external_id, title, project_path)
                 VALUES (1, ?1, 't', ?2)",
                [ext, path],
            )
            .unwrap();
            let tid = conn.last_insert_rowid();
            for seq in 0..2 {
                conn.execute(
                    "INSERT INTO messages (thread_id, seq, role, text, ts)
                     VALUES (?1, ?2, 'tool', ?3, 100)",
                    rusqlite::params![tid, seq, err],
                )
                .unwrap();
            }
        }
        assert_eq!(
            recurring_issues(&conn, Some("/r/app"), 0, 10)
                .unwrap()
                .len(),
            2
        );
        let exact =
            recurring_issues_matching(&conn, Some("/r/app"), ProjectMatch::Exact, 0, 10).unwrap();
        assert_eq!(exact.len(), 1, "{exact:?}");
        assert!(exact[0].example.contains("linker"));
        assert_eq!(
            recurring_issues_cached(&conn, Some("/r/app"), 0, 10)
                .unwrap()
                .len(),
            1
        );
    }

    fn clusters_db(tag: &str) -> Connection {
        let p = std::env::temp_dir().join(format!("calli_issues_{tag}_{}.db", std::process::id()));
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        crate::db::open(&p).unwrap()
    }

    /// A thread under /r/app (indexed at `indexed_at`) and an adder for its tool messages.
    fn app_thread(conn: &Connection, indexed_at: i64) -> i64 {
        conn.execute(
            "INSERT INTO threads (source_id, external_id, title, project_path, last_indexed_at)
             VALUES (1, 't', 't', '/r/app', ?1)",
            [indexed_at],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn add_msg(conn: &Connection, tid: i64, seq: i64, text: &str, ts: i64) {
        conn.execute(
            "INSERT INTO messages (thread_id, seq, role, text, ts) VALUES (?1, ?2, 'tool', ?3, ?4)",
            rusqlite::params![tid, seq, text, ts],
        )
        .unwrap();
    }

    #[test]
    fn refreshed_clusters_answer_unscoped_reads_while_fresh() {
        let conn = clusters_db("tbl");
        let now = chrono::Utc::now().timestamp();
        let tid = app_thread(&conn, now - 60);
        let add = |seq: i64, text: &str| add_msg(&conn, tid, seq, text, now - 60);
        add(0, "error: linker failed");
        add(1, "error: linker failed");
        assert_eq!(refresh_clusters_at(&conn, now).unwrap(), 1);
        // The resolver's columns survive the next refresh.
        conn.execute(
            "UPDATE issue_clusters SET resolved_sha = 'abc', resolved_thread_id = ?1",
            [tid],
        )
        .unwrap();
        add(2, "fatal: bad object HEAD");
        add(3, "fatal: bad object HEAD");
        assert_eq!(refresh_clusters_at(&conn, now).unwrap(), 2);
        let resolved: Option<String> = conn
            .query_row(
                "SELECT resolved_sha FROM issue_clusters WHERE signature = ?1",
                [normalize("error: linker failed")],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(resolved.as_deref(), Some("abc"));

        // Fresh: an unscoped 180-day read comes from the table, even after new sightings.
        add(4, "error: disk full");
        add(5, "error: disk full");
        let since = now - CLUSTER_WINDOW_SECS;
        assert_eq!(recurring_issues(&conn, None, since, 10).unwrap().len(), 2);
        // A scoped read, another window or a stale refresh mines live.
        assert_eq!(
            recurring_issues(&conn, Some("/r/app"), since, 10)
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            recurring_issues(&conn, None, now - 30 * 86_400, 10)
                .unwrap()
                .len(),
            3
        );
        // Stale and something indexed since (the thread above, re-stamped after the refresh).
        let old = now - CLUSTERS_FRESH_SECS - 1;
        crate::knowledge::app_config::put(&conn, CLUSTERS_REFRESHED_AT, &old.to_string()).unwrap();
        conn.execute("UPDATE issue_clusters SET refreshed_at = ?1", [old])
            .unwrap();
        assert_eq!(recurring_issues(&conn, None, since, 10).unwrap().len(), 3);
        // An index without the table (a reader on an older schema) mines live too.
        crate::knowledge::app_config::put(&conn, CLUSTERS_REFRESHED_AT, &now.to_string()).unwrap();
        conn.execute_batch("DROP TABLE issue_clusters").unwrap();
        assert_eq!(recurring_issues(&conn, None, since, 10).unwrap().len(), 3);
    }

    fn temp_db(tag: &str) -> Connection {
        let p = std::env::temp_dir().join(format!("calli_issues_{tag}_{}.db", std::process::id()));
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        crate::db::open(&p).unwrap()
    }

    fn thread(conn: &Connection, ext: &str, project: &str) -> i64 {
        conn.execute(
            "INSERT INTO threads (source_id, external_id, title, project_path, project_key)
             VALUES (1, ?1, ?1, ?2, ?2)",
            [ext, project],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn message(conn: &Connection, tid: i64, seq: i64, text: &str, ts: i64) {
        conn.execute(
            "INSERT INTO messages (thread_id, seq, role, text, ts) VALUES (?1, ?2, 'tool', ?3, ?4)",
            rusqlite::params![tid, seq, text, ts],
        )
        .unwrap();
    }

    fn edit(conn: &Connection, tid: i64, path: &str, op: &str, ts: i64) {
        conn.execute(
            "INSERT INTO file_edits (thread_id, message_id, path, op, ts)
             VALUES (?1, NULL, ?2, ?3, ?4)",
            rusqlite::params![tid, path, op, ts],
        )
        .unwrap();
    }

    fn commit(conn: &Connection, tid: i64, sha: &str, subject: &str, at: i64) {
        conn.execute(
            "INSERT INTO thread_commits
                (thread_id, sha, short_sha, subject, committed_at, overlap, created_at)
             VALUES (?1, ?2, substr(?2, 1, 7), ?3, ?4, 1, ?4)",
            rusqlite::params![tid, sha, subject, at],
        )
        .unwrap();
    }

    fn playbooks(conn: &Connection) -> Vec<(i64, String, String, i64)> {
        conn.prepare(
            "SELECT thread_id, text, uid, pinned FROM facts
             WHERE extractor = 'playbook' AND kind = 'gotcha' ORDER BY id",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    }

    const BORROW: &str = "error[E0502]: cannot borrow `self.items` as mutable";

    fn borrow_error(file: &str) -> String {
        format!("{BORROW}\n  --> {file}:12:9\n   |")
    }

    #[test]
    fn errors_carry_the_files_they_name() {
        let text = "error[E0502]: cannot borrow `x` as mutable\n  --> src/lib.rs:42:5\n\
                    Traceback (most recent call last):\n  File \"/app/main.py\", line 3, in <module>\n\
                    ValueError: bad input e.g. version 1.2.3\n\
                    TypeError: x is undefined\n    at render (./web/app.tsx:10:2)\n\
                    src/cart.ts(3,5): error TS2322: Type 'string' is not assignable to type 'number'.";
        let found = extract_errors_with_files(text);
        let files = |prefix: &str| {
            found
                .iter()
                .find(|(l, _)| l.starts_with(prefix))
                .map(|(_, f)| f.clone())
                .unwrap()
        };
        assert!(files("error[E0502]").contains(&"src/lib.rs".to_string()));
        assert!(files("ValueError").contains(&"/app/main.py".to_string()));
        assert!(!files("ValueError")
            .iter()
            .any(|f| f == "e.g" || f == "1.2.3"));
        assert!(files("TypeError").contains(&"./web/app.tsx".to_string()));
        // tsc's plain location format, the one a non-TTY shell gets.
        assert!(files("src/cart.ts(3,5)").contains(&"src/cart.ts".to_string()));
        assert_eq!(file_token("a.ts(12)"), Some("a.ts".into()));
        assert_eq!(file_token("Cargo.toml:"), Some("Cargo.toml".into()));
        assert_eq!(file_token("https://x.dev/a.js"), None);
        assert_eq!(file_token("1.2.3"), None);
        assert_eq!(file_token("0.03s"), None);
        assert_eq!(file_token("12.5s"), None);
        assert_eq!(file_token("v1.ts"), Some("v1.ts".into()));

        assert_eq!(path_match_len("/Users/me/repo/src/lib.rs", "src/lib.rs"), 2);
        assert_eq!(path_match_len("./src/lib.rs", "src/lib.rs"), 2);
        assert_eq!(path_match_len("lib.rs", "src/lib.rs"), 1);
        assert_eq!(path_match_len("src/mylib.rs", "lib.rs"), 0);
        assert_eq!(path_match_len("src/lib.rs", "src/main.rs"), 0);
        assert_eq!(
            path_match_len("core/src/lib.rs", "standalone/src/lib.rs"),
            0
        );
        // The longest shared tail wins: `src/lib.rs` is the root crate's, not core's.
        let edits = vec![
            ("core/src/lib.rs".to_string(), 10),
            ("src/lib.rs".to_string(), 20),
            ("tests/unit/test_a.py".to_string(), 30),
            ("legacy/test_a.py".to_string(), 40),
            ("src/old.rs".to_string(), 1),
        ];
        let named = |f: &str| erroring_edits(&[f.to_string()], &edits, 5);
        assert_eq!(named("/r/app/src/lib.rs"), vec![("src/lib.rs", 20)]);
        assert_eq!(
            named("lib.rs"),
            vec![("core/src/lib.rs", 10), ("src/lib.rs", 20)]
        );
        assert_eq!(named("unit/test_a.py"), vec![("tests/unit/test_a.py", 30)]);
        assert!(named("src/old.rs").is_empty(), "edited before the sighting");
        // A commit's root-relative path against an edit's spelling.
        assert!(commit_touches("src/a.rs", "src/a.rs"));
        assert!(commit_touches("apps/x/src/a.rs", "src/a.rs"));
        assert!(commit_touches("src/a.rs", "/Users/me/wt/src/a.rs"));
        assert!(!commit_touches("README.md", "docs/README.md"));
        assert!(!commit_touches("src/b.rs", "src/a.rs"));

        for generic in [
            "error:",
            "error during build:",
            "traceback most recent call last):",
            "error: process completed with exit code 1.",
            "keyerror: '…'",
            "ugrep: warning: <path> no such file or directory",
            "error: could not compile '…' lib due to 2 previous errors",
            "error: aborting due to 2 previous errors",
            "npm err! code elifecycle",
            "error: command failed: '…'",
            &normalize("error: test failed, to rerun pass `-p app-core --lib`"),
            &normalize("error: run failed: command exited (1)"),
            &normalize("error: patch failed: src/a.rs:12\nerror: src/a.rs: patch does not apply"),
        ] {
            assert!(!is_specific(generic), "{generic}");
        }
        for specific in [
            "error: cannot find module <path>",
            "<path> error ts2304: cannot find name '…'",
            &normalize("src/cart.ts(3,5): error TS2322: Type 'string' is not assignable to type"),
            &normalize(BORROW),
        ] {
            assert!(is_specific(specific), "{specific}");
        }
    }

    /// The fix is the latest sighting whose session then edited the named file and committed
    /// after the edit; a later sighting with no commit, or a commit from before the edit,
    /// does not count. The playbook fact is recorded once and refreshed in place.
    #[test]
    fn resolution_picks_the_fixing_session_and_the_playbook_refreshes_in_place() {
        let conn = temp_db("resolve");
        let now = chrono::Utc::now().timestamp();
        let t0 = now - 10 * 86_400;
        let early = thread(&conn, "early", "/r/app");
        let fixer = thread(&conn, "fixer", "/r/app");
        let later = thread(&conn, "later", "/r/app");
        let stale = thread(&conn, "stale", "/r/app");
        for (sha, files) in [
            ("aaaaaaaaaaaa", &["src/cart.rs"][..]),
            ("bbbbbbbbbbbb", &["Cargo.lock", "src/cart.rs"]),
            ("cccccccccccc", &["src/cart.rs"]),
            ("dddddddddddd", &["src/cart.rs"]),
            ("eeeeeeeeeeee", &["src/cart.rs"]),
        ] {
            seed_commit_files(sha, files);
        }
        // early: hit it, edited the file, committed: a fix, but an older one.
        message(&conn, early, 0, &borrow_error("src/cart.rs"), t0);
        edit(&conn, early, "src/cart.rs", "edit", t0 + 60);
        commit(&conn, early, "aaaaaaaaaaaa", "fix: first try", t0 + 120);
        // fixer: hit it again, edited, committed.
        message(
            &conn,
            fixer,
            0,
            &borrow_error("/home/me/r/app/src/cart.rs"),
            t0 + 1_000,
        );
        edit(&conn, fixer, "src/cart.rs", "read", t0 + 1_010);
        edit(&conn, fixer, "src/cart.rs", "edit", t0 + 1_060);
        commit(
            &conn,
            fixer,
            "bbbbbbbbbbbb",
            "fix: clone the items first",
            t0 + 1_100,
        );
        commit(
            &conn,
            fixer,
            "cccccccccccc",
            "chore: later work",
            t0 + 5_000,
        );
        // stale: its only commit predates its edit.
        message(&conn, stale, 0, &borrow_error("src/cart.rs"), t0 + 2_000);
        commit(&conn, stale, "dddddddddddd", "unrelated", t0 + 1_900);
        edit(&conn, stale, "src/cart.rs", "edit", t0 + 2_050);
        // later: the newest sighting, never committed.
        message(&conn, later, 0, &borrow_error("src/cart.rs"), t0 + 3_000);
        edit(&conn, later, "src/cart.rs", "edit", t0 + 3_010);

        assert_eq!(refresh_clusters_at(&conn, now).unwrap(), 1);
        let (thread_id, sha): (i64, String) = conn
            .query_row(
                "SELECT resolved_thread_id, resolved_sha FROM issue_clusters",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((thread_id, sha.as_str()), (fixer, "bbbbbbbbbbbb"));
        let facts = playbooks(&conn);
        assert_eq!(facts.len(), 1, "{facts:?}");
        let sig = normalize(BORROW);
        assert_eq!(
            facts[0].1,
            format!(
                "{sig}: seen 4 times; fixed in bbbbbbb (fix: clone the items first), see thread {fixer}"
            )
        );
        assert_eq!(
            (facts[0].0, facts[0].3),
            (fixer, 0),
            "filed under the fix, unpinned"
        );

        // Another sighting: the same fact, same uid, new count; no duplicate.
        message(&conn, later, 1, &borrow_error("src/cart.rs"), t0 + 4_000);
        refresh_clusters_at(&conn, now).unwrap();
        let again = playbooks(&conn);
        assert_eq!(again.len(), 1, "{again:?}");
        assert_eq!(again[0].2, facts[0].2, "the uid survives the refresh");
        assert!(again[0].1.contains("seen 5 times"), "{}", again[0].1);
        let embedded: i64 = conn
            .query_row(
                "SELECT embedded FROM facts WHERE extractor = 'playbook'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(embedded, 0, "changed text is re-embedded");

        // A later session that fixes it moves the playbook there, in place.
        commit(
            &conn,
            later,
            "eeeeeeeeeeee",
            "fix: drop the borrow",
            t0 + 4_100,
        );
        edit(&conn, later, "src/cart.rs", "edit", t0 + 4_050);
        refresh_clusters_at(&conn, now).unwrap();
        let moved = playbooks(&conn);
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].0, later);
        assert!(
            moved[0].1.contains("eeeeeee (fix: drop the borrow)"),
            "{}",
            moved[0].1
        );
        assert_eq!(moved[0].2, facts[0].2);

        // A playbook the user edited is theirs: refreshes leave it.
        conn.execute(
            "UPDATE facts SET text = 'clone before borrowing', edited = 1
             WHERE extractor = 'playbook'",
            [],
        )
        .unwrap();
        message(&conn, later, 2, &borrow_error("src/cart.rs"), t0 + 4_000);
        refresh_clusters_at(&conn, now).unwrap();
        assert_eq!(playbooks(&conn)[0].1, "clone before borrowing");

        // Readers get the fix, the sessions and the playbook; another project gets no fix.
        let found = recurring_issues_with_fixes(
            &conn,
            Some("/r/app"),
            ProjectMatch::Exact,
            now - CLUSTER_WINDOW_SECS,
            10,
        )
        .unwrap();
        assert_eq!(found.len(), 1);
        let fix = found[0].fix.as_ref().expect("a fix");
        assert_eq!((fix.thread_id, fix.short_sha.as_str()), (later, "eeeeeee"));
        assert_eq!(fix.subject.as_deref(), Some("fix: drop the borrow"));
        assert_eq!(found[0].playbook.as_deref(), Some("clone before borrowing"));
        assert_eq!(found[0].thread_ids, vec![later, stale, fixer, early]);
        let stored = recurring_issues_with_fixes(
            &conn,
            None,
            ProjectMatch::Exact,
            now - CLUSTER_WINDOW_SECS,
            10,
        )
        .unwrap();
        assert_eq!(
            stored[0].thread_ids, found[0].thread_ids,
            "stored rows keep them"
        );
        assert!(stored[0].fix.is_some());
        let elsewhere = with_fixes(
            &conn,
            vec![found[0].cluster.clone()],
            Some("/r/other"),
            ProjectMatch::Exact,
        );
        assert!(elsewhere[0].fix.is_none() && elsewhere[0].playbook.is_none());

        // The hook's lookup: this project only, and never a playbook the user deleted.
        let hits = playbooks_for(&conn, std::slice::from_ref(&sig), "/r/app").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text, "clone before borrowing");
        assert!(playbooks_for(&conn, std::slice::from_ref(&sig), "/r/other")
            .unwrap()
            .is_empty());
        conn.execute(
            "UPDATE facts SET hidden = 1 WHERE extractor = 'playbook'",
            [],
        )
        .unwrap();
        assert!(playbooks_for(&conn, std::slice::from_ref(&sig), "/r/app")
            .unwrap()
            .is_empty());
        assert!(found_without_table(&conn, &sig));
    }

    /// Fixes are per project: a later fix in another repo leaves the first repo's fix, its
    /// playbook fact and its hook line alone, and unscoped reads show the fix of the project
    /// where the error was seen last. A sighting counts only against the files its own error
    /// lines named, and only a linked commit that changed that file is a fix.
    #[test]
    fn each_project_keeps_its_own_fix_and_only_a_commit_touching_the_file_counts() {
        let conn = temp_db("per_project");
        let now = chrono::Utc::now().timestamp();
        let t0 = now - 10 * 86_400;
        let a = thread(&conn, "a", "/r/a");
        let b = thread(&conn, "b", "/r/b");
        let c = thread(&conn, "c", "/r/a");
        seed_commit_files("a0a0a0a0a0a0", &["README.md"]);
        seed_commit_files("a1a1a1a1a1a1", &["src/parser.rs"]);
        seed_commit_files("b0b0b0b0b0b0", &["src/main.rs"]);
        seed_commit_files("c0c0c0c0c0c0", &["src/main.rs"]);
        // Repo A: the first linked commit after the edit only touched the README (a link by
        // mention); the next one changed the erroring file and is the fix.
        message(&conn, a, 0, &borrow_error("src/parser.rs"), t0);
        message(&conn, a, 1, &borrow_error("src/parser.rs"), t0 + 1);
        edit(&conn, a, "src/parser.rs", "edit", t0 + 60);
        edit(&conn, a, "README.md", "edit", t0 + 70);
        commit(&conn, a, "a0a0a0a0a0a0", "docs: readme", t0 + 100);
        commit(&conn, a, "a1a1a1a1a1a1", "fix: parser borrow", t0 + 200);
        // Repo B fixes the same signature later, in its own file.
        message(&conn, b, 0, &borrow_error("src/main.rs"), t0 + 1_000);
        message(&conn, b, 1, &borrow_error("src/main.rs"), t0 + 1_000 + 1);
        edit(&conn, b, "src/main.rs", "edit", t0 + 1_060);
        commit(&conn, b, "b0b0b0b0b0b0", "fix: main borrow", t0 + 1_100);
        // A later session in A hit it in parser.rs, then edited and committed main.rs for
        // another reason: B's file, not its own, so no fix.
        message(&conn, c, 0, &borrow_error("src/parser.rs"), t0 + 2_000);
        message(&conn, c, 1, &borrow_error("src/parser.rs"), t0 + 2_000 + 1);
        edit(&conn, c, "src/main.rs", "edit", t0 + 2_060);
        commit(&conn, c, "c0c0c0c0c0c0", "refactor: main", t0 + 2_100);

        refresh_clusters_at(&conn, now).unwrap();
        let sig = normalize(BORROW);
        let fixes = load_fixes(&conn);
        let per = &fixes[&sig];
        assert_eq!(per.len(), 2, "{per:?}");
        assert_eq!(
            (per["/r/a"].thread_id, per["/r/a"].sha.as_str()),
            (a, "a1a1a1a1a1a1")
        );
        assert_eq!(
            (per["/r/b"].thread_id, per["/r/b"].sha.as_str()),
            (b, "b0b0b0b0b0b0")
        );
        // Two playbook facts, each filed under its own project's fixing session.
        let facts = playbooks(&conn);
        assert_eq!(facts.len(), 2, "{facts:?}");
        assert!(facts
            .iter()
            .any(|f| f.0 == a && f.1.contains("a1a1a1a (fix: parser borrow)")));
        assert!(facts
            .iter()
            .any(|f| f.0 == b && f.1.contains("b0b0b0b (fix: main borrow)")));

        let read = |project: Option<&str>| {
            recurring_issues_with_fixes(
                &conn,
                project,
                ProjectMatch::Exact,
                now - CLUSTER_WINDOW_SECS,
                10,
            )
            .unwrap()
        };
        let in_a = read(Some("/r/a"));
        assert_eq!(in_a[0].fix.as_ref().unwrap().short_sha, "a1a1a1a");
        assert!(in_a[0].playbook.as_deref().unwrap().contains("a1a1a1a"));
        let in_b = read(Some("/r/b"));
        assert_eq!(in_b[0].fix.as_ref().unwrap().short_sha, "b0b0b0b");
        // Unscoped: the latest sighting is c's, in repo A.
        let all = read(None);
        assert_eq!(all[0].fix.as_ref().unwrap().thread_id, a);
        let (resolved,): (i64,) = conn
            .query_row("SELECT resolved_thread_id FROM issue_clusters", [], |r| {
                Ok((r.get(0)?,))
            })
            .unwrap();
        assert_eq!(resolved, a);
        // A substring scope (`cal issues -p`, MCP) picks the newest fix among its projects.
        let wide = with_fixes(
            &conn,
            vec![all[0].cluster.clone()],
            Some("/R/"),
            ProjectMatch::Substring,
        );
        assert_eq!(wide[0].fix.as_ref().unwrap().thread_id, b);
        // The hook: each repo hears its own fix.
        let hit = |p: &str| playbooks_for(&conn, std::slice::from_ref(&sig), p).unwrap();
        assert!(hit("/r/a")[0].text.contains("a1a1a1a"));
        assert!(hit("/r/b")[0].text.contains("b0b0b0b"));

        // A's fix outlives its sightings (a refresh that no longer finds it keeps it), and a
        // fix whose commit link is gone is dropped.
        conn.execute("DELETE FROM messages WHERE thread_id = ?1", [a])
            .unwrap();
        conn.execute(
            "DELETE FROM thread_commits WHERE thread_id = ?1 AND sha = 'b0b0b0b0b0b0'",
            [b],
        )
        .unwrap();
        refresh_clusters_at(&conn, now).unwrap();
        let fixes = load_fixes(&conn);
        let kept: Vec<&String> = fixes[&sig].keys().collect();
        assert_eq!(kept, vec!["/r/a"]);
        assert_eq!(read(Some("/r/a"))[0].fix.as_ref().unwrap().thread_id, a);
        assert!(read(Some("/r/b"))[0].fix.is_none());
    }

    /// Stores a fix of `signature` in `project` by `tid`'s commit `sha`, as an earlier
    /// refresh did: the cluster row (resolved, last seen at `last_seen`), the fix in
    /// [`CLUSTER_FIXES`] and the playbook fact.
    fn stored_fix(
        conn: &Connection,
        tid: i64,
        project: &str,
        example: &str,
        sha: &str,
        last_seen: i64,
    ) {
        let signature = normalize(example);
        commit(conn, tid, sha, "fix: it", last_seen);
        conn.execute(
            "INSERT INTO issue_clusters (signature, example, count, threads, first_seen,
                last_seen, last_thread_id, refreshed_at, resolved_thread_id, resolved_sha)
             VALUES (?1, ?2, 3, 1, 0, ?3, ?4, 1, ?4, ?5)",
            rusqlite::params![signature, example, last_seen, tid, sha],
        )
        .unwrap();
        let mut fixes = load_fixes(conn);
        fixes.entry(signature.clone()).or_default().insert(
            project.to_string(),
            Resolution {
                thread_id: tid,
                sha: sha.to_string(),
                committed_at: last_seen,
            },
        );
        crate::knowledge::app_config::put(
            conn,
            CLUSTER_FIXES,
            &serde_json::to_string(&fixes).unwrap(),
        )
        .unwrap();
        let origin = playbook_ref(&signature, project);
        crate::knowledge::upsert_playbook(conn, &origin, tid, &signature, last_seen).unwrap();
    }

    fn playbook_refs(conn: &Connection) -> Vec<String> {
        conn.prepare("SELECT origin_ref FROM facts WHERE extractor = 'playbook' ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// A fix recorded under older rules for a line the current ones reject (a grep hit on a
    /// source line) loses its fix, its row's resolution and its uncurated playbook on the
    /// next refresh, while a pinned playbook of it stays. A fix of a real error that this
    /// refresh simply did not mine (aged out of the window) keeps all three.
    #[test]
    fn a_fix_of_a_line_the_rules_no_longer_take_is_dropped() {
        let conn = crate::db::test_support::TestIndex::new("issues_retired");
        let now = chrono::Utc::now().timestamp();
        let tid = thread(&conn, "t", "/r/app");
        let code = "src/api/client.ts:88:      error: 'Unable to refresh the session token',";
        let retired = normalize(code);
        assert!(is_specific(&retired) && is_error_line(code) && reads_as_code(code));
        stored_fix(
            &conn,
            tid,
            "/r/app",
            code,
            "c0dec0dec0de",
            now - 30 * 86_400,
        );
        stored_fix(
            &conn,
            tid,
            "/r/app",
            BORROW,
            "f1f1f1f1f1f1",
            now - CLUSTER_WINDOW_SECS - 10,
        );
        let real = normalize(BORROW);
        let pinned = playbook_ref(&retired, "/r/pinned");
        crate::knowledge::upsert_playbook(&conn, &pinned, tid, "keep this one", now).unwrap();
        conn.execute(
            "UPDATE facts SET pinned = 1 WHERE origin_ref = ?1",
            [&pinned],
        )
        .unwrap();

        refresh_clusters_at(&conn, now).unwrap();
        let fixes = load_fixes(&conn);
        assert_eq!(fixes.keys().collect::<Vec<_>>(), [&real]);
        let resolved = |sig: &str| -> Option<String> {
            conn.query_row(
                "SELECT resolved_sha FROM issue_clusters WHERE signature = ?1",
                [sig],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(resolved(&retired), None);
        assert_eq!(resolved(&real).as_deref(), Some("f1f1f1f1f1f1"));
        assert_eq!(
            playbook_refs(&conn),
            [playbook_ref(&real, "/r/app"), pinned]
        );
    }

    /// A project key can hold an `@` (a cloud-storage folder named after an email address):
    /// a playbook of a stored fix is judged by that fix's signature, not by a split at the
    /// last `@`, which cut `typeerror: failed to fetch` down to a generic one and deleted
    /// its playbook on every refresh.
    #[test]
    fn a_playbook_in_a_project_key_with_an_at_is_kept() {
        let conn = crate::db::test_support::TestIndex::new("issues_at_key");
        let now = chrono::Utc::now().timestamp();
        let project = "/r/Drive-me@example.com/app";
        let tid = thread(&conn, "t", project);
        let example = "TypeError: Failed to fetch";
        let signature = normalize(example);
        let origin = playbook_ref(&signature, project);
        assert!(is_specific(&signature));
        assert!(
            !is_specific(playbook_signature(&origin).unwrap()),
            "the guess"
        );
        stored_fix(&conn, tid, project, example, "a7a7a7a7a7a7", now - 86_400);
        refresh_clusters_at(&conn, now).unwrap();
        assert_eq!(playbook_refs(&conn), [origin]);
    }

    /// A commit git cannot vouch for (no checkout of the session on this machine, nothing
    /// cached) resolves nothing.
    #[test]
    fn a_commit_with_no_checkout_resolves_nothing() {
        let conn = temp_db("no_checkout");
        let now = chrono::Utc::now().timestamp();
        let tid = thread(&conn, "t", "/nonexistent/callimachus/app");
        message(&conn, tid, 0, &borrow_error("src/a.rs"), now - 600);
        message(&conn, tid, 1, &borrow_error("src/a.rs"), now - 500);
        edit(&conn, tid, "src/a.rs", "edit", now - 400);
        commit(&conn, tid, "9e9e9e9e9e9e", "fix: a", now - 300);
        refresh_clusters_at(&conn, now).unwrap();
        assert!(load_fixes(&conn).is_empty());
        assert!(playbooks(&conn).is_empty());
    }

    /// The files of a real commit, read with `git show` in the session's checkout.
    #[test]
    fn commit_files_come_from_git_show() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let repo = std::env::temp_dir().join(format!("calli_issues_git_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(repo.join("src")).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&["init", "-q"]);
        std::fs::write(repo.join("src/cart.rs"), "fn a() {}").unwrap();
        std::fs::write(repo.join("src/caf\u{e9}.rs"), "fn b() {}").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "one"]);
        let sha = git(&["rev-parse", "HEAD"]);
        let dir = repo.to_string_lossy().into_owned();
        let mut files = CommitFiles { git_left: 1 };
        let mut got = files.files(Some(&dir), &sha).unwrap();
        got.sort();
        assert_eq!(
            got,
            vec!["src/caf\u{e9}.rs".to_string(), "src/cart.rs".to_string()]
        );
        // Cached: no budget needed the second time, and no checkout either.
        assert_eq!(files.files(None, &sha).unwrap().len(), 2);
        // Spent budget or no checkout: unknown. A sha git cannot show there: changed nothing.
        assert_eq!(files.files(Some(&dir), "0123456789ab"), None);
        let mut more = CommitFiles { git_left: 1 };
        assert_eq!(more.files(Some(&dir), "0123456789ab"), Some(Vec::new()));
        assert_eq!(more.files(Some(&dir), "--help"), Some(Vec::new()));
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// An index without the 0034 tables (a reader on an older schema) resolves nothing and
    /// returns no playbook, without an error.
    fn found_without_table(conn: &Connection, sig: &str) -> bool {
        conn.execute_batch("DROP TABLE issue_clusters").unwrap();
        playbooks_for(conn, &[sig.to_string()], "/r/app")
            .unwrap()
            .is_empty()
    }

    /// An error that names no file, or whose file the session never edited, stays open.
    #[test]
    fn errors_without_an_edited_file_stay_unresolved() {
        let conn = temp_db("unresolved");
        let now = chrono::Utc::now().timestamp();
        let tid = thread(&conn, "t", "/r/app");
        message(&conn, tid, 0, "error: linker `cc` failed", now - 600);
        message(&conn, tid, 1, "error: linker `cc` failed", now - 500);
        message(&conn, tid, 2, &borrow_error("src/a.rs"), now - 400);
        message(&conn, tid, 3, &borrow_error("src/a.rs"), now - 300);
        edit(&conn, tid, "src/b.rs", "edit", now - 200);
        commit(&conn, tid, "ffffffffffff", "fix: something else", now - 100);
        assert_eq!(refresh_clusters_at(&conn, now).unwrap(), 2);
        let resolved: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM issue_clusters WHERE resolved_sha IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(resolved, 0);
        assert!(playbooks(&conn).is_empty());
    }

    /// The resolver's lookups stay index probes: the linked-thread list reads the covering
    /// thread index, and each session's edits and commits are searched by thread.
    #[test]
    fn resolver_lookups_use_their_indexes() {
        let conn = temp_db("resolve_plan");
        let one: Vec<Box<dyn ToSql>> = vec![Box::new(1_i64)];
        let plan_of = |sql: &str| plan(&conn, sql, &one);
        let p = plan(&conn, LINKED_THREADS_SQL, &[]);
        assert!(p.contains("USING COVERING INDEX"), "{p}");
        let p = plan_of(THREAD_EDITS_SQL);
        assert!(
            p.contains("SEARCH file_edits USING INDEX idx_file_edits_thread"),
            "{p}"
        );
        let p = plan_of(THREAD_COMMITS_SQL);
        assert!(p.starts_with("SEARCH thread_commits USING INDEX"), "{p}");
        let p = plan_of(THREAD_DIRS_SQL);
        assert!(
            p.contains("SEARCH threads USING INTEGER PRIMARY KEY"),
            "{p}"
        );
        let p = plan_of(PROJECT_OF_SQL);
        assert!(
            p.contains("SEARCH threads USING INTEGER PRIMARY KEY"),
            "{p}"
        );
        let sig: Vec<Box<dyn ToSql>> = vec![Box::new("sig".to_string())];
        let p = plan(&conn, LATEST_PROJECT_SQL, &sig);
        assert!(
            p.contains("SEARCH c USING INDEX sqlite_autoindex_issue_clusters_1 (signature=?)"),
            "{p}"
        );
        let p = plan(&conn, CLUSTER_COUNT_SQL, &sig);
        assert!(
            p.contains("sqlite_autoindex_issue_clusters_1 (signature=?)"),
            "{p}"
        );
        let link: Vec<Box<dyn ToSql>> = vec![
            Box::new(1_i64),
            Box::new("abc".to_string()),
            Box::new("playbook:sig@/r/app".to_string()),
        ];
        let p = plan(&conn, LINK_EXISTS_SQL, &link[..2]);
        assert!(p.contains("(thread_id=? AND sha=?)"), "{p}");
        for sql in [FIX_DETAIL_SQL, PLAYBOOK_HIT_SQL, PLAYBOOK_SOURCE_SQL] {
            let p = plan(&conn, sql, &link);
            assert!(p.contains("SEARCH tc USING"), "{p}");
            assert!(p.contains("(thread_id=? AND sha=?)"), "{p}");
            assert!(!p.contains("SCAN"), "{p}");
        }
        for sql in [FIX_DETAIL_SQL, PLAYBOOK_HIT_SQL] {
            let p = plan(&conn, sql, &link);
            assert!(
                p.contains("SEARCH f USING INDEX idx_facts_origin_ref"),
                "{p}"
            );
        }
        // Runs inside the refresh's write transaction, so it reads the playbook range of
        // the origin_ref index, never every fact.
        let p = plan(&conn, MACHINE_PLAYBOOKS_SQL, &[]);
        assert!(
            p.contains(
                "SEARCH facts USING INDEX idx_facts_origin_ref (origin_ref>? AND origin_ref<?)"
            ),
            "{p}"
        );
    }

    /// Measures resolution on a real index. Run the test binary directly against a disposable
    /// copy (never the live index): `CALLIMACHUS_DB=/path/to/copy.db <test binary>
    /// playbooks_on_a_real_index --ignored --nocapture`. It links the commits of the projects
    /// the clusters' sessions ran in (`git log` in those checkouts, read-only) and fills
    /// `file_edits` for the linked sessions, as a reindex would, then times the refresh.
    #[test]
    #[ignore]
    fn playbooks_on_a_real_index() {
        let path = std::env::var("CALLIMACHUS_DB").expect("CALLIMACHUS_DB names a copy");
        let conn = crate::db::open(std::path::Path::new(&path)).unwrap();
        let now = chrono::Utc::now().timestamp();
        let t = std::time::Instant::now();
        let mined = mine(
            &conn,
            None,
            ProjectMatch::Exact,
            now - CLUSTER_WINDOW_SECS,
            500,
            Sightings::All,
        )
        .unwrap();
        let with_files = mined
            .iter()
            .filter(|m| m.sightings.iter().any(|s| !s.files.is_empty()))
            .count();
        eprintln!(
            "mined {} clusters ({with_files} naming a file) in {:?}",
            mined.len(),
            t.elapsed()
        );
        let mut projects: HashMap<String, usize> = HashMap::new();
        for s in mined.iter().flat_map(|m| &m.sightings) {
            if let (false, Some(p)) = (s.files.is_empty(), &s.project) {
                *projects.entry(p.clone()).or_default() += 1;
            }
        }
        let mut projects: Vec<(String, usize)> = projects.into_iter().collect();
        projects.sort_by_key(|p| std::cmp::Reverse(p.1));
        let t = std::time::Instant::now();
        let mut links = 0;
        for (p, _) in projects.iter().take(15) {
            if std::path::Path::new(p).join(".git").exists() {
                links += crate::gitlink::link_project(&conn, p).unwrap_or(0);
            }
        }
        eprintln!("linked {links} thread commits in {:?}", t.elapsed());
        let linked: Vec<(i64, Option<String>)> = conn
            .prepare(
                "SELECT DISTINCT t.id, t.project_path FROM thread_commits tc
                 JOIN threads t ON t.id = tc.thread_id",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let t = std::time::Instant::now();
        conn.execute_batch("BEGIN").unwrap();
        for (id, project) in &linked {
            let msgs: Vec<(i64, String, String, Option<i64>)> = conn
                .prepare(
                    "SELECT id, tool_name, text, ts FROM messages
                     WHERE thread_id = ?1 AND tool_name IS NOT NULL ORDER BY seq",
                )
                .unwrap()
                .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            let rows: Vec<crate::indexer::file_edits::ToolRow> = msgs
                .iter()
                .map(
                    |(mid, name, text, ts)| crate::indexer::file_edits::ToolRow {
                        id: Some(*mid),
                        tool_name: Some(name.as_str()),
                        text: text.as_str(),
                        ts: *ts,
                        cwd: None,
                    },
                )
                .collect();
            let cwds = crate::indexer::file_edits::Anchors::cwds_of(&rows);
            let anchors = crate::indexer::file_edits::Anchors::resolve(
                project.as_deref(),
                cwds.iter().map(String::as_str),
            );
            crate::indexer::file_edits::derive(&conn, *id, &anchors, &rows, false).unwrap();
        }
        conn.execute_batch("COMMIT").unwrap();
        eprintln!(
            "file_edits for {} linked sessions in {:?}",
            linked.len(),
            t.elapsed()
        );
        let t = std::time::Instant::now();
        let fixes = resolve(&conn, &mined).unwrap();
        let shown = || COMMIT_FILES.lock().unwrap().len();
        eprintln!(
            "resolve: fixes for {} signatures ({} projects' fixes) in {:?}, {} commits shown",
            fixes.len(),
            fixes.values().map(BTreeMap::len).sum::<usize>(),
            t.elapsed(),
            shown()
        );
        for _ in 0..2 {
            let t = std::time::Instant::now();
            let n = refresh_clusters(&conn).unwrap();
            eprintln!(
                "refresh_clusters with resolution: {n} clusters in {:?}, {} commits shown",
                t.elapsed(),
                shown()
            );
        }
        let playbooks: Vec<String> = conn
            .prepare("SELECT text FROM facts WHERE extractor = 'playbook' ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        eprintln!("{} playbook facts", playbooks.len());
        for p in playbooks.iter().take(8) {
            eprintln!("  {p}");
        }
        let t = std::time::Instant::now();
        let found = recurring_issues_with_fixes(
            &conn,
            None,
            ProjectMatch::Exact,
            now - CLUSTER_WINDOW_SECS,
            50,
        )
        .unwrap();
        eprintln!(
            "stored read with fixes: {} rows, {} fixed, in {:?}",
            found.len(),
            found.iter().filter(|i| i.fix.is_some()).count(),
            t.elapsed()
        );
    }

    /// The precision gate on a real index, read-only (it never migrates, so a clone at an
    /// older schema is read as it is): what a refresh would mine over the standard window,
    /// with subagents (first, so a fresh clone's first pass is cold) and then top-level
    /// sessions only, each cold-or-warm then warm, with the resolve after it; the share of
    /// the stored top [`CLUSTER_TABLE_CAP`] that fails [`is_specific`]; how many of those
    /// [`resolve`] finds a fix for, and in how many projects a specific one; the top 30 by
    /// count as signatures (placeholders for paths, quotes and hashes; no session text); a
    /// project-scoped read's time; and, per arm and line rule, how many error-marked lines
    /// of the candidates it drops, with the signatures it drops most, to check it drops no
    /// real error, and what the traceback rule emits.
    ///
    /// CALLIMACHUS_GROUNDING_DB=<an APFS clone of index.db> cargo test -p callimachus-core \
    ///   real_issue_clusters_gate -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_issue_clusters_gate() {
        let Ok(path) = std::env::var("CALLIMACHUS_GROUNDING_DB") else {
            eprintln!("set CALLIMACHUS_GROUNDING_DB to an index clone");
            return;
        };
        let conn = crate::db::open_readonly(std::path::Path::new(&path)).unwrap();
        let now = chrono::Utc::now().timestamp();
        let since = now - CLUSTER_WINDOW_SECS;
        let t = std::time::Instant::now();
        let parents = crate::indexer::subagent_parents(&conn).unwrap();
        eprintln!("subagent_parents: {} in {:?}", parents.len(), t.elapsed());
        let mut busiest: Option<String> = None;
        for sightings in [Sightings::All, Sightings::TopLevel] {
            let mut mined = Vec::new();
            for pass in ["first", "warm"] {
                let t = std::time::Instant::now();
                mined = mine(
                    &conn,
                    None,
                    ProjectMatch::Exact,
                    since,
                    usize::MAX,
                    sightings,
                )
                .unwrap();
                let mined_in = t.elapsed();
                let top = &mined[..mined.len().min(CLUSTER_TABLE_CAP)];
                let t = std::time::Instant::now();
                let fixes = resolve(&conn, top).unwrap();
                eprintln!(
                    "{sightings:?} refresh ({pass}): mine {} recurring clusters in {mined_in:?}, \
                     resolve {} in {:?}",
                    mined.len(),
                    fixes.len(),
                    t.elapsed()
                );
            }
            let top = &mined[..mined.len().min(CLUSTER_TABLE_CAP)];
            if busiest.is_none() {
                let mut per: HashMap<&str, usize> = HashMap::new();
                for s in top.iter().flat_map(|m| &m.sightings) {
                    if let Some(p) = s.project.as_deref() {
                        *per.entry(p).or_default() += 1;
                    }
                }
                busiest = per
                    .into_iter()
                    .max_by_key(|(p, n)| (*n, *p))
                    .map(|(p, _)| p.to_string());
            }
            let generic = |ms: &[Mined]| ms.iter().filter(|m| !is_specific(&m.signature)).count();
            eprintln!(
                "stored top {}: {} fail is_specific ({:.1}%), {} of the top 30",
                top.len(),
                generic(top),
                100.0 * generic(top) as f64 / top.len().max(1) as f64,
                generic(&top[..top.len().min(30)])
            );
            let sightings: usize = top.iter().map(|m| m.sightings.len()).sum();
            let named = top
                .iter()
                .flat_map(|m| &m.sightings)
                .filter(|s| !s.files.is_empty())
                .count();
            let messages: i64 = top.iter().map(|m| m.cluster.count).sum();
            eprintln!(
                "{messages} sightings over {sightings} (session, cluster) pairs, {named} naming a file"
            );
            let fixes = resolve(&conn, top).unwrap();
            let specific_fixes: Vec<&BTreeMap<String, Resolution>> = fixes
                .iter()
                .filter(|(s, _)| is_specific(s))
                .map(|(_, p)| p)
                .collect();
            let fixed_projects: HashSet<&String> =
                specific_fixes.iter().flat_map(|p| p.keys()).collect();
            eprintln!(
                "resolve: {} clusters with a fix ({} project fixes), {} of them specific, \
                 in {} projects",
                fixes.len(),
                fixes.values().map(BTreeMap::len).sum::<usize>(),
                specific_fixes.len(),
                fixed_projects.len()
            );
            for (i, m) in top.iter().take(30).enumerate() {
                eprintln!(
                    "{:>3} {:>6} {:>5} {} {}",
                    i + 1,
                    m.cluster.count,
                    m.cluster.threads,
                    if is_specific(&m.signature) { "S" } else { "-" },
                    cap_chars(&m.signature, 110)
                );
            }
        }
        let stored = load_fixes(&conn);
        eprintln!(
            "stored fixes: {} signatures, {} specific",
            stored.len(),
            stored.keys().filter(|s| is_specific(s)).count()
        );
        // Harvested memory lives in `callimachus-notes:<project>` threads, which hold facts,
        // not messages: nothing there to mine.
        let notes: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages m JOIN threads t ON t.id = m.thread_id
                 WHERE t.external_id LIKE 'callimachus-notes:%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        eprintln!("messages in harvested-memory threads: {notes}");
        if let Some(project) = busiest {
            for pass in ["first", "warm"] {
                let t = std::time::Instant::now();
                let n = recurring_issues_matching(
                    &conn,
                    Some(&project),
                    ProjectMatch::Exact,
                    since,
                    CLUSTER_TABLE_CAP,
                )
                .unwrap()
                .len();
                eprintln!(
                    "scoped read of the busiest project ({pass}): {n} clusters in {:?}",
                    t.elapsed()
                );
            }
        }

        // The line rules, over each arm's candidates.
        for arm in [Arm::TopLevel, Arm::Subagents] {
            let (sql, args) = candidates_sql(&conn, None, ProjectMatch::Exact, since, arm);
            let refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
            let mut stmt = conn.prepare(&sql).unwrap();
            let mut rows = stmt.query(refs.as_slice()).unwrap();
            let mut dropped: BTreeMap<&str, HashMap<String, usize>> = BTreeMap::new();
            let (mut marked, mut candidates, mut headers) = (0usize, 0usize, 0usize);
            let mut exceptions: HashMap<String, usize> = HashMap::new();
            let mut extract = std::time::Duration::ZERO;
            while let Some(row) = rows.next().unwrap() {
                let text: String = row.get(0).unwrap();
                candidates += 1;
                for raw in text.lines() {
                    let line = raw.trim();
                    if starts_with_ci(line, TRACEBACK_HEADER) && !behind_gutter(raw) {
                        headers += 1;
                    }
                    if line.len() < 6 || line.len() > 400 || !is_error_line(line) {
                        continue;
                    }
                    marked += 1;
                    let rule = if behind_gutter(raw) {
                        "gutter"
                    } else if reads_as_code(line) {
                        "code"
                    } else {
                        continue;
                    };
                    *dropped
                        .entry(rule)
                        .or_default()
                        .entry(normalize(line))
                        .or_default() += 1;
                }
                let t = std::time::Instant::now();
                let found = extract_errors_with_files(&text);
                extract += t.elapsed();
                // Lines with no marker of their own come from the traceback rule.
                for (line, _) in found.iter().filter(|(l, _)| !is_error_line(l)) {
                    *exceptions.entry(normalize(line)).or_default() += 1;
                }
            }
            eprintln!(
                "{arm:?}: {candidates} candidate rows, extraction {extract:?}; \
                 {marked} error-marked lines; {headers} traceback headers"
            );
            let mut exc: Vec<(&String, &usize)> = exceptions.iter().collect();
            exc.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            eprintln!(
                "  traceback rule: {} exception lines with no marker, {} signatures",
                exc.iter().map(|e| *e.1).sum::<usize>(),
                exc.len()
            );
            for (sig, n) in exc.iter().take(10) {
                eprintln!("  {n:>5} {}", cap_chars(sig, 100));
            }
            for (rule, sigs) in &dropped {
                let mut sigs: Vec<(&String, &usize)> = sigs.iter().collect();
                sigs.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
                let lines: usize = sigs.iter().map(|s| *s.1).sum();
                let specific: usize = sigs.iter().filter(|s| is_specific(s.0)).map(|s| *s.1).sum();
                eprintln!(
                    "  rule {rule}: drops {lines} lines ({specific} of them is_specific), {} \
                     signatures",
                    sigs.len()
                );
                for (sig, n) in sigs.iter().take(25) {
                    eprintln!(
                        "  {n:>5} {} {}",
                        if is_specific(sig) { "S" } else { "-" },
                        cap_chars(sig, 100)
                    );
                }
            }
        }
    }

    /// Whether the recorded fixes hold, on a real index, read-only: the number a rule that
    /// the error stopped (a quiet period, several sessions) must beat before a playbook is
    /// memory again. Over the stored clusters with a fix, and over those that name their
    /// error (each has a playbook gotcha): the fix commit (its earliest `committed_at`)
    /// within 48 h after the last sighting; seen again after it; seen over more than 30 and
    /// 100 days; seen in one session only; a fix at least 7 and 30 days old with no sighting
    /// since. Counts only, never a signature, path or subject. A measurement: it asserts
    /// nothing.
    ///
    /// CALLIMACHUS_GROUNDING_DB=<an APFS clone of index.db> cargo test -p callimachus-core \
    ///   real_playbook_fixes_gate -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_playbook_fixes_gate() {
        let Ok(path) = std::env::var("CALLIMACHUS_GROUNDING_DB") else {
            eprintln!("set CALLIMACHUS_GROUNDING_DB to an index clone");
            return;
        };
        let conn = crate::db::open_readonly(std::path::Path::new(&path)).unwrap();
        if !crate::db::has_table(&conn, "issue_clusters") {
            eprintln!("no issue_clusters table: an index from before 0034");
            return;
        }
        const DAY: i64 = 86_400;
        let now = chrono::Utc::now().timestamp();
        // (signature, first_seen, last_seen, threads, the fix commit's time)
        type Row = (String, i64, i64, i64, Option<i64>);
        let rows: Vec<Row> = conn
            .prepare(
                "SELECT c.signature, c.first_seen, c.last_seen, c.threads,
                        (SELECT MIN(tc.committed_at) FROM thread_commits tc
                         WHERE tc.sha = c.resolved_sha)
                 FROM issue_clusters c WHERE c.resolved_sha IS NOT NULL",
            )
            .unwrap()
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for (label, specific_only) in [
            ("resolved clusters", false),
            ("naming their error (a playbook each)", true),
        ] {
            let set: Vec<&Row> = rows
                .iter()
                .filter(|r| !specific_only || is_specific(&r.0))
                .collect();
            let n = |p: &dyn Fn(&Row) -> bool| set.iter().filter(|r| p(r)).count();
            let quiet_for =
                |days: i64| n(&|r: &Row| r.4.is_some_and(|at| now - at >= days * DAY && r.2 <= at));
            eprintln!("{label}: {}", set.len());
            eprintln!(
                "  fix commit not in thread_commits: {}",
                n(&|r: &Row| r.4.is_none())
            );
            eprintln!(
                "  fix within 48 h after the last sighting: {}",
                n(&|r: &Row| r.4.is_some_and(|at| at >= r.2 && at - r.2 <= 2 * DAY))
            );
            eprintln!(
                "  seen again after the fix: {}",
                n(&|r: &Row| r.4.is_some_and(|at| at < r.2))
            );
            eprintln!(
                "  seen over more than 30 days: {}, more than 100: {}",
                n(&|r: &Row| r.2 - r.1 > 30 * DAY),
                n(&|r: &Row| r.2 - r.1 > 100 * DAY)
            );
            eprintln!("  seen in one session only: {}", n(&|r: &Row| r.3 <= 1));
            eprintln!(
                "  fix at least 7 days old with no sighting since: {}, at least 30: {}",
                quiet_for(7),
                quiet_for(30)
            );
        }
    }

    /// An idle machine (no batch since the refresh) keeps serving the table past the ten
    /// minutes, up to a day of window drift; a batch after the refresh makes it stale again.
    #[test]
    fn an_idle_index_serves_the_last_refresh() {
        let conn = clusters_db("idle");
        let now = chrono::Utc::now().timestamp();
        let at = now - 3 * 3600;
        let tid = app_thread(&conn, at - 60);
        for seq in 0..2 {
            add_msg(&conn, tid, seq, "error: linker failed", at - 60);
        }
        refresh_clusters_at(&conn, at).unwrap();
        add_msg(&conn, tid, 2, "error: disk full", at - 30);
        add_msg(&conn, tid, 3, "error: disk full", at - 30);
        let since = now - CLUSTER_WINDOW_SECS;
        assert_eq!(recurring_issues(&conn, None, since, 10).unwrap().len(), 1);
        assert!(!clusters_due(&conn, now, 300), "nothing indexed since");
        // Something indexed after the refresh: mined live, and the watcher refreshes.
        conn.execute("UPDATE threads SET last_indexed_at = ?1", [at + 5])
            .unwrap();
        assert_eq!(recurring_issues(&conn, None, since, 10).unwrap().len(), 2);
        assert!(clusters_due(&conn, now, 300));
        assert!(
            !clusters_due(&conn, at + 60, 300),
            "refreshed under the interval ago"
        );
        // A day later the window has drifted too far, idle or not.
        conn.execute("UPDATE threads SET last_indexed_at = ?1", [at - 60])
            .unwrap();
        let later = at + CLUSTERS_IDLE_SECS + 1;
        assert!(clusters_due(&conn, later, 300));
        assert_eq!(
            recurring_issues(&conn, None, later - CLUSTER_WINDOW_SECS, 10)
                .unwrap()
                .len(),
            2
        );
    }

    /// A refresh keeps the rows it no longer sees (out of the readers' sight) while they are
    /// inside the window or resolved, and drops only unresolved rows unseen for 180 days.
    #[test]
    fn a_refresh_prunes_only_unresolved_rows_unseen_for_the_window() {
        let conn = clusters_db("prune");
        let now = chrono::Utc::now().timestamp();
        let tid = app_thread(&conn, now - 60);
        for seq in 0..2 {
            add_msg(&conn, tid, seq, "error: linker failed", now - 60);
        }
        let row = |sig: &str, last_seen: i64, resolved: Option<&str>| {
            conn.execute(
                "INSERT INTO issue_clusters (signature, example, count, threads, first_seen,
                    last_seen, refreshed_at, resolved_sha) VALUES (?1, ?1, 9, 1, 0, ?2, 1, ?3)",
                rusqlite::params![sig, last_seen, resolved],
            )
            .unwrap();
        };
        row("fell out of the top", now - 86_400, None);
        row(
            "old and resolved",
            now - CLUSTER_WINDOW_SECS - 10,
            Some("abc"),
        );
        row("old and unresolved", now - CLUSTER_WINDOW_SECS - 10, None);
        refresh_clusters_at(&conn, now).unwrap();
        let mut kept: Vec<String> = conn
            .prepare("SELECT signature FROM issue_clusters ORDER BY signature")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let mut want = vec![
            normalize("error: linker failed"),
            "fell out of the top".to_string(),
            "old and resolved".to_string(),
        ];
        want.sort();
        kept.sort();
        assert_eq!(kept, want);
        // Readers see only the rows the last refresh wrote.
        let got = recurring_issues(&conn, None, now - CLUSTER_WINDOW_SECS, 10).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].example, "error: linker failed");
    }

    /// File content read with a line-number gutter and source lines that only name an
    /// error variable are not errors. Each case is a synthetic line of the shape that
    /// recurred on a real index; quoted `=>`, `?` and `:` inside a real message stay errors.
    #[test]
    fn read_gutters_and_code_lines_are_not_errors() {
        let read = "     1\tconst x = 1;\n\
                    \x20   12\terror: error instanceof Error ? error.message : String(error),\n\
                    \x20  131\t   130\t      setFailure(`could not save: ${e instanceof Error ? e.message : e}`);\n\
                    \x20   40\u{2192}TypeError: this line is in a file that was read\n\
                    \x20    7\tTraceback (most recent call last):";
        assert!(
            extract_errors(read).is_empty(),
            "{:?}",
            extract_errors(read)
        );

        for code in [
            "error: err instanceof Error ? err.message : err",
            "onError: (error: Error) => toast.error(error.message),",
            "onError: (error: unknown) =>",
            "onError: (error) => {",
            "error: {",
            "error: { code: \"forbidden\", message: \"Accept the terms first\" },",
            "+      error: { code: \"too_large\", message: \"Request too large\" },",
            "const status = error: ok ? 200 : 500",
            "} catch (error) {",
            "\"errors\": [",
            "return { error: {",
            "fn main() -> Result<(), Box<dyn std::error::Error>> {",
            "json({ error: e.message, details: {",
            // `error::` is a path, not a marker.
            "the embedder returns EmbedError::Init when the model is missing",
            "let e = io::Error::new(io::ErrorKind::Other, msg)",
            // A script's command, read without a gutter, not what it prints.
            "echo \"::error::the release tag is missing\"",
            "echo \"error: the release tag is missing\" >&2",
            "printf 'error: %s\\n' \"$msg\"",
            // The same command in a diff, a `set -x` trace, a workflow step and a prompt.
            "+          echo \"Error: version mismatch\" >&2",
            "+ echo 'error: missing env'",
            "run: echo \"error: tag missing\" && exit 1",
            "+      - run: echo \"error: tag missing\" && exit 1",
            "$ echo \"error: the release tag is missing\"",
            // pytest's verbose status line names a test, no error.
            "tests/test_parser.py::TestParseError::test_empty FAILED",
        ] {
            assert!(extract_errors(code).is_empty(), "{code}");
        }
        // An `error::` that straddles the 64-char head: the `:` after it lies past the head.
        let prose = "the embedder hands back a typed failure, for example ";
        for prefix in [prose.to_string(), "\u{2501}".repeat(53)] {
            let path = format!("{prefix}EmbedError::Init when the model is missing");
            let (at, _) = path.char_indices().nth(64).unwrap();
            assert!(path[..at].ends_with("EmbedError:"), "{path}");
            assert!(extract_errors(&path).is_empty(), "{path}");
        }

        for real in [
            "src/a.ts(3,5): error TS2322: Type '(e: Event) => void' is not assignable to type 'string'.",
            "error: expected one of `=>`, `@`, `if`, or `|`, found `:`",
            "error: can't find crate for `serde` (is it installed?)",
            "TypeError: Cannot read properties of undefined (reading 'map')",
            "error[E0277]: the `?` operator can only be used in a function that returns `Result`",
            "Error: ENOENT: no such file or directory, open 'config.json'",
            // A message that prints its payload on the next lines.
            "MCP error -32602: Input validation error: Invalid arguments for tool lookup: [",
            "Error: the upload was rejected with body: {",
            // Node's print of an error object without its stack.
            "[Error: EACCES: permission denied, open '/etc/x'] {",
            "[Error: SQLITE_ERROR: no such table: users] {",
            // The same behind Node's REPL and as a logged object's key.
            "Uncaught [Error: boom: bad] {",
            "  error: [Error: ENOENT: no such file or directory, open 'a.json'] {",
            "error[E0308]: mismatched types",
            "foo::Error: bad input",
            "echo: write error: Broken pipe",
            // pytest's summary, whose long node id pushes the exception past the head.
            "FAILED tests/test_parser.py::TestParseError::test_empty - ValueError: empty input",
        ] {
            assert_eq!(extract_errors(real), [real.trim()], "{real}");
        }
        assert!(behind_gutter("  1234567\tx") && behind_gutter("9\u{2192}x"));
        assert!(!behind_gutter("12345678\tx") && !behind_gutter("\t12\tx"));
        assert!(!behind_gutter("     12 error: counted by uniq -c"));
    }

    /// A Python traceback counts once, as its exception line, filed under the innermost
    /// frame's file; the header and the frames (whose source lines can carry a marker, like
    /// `except KeyError:`) never count. A block that ends without an exception line, a bare
    /// exception name and a header behind a log prefix add nothing.
    #[test]
    fn a_traceback_counts_as_its_exception_line() {
        let text = "Running the import\n\
                    Traceback (most recent call last):\n\
                    \x20 File \"/srv/app/cli.py\", line 9, in <module>\n\
                    \x20   main()\n\
                    \x20 File \"/srv/app/loader.py\", line 41, in load\n\
                    \x20   except KeyError: raise\n\
                    \x20          ^^^^^^^^\n\
                    KeyError: 'user_id'\n\
                    \n\
                    During handling of the above exception, another exception occurred:\n\
                    \n\
                    Traceback (most recent call last):\n\
                    \x20 File \"/srv/app/net.py\", line 3, in fetch\n\
                    \x20   conn.request()\n\
                    http.client.InvalidURL: nonnumeric port: 'eighty'\n\
                    done";
        let found = extract_errors_with_files(text);
        let lines: Vec<&str> = found.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(
            lines,
            [
                "KeyError: 'user_id'",
                "http.client.InvalidURL: nonnumeric port: 'eighty'"
            ]
        );
        assert_eq!(found[0].1[0], "/srv/app/loader.py", "{:?}", found[0].1);
        assert_eq!(found[1].1[0], "/srv/app/net.py", "{:?}", found[1].1);

        // Indented as a whole (a log, a code block): the block is read relative to its header.
        let indented = "    Traceback (most recent call last):\n\
                        \x20     File \"a.py\", line 1, in <module>\n\
                        \x20   json.decoder.JSONDecodeError: Expecting value: line 1 column 1 (char 0)";
        assert_eq!(
            extract_errors(indented),
            ["json.decoder.JSONDecodeError: Expecting value: line 1 column 1 (char 0)"]
        );
        for nothing in [
            // Cut off before its exception line (a head and tail join, a killed run).
            "Traceback (most recent call last):\n  File \"a.py\", line 1, in <module>\n",
            // A bare name says no more than the header did.
            "Traceback (most recent call last):\n  File \"t.py\", line 2, in test\nAssertionError",
            // Behind a log prefix the block cannot be followed; the header alone is no error.
            "[2026-01-02 10:00:00] Traceback (most recent call last):",
            // Cut off inside its frames, then a server's log line or a status line: not the
            // exception line, though capitalized and followed by `: `.
            "Traceback (most recent call last):\n  File \"/srv/app/main.py\", line 4, in run\n\
             INFO:     Application shutdown complete.",
            "Traceback (most recent call last):\n  File \"/srv/app/main.py\", line 4, in run\n\
             Status: 404",
            "Traceback (most recent call last):\n  File \"/srv/app/main.py\", line 4, in run\n\
             Warning: the config file is older than the lock file",
        ] {
            assert!(extract_errors(nothing).is_empty(), "{nothing}");
        }
        for exception in [
            "re.error: missing ), unterminated subpattern",
            "Exception: the importer gave up",
            "UserWarning: the field is deprecated",
            "ExceptionGroup: unhandled errors in a TaskGroup (1 sub-exception)",
            "jinja2.exceptions.TemplateNotFound: index.html",
            "subprocess.TimeoutExpired: Command '['make']' timed out after 5 seconds",
            "pydantic_core._pydantic_core.ValidationError: 1 validation error for User",
        ] {
            assert!(is_python_exception(exception), "{exception}");
        }
        for other in [
            "During handling: of the above",
            "note: run with `RUST_BACKTRACE=1`",
            "INFO: started",
            "Status: 404",
            "Warning: x",
            "Result: ok",
            "App.Status: ok",
        ] {
            assert!(!is_python_exception(other), "{other}");
        }
    }

    /// An exception raised inside the standard library or a package is filed under the
    /// session's own frames, innermost first, which a session can edit; the library's own
    /// file only when no frame is the session's.
    #[test]
    fn a_traceback_is_filed_under_the_sessions_own_frames() {
        let text = "Traceback (most recent call last):\n\
                    \x20 File \"/srv/app/cli.py\", line 9, in <module>\n\
                    \x20   main()\n\
                    \x20 File \"/srv/app/load.py\", line 12, in main\n\
                    \x20   json.loads(raw)\n\
                    \x20 File \"/usr/lib/python3.12/json/__init__.py\", line 346, in loads\n\
                    \x20   return _default_decoder.decode(s)\n\
                    \x20 File \"/srv/app/.venv/lib/python3.12/site-packages/x/y.py\", line 3, in f\n\
                    \x20   raise\n\
                    json.decoder.JSONDecodeError: Expecting value: line 1 column 1 (char 0)";
        let found = extract_errors_with_files(text);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].1[..2], ["/srv/app/load.py", "/srv/app/cli.py"]);

        let only_library = "Traceback (most recent call last):\n\
                            \x20 File \"<frozen runpy>\", line 198, in _run_module_as_main\n\
                            \x20 File \"C:\\Python312\\Lib\\json\\decoder.py\", line 355, in raw_decode\n\
                            json.decoder.JSONDecodeError: Expecting value: line 1 column 1 (char 0)";
        let found = extract_errors_with_files(only_library);
        assert_eq!(found[0].1[0], "C:\\Python312\\Lib\\json\\decoder.py");

        // A recursion error prints a frame per call; the innermost are kept.
        let mut deep = String::from("Traceback (most recent call last):\n");
        for i in 0..1_000 {
            deep.push_str(&format!(
                "  File \"/srv/app/m{i}.py\", line 1, in f\n    f()\n"
            ));
        }
        deep.push_str("RecursionError: maximum recursion depth exceeded");
        let found = extract_errors_with_files(&deep);
        assert_eq!(found[0].1[0], "/srv/app/m999.py");
        assert_eq!(found[0].1.len(), FILES_PER_ERROR);
    }

    /// A pytest run prints one failure twice, as the `E` line of its section and as the short
    /// summary's `FAILED <node> - <message>`: it counts once, filed under the source file its
    /// frames name and the test file of its node id, never under the run's timing. The same
    /// holds for a summary pytest cut to the terminal width, and for a run that prints only
    /// the summary.
    #[test]
    fn a_pytest_failure_counts_once_under_its_files() {
        let run = |summary: &str| {
            [
                "=================================== FAILURES ===================================",
                "_______________________________ test_empty_input _______________________________",
                "",
                "    def test_empty_input():",
                ">       parse(\"\")",
                "",
                "tests/test_parse.py:5: ",
                "_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ ",
                "",
                "    def parse(s):",
                "        if not s:",
                ">           raise ValueError(\"empty input\")",
                "E           ValueError: empty input",
                "",
                "src/parse.py:3: ValueError",
                "=========================== short test summary info ============================",
                summary,
                "========================= 1 failed, 1 passed in 0.03s ==========================",
            ]
            .join("\n")
        };
        for summary in [
            "FAILED tests/test_parse.py::test_empty_input - ValueError: empty input",
            "FAILED tests/test_parse.py::test_empty_input - ValueError: empty i...",
        ] {
            let found = extract_errors_with_files(&run(summary));
            assert_eq!(found.len(), 1, "{found:?}");
            assert_eq!(found[0].0, "E           ValueError: empty input");
            assert_eq!(found[0].1, ["src/parse.py", "tests/test_parse.py"]);
        }
        let summary = "FAILED tests/test_parse.py::test_empty_input - ValueError: empty input";
        assert_eq!(normalize(summary), "valueerror: empty input");
        assert_eq!(normalize("E   ValueError: empty input"), normalize(summary));
        let only = format!("{summary}\n1 failed, 1 passed in 0.03s");
        assert_eq!(
            extract_errors_with_files(&only),
            [(summary.to_string(), vec!["tests/test_parse.py".to_string()])]
        );

        // Two failures whose messages share a start: the summary cut to that start could
        // repeat either, so it files its test under neither, and both still count once.
        let two = [
            "=================================== FAILURES ===================================",
            "_______________________________ test_empty_input _______________________________",
            "",
            ">       parse(\"\")",
            "",
            "tests/test_parse.py:5: ",
            "_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ ",
            ">           raise ValueError(\"invalid input: empty\")",
            "E           ValueError: invalid input: empty",
            "",
            "src/parse.py:3: ValueError",
            "___________________ TestHeaders.test_header_rejects_garbage ____________________",
            "",
            ">       read_header(b\"\")",
            "",
            "tests/test_header.py:9: ",
            "_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ ",
            ">           raise ValueError(\"invalid header bytes\")",
            "E           ValueError: invalid header bytes",
            "",
            "src/header.py:7: ValueError",
            "=========================== short test summary info ============================",
            "FAILED tests/test_parse.py::test_empty_input - ValueError: invalid input: empty",
            "FAILED tests/test_header.py::TestHeaders::test_header_rejects_garbage - ValueError: invalid...",
            "============================== 2 failed in 0.05s ===============================",
        ]
        .join("\n");
        let files = |v: &[&str]| v.iter().map(|f| f.to_string()).collect::<Vec<_>>();
        assert_eq!(
            extract_errors_with_files(&two),
            [
                (
                    "E           ValueError: invalid input: empty".to_string(),
                    files(&["src/parse.py", "tests/test_parse.py"])
                ),
                (
                    "E           ValueError: invalid header bytes".to_string(),
                    files(&["src/header.py"])
                ),
            ]
        );
        // `--tb=no` prints only the summaries, each cut: two failures stay two, even when one
        // cut is a prefix of the other.
        let (a, b) = (
            "FAILED tests/test_parse.py::test_empty_input - ValueError: empty i...",
            "FAILED tests/test_other.py::test_other_thing_long - ValueError: em...",
        );
        assert_eq!(
            extract_errors_with_files(&format!("{a}\n{b}\n2 failed in 0.05s")),
            [
                (a.to_string(), files(&["tests/test_parse.py"])),
                (b.to_string(), files(&["tests/test_other.py"])),
            ]
        );
    }

    /// Two threads of one Claude Code session on disk: the session and a subagent of it.
    fn session_with_subagent(conn: &Connection, slug: &str) -> (i64, i64) {
        let add = |ext: &str, sub: bool| {
            conn.execute(
                "INSERT INTO threads (source_id, external_id, title, project_path, project_key,
                                      is_subagent)
                 VALUES (1, ?1, 't', '/r/app', '/r/app', ?2)",
                rusqlite::params![ext, sub],
            )
            .unwrap();
            conn.last_insert_rowid()
        };
        (
            add(&format!("{slug}.jsonl"), false),
            add(&format!("{slug}/subagents/agent-a.jsonl"), true),
        )
    }

    fn message_as(conn: &Connection, tid: i64, seq: i64, role: &str, text: &str, ts: i64) {
        conn.execute(
            "INSERT INTO messages (thread_id, seq, role, text, ts) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![tid, seq, role, text, ts],
        )
        .unwrap();
    }

    /// A subagent's tool output counts for its parent session (count, session list, the
    /// session of the latest sighting and its project); its `user` turns, the orchestrator's
    /// prompt, do not count; a subagent whose parent is not indexed counts on its own; and
    /// Today's top-level reading leaves subagents out.
    #[test]
    fn subagent_tool_output_counts_for_its_parent_session() {
        let conn = crate::db::test_support::TestIndex::new("issues_subagents");
        let now = chrono::Utc::now().timestamp();
        let (parent, sub) = session_with_subagent(&conn, "-r-app/s1");
        let err = "error[E0425]: cannot find value `total` in this scope";
        message_as(&conn, parent, 0, "tool", err, now - 300);
        message_as(&conn, sub, 0, "tool", err, now - 200);
        message_as(&conn, sub, 1, "user", err, now - 100);
        let orphan = {
            conn.execute(
                "INSERT INTO threads (source_id, external_id, title, project_path, is_subagent)
                 VALUES (1, '-r-app/gone/subagents/agent-b.jsonl', 't', '/r/other', 1)",
                [],
            )
            .unwrap();
            conn.last_insert_rowid()
        };
        let lonely = "error: linker `cc` not found in the toolchain";
        message_as(&conn, orphan, 0, "tool", lonely, now - 50);
        message_as(&conn, orphan, 1, "tool", lonely, now - 40);
        // Another agent's subagents (OpenCode's, Gemini CLI's, Cursor's) have no parent
        // mapping, so each would count as a session of its own: not mined.
        let elsewhere = "error: the lockfile needs to be updated but --frozen was passed";
        for (n, ext) in ["ses_child_a", "ses_child_b"].iter().enumerate() {
            conn.execute(
                "INSERT INTO threads (source_id, external_id, title, project_path, is_subagent)
                 VALUES ((SELECT id FROM sources WHERE kind = 'cursor'), ?1, 't', '/r/app', 1)",
                [ext],
            )
            .unwrap();
            message_as(
                &conn,
                conn.last_insert_rowid(),
                0,
                "tool",
                elsewhere,
                now - 30 + n as i64,
            );
        }

        let mined = mine(
            &conn,
            None,
            ProjectMatch::Exact,
            now - 3600,
            10,
            Sightings::All,
        )
        .unwrap();
        assert!(
            mined.iter().all(|m| m.cluster.example != elsewhere),
            "{:?}",
            mined.iter().map(|m| &m.cluster.example).collect::<Vec<_>>()
        );
        let of = |sig: &str| mined.iter().find(|m| m.cluster.example == sig).unwrap();
        let c = &of(err).cluster;
        assert_eq!((c.count, c.threads), (2, 1), "the user turn is not counted");
        assert_eq!(c.thread_ids, vec![parent]);
        assert_eq!((c.last_thread_id, c.last_seen), (parent, now - 200));
        assert_eq!(of(err).sightings[0].project.as_deref(), Some("/r/app"));
        let o = &of(lonely);
        assert_eq!(o.cluster.thread_ids, vec![orphan]);
        assert_eq!(o.sightings[0].project.as_deref(), Some("/r/other"));
        // Scoped to a project, the subagent arm is scoped too.
        let scoped =
            recurring_issues_matching(&conn, Some("/r/app"), ProjectMatch::Exact, now - 3600, 10)
                .unwrap();
        assert_eq!(scoped.len(), 1, "{scoped:?}");

        let top = recurring_issues_from(
            &conn,
            None,
            ProjectMatch::Exact,
            now - 3600,
            10,
            Sightings::TopLevel,
        )
        .unwrap();
        assert!(top.is_empty(), "seen once at top level: {top:?}");
    }

    /// A subagent's final report comes back to its parent as a tool row that quotes its
    /// output: that copy, and any later sighting in the parent, is no second sighting. A
    /// parent that hit the error before its subagent counts both; two sessions that each
    /// quote their subagent's error count once each. Today's top-level reading skips nothing.
    #[test]
    fn a_subagents_error_quoted_back_to_its_parent_counts_once() {
        let conn = crate::db::test_support::TestIndex::new("issues_reports");
        let now = chrono::Utc::now().timestamp();
        let (p1, s1) = session_with_subagent(&conn, "-r-app/s1");
        let quoted = "error[E0425]: cannot find value `total` in this scope";
        message_as(&conn, s1, 0, "tool", quoted, now - 300);
        let report = format!("The tests failed:\n{quoted}");
        message_as(&conn, p1, 0, "tool", &report, now - 250);
        message_as(&conn, p1, 1, "tool", quoted, now - 200);
        let (p2, s2) = session_with_subagent(&conn, "-r-app/s2");
        let first = "error[E0599]: no method named `frob` found for struct `Widget`";
        message_as(&conn, p2, 0, "tool", first, now - 400);
        message_as(&conn, s2, 0, "tool", first, now - 300);
        message_as(&conn, p2, 1, "tool", first, now - 290);
        let shared = "error: linker `cc` not found in the toolchain";
        let (p3, s3) = session_with_subagent(&conn, "-r-app/s3");
        let (p4, s4) = session_with_subagent(&conn, "-r-app/s4");
        for (p, s, at) in [(p3, s3, now - 500), (p4, s4, now - 100)] {
            message_as(&conn, s, 0, "tool", shared, at);
            message_as(&conn, p, 0, "tool", shared, at + 30);
        }

        let read = |sightings| {
            let mined = mine(&conn, None, ProjectMatch::Exact, now - 3600, 10, sightings);
            mined.unwrap()
        };
        let all = read(Sightings::All);
        let of = |mined: &[Mined], err: &str| {
            let m = mined.iter().find(|m| m.cluster.example == err);
            m.map(|m| (m.cluster.count, m.cluster.threads))
        };
        assert_eq!(of(&all, quoted), None, "one sighting, not listed");
        assert_eq!(of(&all, first), Some((2, 1)));
        assert_eq!(of(&all, shared), Some((2, 2)));
        let c = &all
            .iter()
            .find(|m| m.cluster.example == shared)
            .unwrap()
            .cluster;
        assert_eq!((c.last_thread_id, c.last_seen), (p4, now - 100));
        assert_eq!(c.thread_ids, [p4, p3]);

        let top = read(Sightings::TopLevel);
        assert_eq!(of(&top, quoted), Some((2, 1)));
        assert_eq!(of(&top, first), Some((2, 1)));
        assert_eq!(of(&top, shared), Some((2, 2)));
    }

    /// The lists show only clusters that name their error; the table keeps the rest, and
    /// `--all` lists them too. A fix stored for a signature that now reads as generic is no
    /// playbook for the hook.
    #[test]
    fn lists_hold_specific_clusters_and_all_adds_the_rest() {
        let conn = crate::db::test_support::TestIndex::new("issues_specific");
        let now = chrono::Utc::now().timestamp();
        let tid = app_thread(&conn, now - 60);
        let specific = "error[E0599]: no method named `frob` found for struct `Widget`";
        for (seq, text) in [
            "error: process completed with exit code 1.",
            "error: process completed with exit code 1.",
            "error: run failed: command exited (1)",
            "error: run failed: command exited (1)",
            specific,
            specific,
        ]
        .into_iter()
        .enumerate()
        {
            add_msg(&conn, tid, seq as i64, text, now - 60);
        }
        assert_eq!(refresh_clusters_at(&conn, now).unwrap(), 3);
        let since = now - CLUSTER_WINDOW_SECS;
        let listed =
            recurring_issues_with_fixes(&conn, None, ProjectMatch::Exact, since, 10).unwrap();
        let examples: Vec<&str> = listed.iter().map(|i| i.cluster.example.as_str()).collect();
        assert_eq!(examples, [specific]);
        let all = listed_issues(&conn, None, ProjectMatch::Exact, since, 10, true).unwrap();
        assert_eq!(all.len(), 3);
        // Live (scoped) reads filter the same way, before the limit.
        let scoped =
            recurring_issues_with_fixes(&conn, Some("/r/app"), ProjectMatch::Exact, since, 1)
                .unwrap();
        assert_eq!(scoped[0].cluster.example, specific);

        // A fix recorded for `patch does not apply` before it counted as generic.
        let generic = normalize("error: src/a.rs: patch does not apply");
        assert!(!is_specific(&generic));
        conn.execute(
            "INSERT INTO thread_commits
                (thread_id, sha, short_sha, subject, committed_at, overlap, created_at)
             VALUES (?1, 'abababababab', 'abababa', 'fix: rebase', ?2, 1, ?2)",
            rusqlite::params![tid, now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO issue_clusters (signature, example, count, threads, first_seen,
                last_seen, refreshed_at) VALUES (?1, ?1, 3, 1, 0, 0, 0)",
            [&generic],
        )
        .unwrap();
        let mut fixes = Fixes::new();
        fixes.entry(generic.clone()).or_default().insert(
            "/r/app".to_string(),
            Resolution {
                thread_id: tid,
                sha: "abababababab".into(),
                committed_at: now,
            },
        );
        crate::knowledge::app_config::put(
            &conn,
            CLUSTER_FIXES,
            &serde_json::to_string(&fixes).unwrap(),
        )
        .unwrap();
        assert!(
            playbooks_for(&conn, std::slice::from_ref(&generic), "/r/app")
                .unwrap()
                .is_empty()
        );

        // Its gotcha, written back then, leaves project memory on the next refresh, unless
        // the user pinned, edited or deleted (hid) one; a specific signature's stays, and so
        // does one whose project key holds an `@`.
        let sig_at =
            normalize("npm error: no matching version found for react@latest in the registry");
        assert!(sig_at.contains('@') && is_specific(&sig_at), "{sig_at}");
        let add = |origin: &str, curated: &str| {
            conn.execute(
                &format!(
                    "INSERT INTO facts (thread_id, kind, text, status, extractor, origin_ref,
                                        created_at, {curated})
                     VALUES (?1, 'gotcha', ?2, 'open', 'playbook', ?2, ?3, 1)"
                ),
                rusqlite::params![tid, origin, now],
            )
            .unwrap();
        };
        add(&playbook_ref(&generic, "/r/app"), "embedded");
        for curated in ["pinned", "edited", "hidden"] {
            add(&playbook_ref(&generic, &format!("/r/{curated}")), curated);
        }
        add(&playbook_ref(&sig_at, "/r/app"), "embedded");
        add(&playbook_ref(&generic, "/r/x@2"), "embedded");
        add(&playbook_ref(&sig_at, "/r/x@2"), "embedded");
        refresh_clusters_at(&conn, now + 1).unwrap();
        let mut stmt = conn
            .prepare("SELECT origin_ref FROM facts WHERE extractor = 'playbook' ORDER BY id")
            .unwrap();
        let left: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let want: Vec<String> = ["/r/pinned", "/r/edited", "/r/hidden"]
            .iter()
            .map(|p| playbook_ref(&generic, p))
            .chain([
                playbook_ref(&sig_at, "/r/app"),
                playbook_ref(&sig_at, "/r/x@2"),
            ])
            .collect();
        assert_eq!(left, want);
        assert_eq!(
            playbook_signature(&playbook_ref(&sig_at, "/r/app")),
            Some(sig_at.as_str())
        );
    }

    /// Clusters a refresh mined with other extraction rules count as no refresh: readers
    /// mine live and a refresh is due at once, and the next refresh serves again. A later
    /// refresh by an older build (which does not stamp the rules) reads as stale too.
    #[test]
    fn clusters_mined_with_other_rules_are_mined_again() {
        let conn = crate::db::test_support::TestIndex::new("issues_rules");
        let now = chrono::Utc::now().timestamp();
        let tid = app_thread(&conn, now - 60);
        for seq in 0..2 {
            add_msg(&conn, tid, seq, "fatal: bad object HEAD", now - 60);
        }
        refresh_clusters_at(&conn, now).unwrap();
        assert!(refresh_state(&conn).is_some());
        assert!(!clusters_due(&conn, now + 60, 300));
        let since = now - CLUSTER_WINDOW_SECS;
        assert!(stored_clusters(&conn, since, 10, now + 60).is_some());

        for stale in ["1@{now}", "2@{earlier}", ""] {
            let stamp = stale
                .replace("{now}", &now.to_string())
                .replace("{earlier}", &(now - 1).to_string());
            crate::knowledge::app_config::put(&conn, CLUSTERS_EXTRACTION, &stamp).unwrap();
            assert!(clusters_due(&conn, now + 60, 300), "{stamp}");
            assert!(
                stored_clusters(&conn, since, 10, now + 60).is_none(),
                "{stamp}"
            );
        }
        refresh_clusters_at(&conn, now + 60).unwrap();
        assert!(!clusters_due(&conn, now + 120, 300));
        assert_eq!(
            crate::knowledge::app_config::get(&conn, CLUSTERS_EXTRACTION)
                .unwrap()
                .as_deref(),
            Some(extraction_stamp(now + 60).as_str())
        );
    }

    #[test]
    fn normalize_replaces_paths_quotes_hashes() {
        let k = normalize("panicked at /a/b/c.rs:1:2 with hash deadbeef1234 and `ident`");
        assert!(k.contains("<path>"), "{k}");
        assert!(k.contains("'…'"), "{k}");
        assert!(k.contains("<n>"), "{k}");
        assert!(!k.contains("deadbeef"), "{k}");
    }
}
