//! Property tests for the text transforms that run on untrusted input (transcripts, tool
//! output, distilled facts) on its way to an agent, a model provider or a terminal. No
//! `proptest`: a fixed-seed xorshift over a curated alphabet (ASCII, multi-byte and combining
//! characters, zero-width and bidi controls, terminal escapes, fence tags, JSON escapes and
//! secret prefixes with random tails built at runtime) runs a few thousand cases per
//! property, and a failure names the case's seed and input so it reproduces exactly.

use crate::context::{escape_fences, pack_thread_detail, redact_secrets, strip_controls};
use crate::export::{agent_memory_md_budgeted, MemoryBudget};
use crate::knowledge::{MemoryFact, ProjectMemory};
use crate::search::{MessageRow, ThreadDetail};
use std::cell::Cell;

/// xorshift64 (Marsaglia's 13/7/17): deterministic, dependency-free, plenty for test input.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // Spread nearby seeds apart; xorshift's state must never be zero.
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }

    fn run(&mut self, alphabet: &str, n: usize) -> String {
        let a = alphabet.as_bytes();
        (0..n).map(|_| char::from(a[self.below(a.len())])).collect()
    }
}

const ALNUM: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
const KEY_DASH: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-";
const HEX: &str = "0123456789abcdef";
const UPPER_DIGIT: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// Pieces random text is made of: the shapes that have broken text pipelines before.
const ATOMS: &[&str] = &[
    // Multi-byte, combining and emoji sequences (a cut inside one would split a char).
    "é",
    "ß",
    "日本",
    "Ω",
    "😀",
    "👍🏽",
    "🇺🇸",
    "ﬁ",
    "e\u{301}",
    "\u{301}",
    "\u{20DD}",
    "a\u{308}",
    // Zero-width and other default-ignorable chars, tag chars, bidi controls.
    "\u{200B}",
    "\u{200C}",
    "\u{200D}",
    "\u{2060}",
    "\u{FEFF}",
    "\u{AD}",
    "\u{FE0F}",
    "\u{E0041}",
    "\u{E007F}",
    "\u{202E}",
    "\u{202A}",
    "\u{2066}",
    "\u{2069}",
    "\u{61C}",
    // C0 / C1 controls and terminal escape sequences, terminated and not.
    "\0",
    "\u{7}",
    "\u{8}",
    "\u{B}",
    "\u{C}",
    "\r",
    "\r\n",
    "\n",
    "\t",
    "\u{1B}",
    "\u{1B}[31m",
    "\u{1B}[0m",
    "\u{1B}]0;title\u{7}",
    "\u{1B}]8;;http://x\u{1B}\\",
    "\u{9B}2J",
    "\u{9D}",
    "\u{85}",
    "\u{7F}",
    "\u{1B}P",
    "\u{90}",
    // Fence tags, whole and in parts, in any case.
    "<",
    "</",
    "< /",
    ">",
    "/",
    "reference_thread",
    "REFERENCE_Thread",
    "project_memory",
    "existing_memory",
    "session_prompts",
    "!-- callimachus:",
    "<!--",
    "-->",
    "```",
    "&lt;",
    "</reference_thread>",
    "<project_memory>",
    // JSON and quoting.
    "\\",
    "\\u0041",
    "\\u0073",
    "\\u200b",
    "\\n",
    "\"",
    "'",
    "{",
    "}",
    ":",
    "=",
    " ",
    "  ",
    // Credential context.
    "token",
    "password",
    "secret",
    "--token ",
    "Bearer ",
    "Basic ",
    "://user:",
    "@host",
    "api_key=",
    "sk-",
    "hf_",
    "npm_",
];

/// The formats [`planted_key`] plants: prefix, body alphabet, shortest body.
const PLANTED: &[(&str, &str, usize)] = &[
    ("sk-ant-api03-", KEY_DASH, 32),
    ("sk_live_", ALNUM, 24),
    ("ghp_", ALNUM, 36),
    ("github_pat_", ALNUM, 40),
    ("AKIA", UPPER_DIGIT, 16),
    ("AIza", KEY_DASH, 35),
    ("xoxb-", ALNUM, 24),
    ("glpat-", KEY_DASH, 20),
    ("npm_", ALNUM, 36),
    ("hf_", ALNUM, 34),
    ("phx_", ALNUM, 43),
    ("gsk_", ALNUM, 52),
    ("xai-", ALNUM, 80),
    ("pplx-", ALNUM, 48),
    ("r8_", ALNUM, 37),
    ("sb_secret_", KEY_DASH, 31),
    ("ya29.", KEY_DASH, 60),
    ("GOCSPX-", KEY_DASH, 28),
    ("dop_v1_", HEX, 64),
    ("shpat_", HEX, 32),
    ("lin_api_", ALNUM, 40),
    ("AGE-SECRET-KEY-1", UPPER_DIGIT, 58),
];

/// A credential in one of the formats `redact_secrets` knows, with a random body of a valid
/// length, built here so no key-shaped literal sits in the source (push protection).
fn planted_key(rng: &mut Rng) -> String {
    let (prefix, alphabet, min) = PLANTED[rng.below(PLANTED.len())];
    // AWS ids are exactly 16 after the prefix; the rest take any length past the minimum.
    let n = if prefix == "AKIA" {
        min
    } else {
        min + rng.below(12)
    };
    format!("{prefix}{}", rng.run(alphabet, n))
}

/// `key` as it goes into random text: between separators (a key glued to a word is
/// deliberately not one) or as the username of a URL with a password, the form GitHub
/// documents for a token (`https://<token>:x-oauth-basic@github.com`).
fn plant(rng: &mut Rng, key: &str) -> String {
    if rng.chance(15) {
        return format!(" https://{key}:x@host ");
    }
    let (before, after) = (
        rng.pick(&[" ", "\n", "\"", "(", "="]),
        rng.pick(&[" ", "\n", "\"", ")", ","]),
    );
    format!("{before}{key}{after}")
}

/// Escapes and controls a stripped key may follow: bare ones that would read its first char
/// as their final byte (`ESC (sk-…`, `ESC SP xoxb-…`, an 8-bit CSI), whole ones, and controls
/// the strip drops, which would glue it to the word before.
const LEADS: &[&str] = &[
    "\u{1B}",
    "\u{1B} ",
    "\u{1B}(",
    "\u{1B}#",
    "\u{1B}[",
    "\u{9B}",
    "\u{1B}[1;",
    "\u{1B}[0m",
    "\u{1B}]0;t\u{7}",
    "\u{0}",
    "\u{7F}",
    "\u{202E}",
];

/// [`plant`] for text that is stripped before it is redacted (the packer, the memory block):
/// after a line break, which ends any escape sequence the random text left open, and often
/// right after one of [`LEADS`], glued to a word or not.
fn plant_after_strip(rng: &mut Rng, key: &str) -> String {
    if rng.chance(30) {
        let (word, lead) = (rng.pick(&["", "x", "out: "]), rng.pick(LEADS));
        return format!("\n{word}{lead}{key} ");
    }
    format!("\n{}", plant(rng, key))
}

/// Whether `text` holds `key` right after one of [`LEADS`].
fn led(text: &str, key: &str) -> bool {
    LEADS
        .iter()
        .any(|lead| text.contains(&format!("{lead}{key}")))
}

/// What of a planted key reached `out`, if any: a 12-char window of its random body. The
/// whole key alone is too weak a check: a key cut before it was redacted (a 50-char key cut
/// to 30 falls under its rule's minimum and passes whole) or a key left out of the
/// redaction altogether in a packed, elided thread never shows up whole either.
fn leaked(out: &str, key: &str) -> Option<String> {
    let (prefix, _, _) = PLANTED.iter().find(|(p, _, _)| key.starts_with(p))?;
    let body = &key[prefix.len()..];
    (0..=body.len().saturating_sub(12))
        .map(|k| &body[k..(k + 12).min(body.len())])
        .find(|window| out.contains(window))
        .map(|window| format!("{window:?} of planted key {key:?} survived"))
}

/// Random text of up to `atoms` pieces: atoms, ASCII runs and words.
fn random_text(rng: &mut Rng, atoms: usize) -> String {
    let mut out = String::new();
    for _ in 0..rng.below(atoms + 1) {
        match rng.below(4) {
            0 => {
                let n = 1 + rng.below(12);
                let printable: String = (0..n)
                    .map(|_| char::from(b' ' + rng.below(95) as u8))
                    .collect();
                out.push_str(&printable);
            }
            1 => out.push_str(rng.pick(&["the", "fix", "error", "cargo", "Users", "README"])),
            _ => out.push_str(rng.pick(ATOMS)),
        }
    }
    out
}

/// Run `cases` cases: `generate` builds each case's input from its own seeded generator, then
/// `property` checks it. A failed check, or a panic inside the code under test (how a cut on
/// a non-char boundary fails), is reported with the case's seed and its whole input, built
/// before the property runs so a panic cannot lose it.
fn check<I: std::fmt::Debug>(
    name: &str,
    base: u64,
    cases: u64,
    generate: impl Fn(&mut Rng) -> I,
    property: impl Fn(&I) -> Result<(), String>,
) {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    for case in 0..cases {
        let seed = base.wrapping_add(case);
        let Ok(input) = catch_unwind(AssertUnwindSafe(|| generate(&mut Rng::new(seed)))) else {
            panic!("{name}: case seed {seed:#x} panicked building its input (see above)");
        };
        match catch_unwind(AssertUnwindSafe(|| property(&input))) {
            Ok(Ok(())) => {}
            Ok(Err(why)) => panic!("{name}: case seed {seed:#x} failed: {why}\ninput: {input:?}"),
            Err(_) => panic!(
                "{name}: case seed {seed:#x} panicked (see the message above)\ninput: {input:?}"
            ),
        }
    }
}

/// Whether `text` opens or closes a fence: a `<`, optional ASCII whitespace, an optional `/`
/// and whitespace, then a marker, case-insensitively. Written apart from the escaper, and run
/// on text after `strip_controls`, which is what an agent finally reads.
fn fence_tag_in(text: &str) -> Option<String> {
    for (at, _) in text.match_indices('<') {
        let rest = text[at + 1..].trim_start_matches(|c: char| c.is_ascii_whitespace());
        let rest = rest.strip_prefix('/').unwrap_or(rest);
        let rest = rest.trim_start_matches(|c: char| c.is_ascii_whitespace());
        for marker in crate::context::FENCE_MARKERS {
            let head: String = rest.chars().take(marker.chars().count()).collect();
            if head.eq_ignore_ascii_case(marker) {
                return Some(text[at..].chars().take(40).collect());
            }
        }
    }
    None
}

/// A property that panics (as a slice on a non-char boundary does) still reports what it ran
/// on, so the case reproduces without rebuilding it from the seed by hand.
#[test]
fn a_panicking_case_reports_its_seed_and_input() {
    let report = std::panic::catch_unwind(|| {
        check(
            "demo",
            0x70,
            1,
            |rng| format!("input-{}", rng.below(10)),
            |_: &String| -> Result<(), String> { panic!("byte index 3 is not a char boundary") },
        )
    })
    .unwrap_err();
    let message = report.downcast_ref::<String>().expect("a formatted panic");
    assert!(
        message.contains("seed 0x70") && message.contains("input: \"input-"),
        "{message}"
    );
}

fn is_stripped(c: char) -> bool {
    matches!(c, '\0'..='\u{8}' | '\u{B}'..='\u{1F}' | '\u{7F}'..='\u{9F}')
        || matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{E0000}'..='\u{E007F}')
}

#[test]
fn redaction_never_panics_is_idempotent_and_leaves_no_planted_key() {
    let (planted, in_urls) = (Cell::new(0), Cell::new(0));
    let generate = |rng: &mut Rng| {
        let mut input = random_text(rng, 30);
        let mut keys = Vec::new();
        for _ in 0..rng.below(3) {
            let key = planted_key(rng);
            input.push_str(&plant(rng, &key));
            input.push_str(&random_text(rng, 10));
            keys.push(key);
        }
        (input, keys)
    };
    check(
        "redact_secrets",
        0x5EC2E7,
        4000,
        generate,
        |(input, keys)| {
            planted.set(planted.get() + keys.len());
            in_urls.set(in_urls.get() + input.matches("https://").count());
            let once = redact_secrets(input).into_owned();
            for key in keys {
                if let Some(leak) = leaked(&once, key) {
                    return Err(format!("{leak}\nout: {once:?}"));
                }
            }
            let twice = redact_secrets(&once);
            if twice != once {
                return Err(format!("not idempotent\nonce: {once:?}\ntwice: {twice:?}"));
            }
            Ok(())
        },
    );
    assert!(planted.get() > 3000, "only {} keys planted", planted.get());
    assert!(in_urls.get() > 300, "only {} keys in URLs", in_urls.get());
}

#[test]
fn stripping_and_fence_escaping_leave_no_control_or_fence_tag() {
    let (stripped_some, escaped_some) = (Cell::new(0), Cell::new(0));
    let generate = |rng: &mut Rng| random_text(rng, 40);
    check(
        "strip_controls / escape_fences",
        0xF34CE,
        4000,
        generate,
        |input| {
            let stripped = strip_controls(input);
            stripped_some.set(stripped_some.get() + usize::from(stripped != *input));
            if let Some(c) = stripped.chars().find(|&c| is_stripped(c)) {
                return Err(format!("{c:?} survived strip_controls\nout: {stripped:?}"));
            }
            if strip_controls(&stripped) != stripped {
                return Err("strip_controls is not idempotent".to_string());
            }
            let escaped = escape_fences(input);
            escaped_some.set(escaped_some.get() + usize::from(escaped != *input));
            if escape_fences(&escaped) != escaped {
                return Err(format!("escape_fences is not idempotent\nout: {escaped:?}"));
            }
            // Escaped first and stripped after (the order the hook and the packer use): a tag
            // that forms only once the controls are gone must already be escaped.
            for shown in [
                strip_controls(&escaped),
                strip_controls(&escape_fences(&stripped)),
            ] {
                if let Some(tag) = fence_tag_in(&shown) {
                    return Err(format!("fence tag {tag:?} left\nout: {shown:?}"));
                }
            }
            Ok(())
        },
    );
    // The generator must keep producing what the transforms act on.
    assert!(
        stripped_some.get() > 2000,
        "{} stripped",
        stripped_some.get()
    );
    assert!(escaped_some.get() > 500, "{} escaped", escaped_some.get());
}

#[test]
fn packed_threads_respect_their_budget_and_their_fence() {
    let (cut, with_keys, after_leads) = (Cell::new(0), Cell::new(0), Cell::new(0));
    let generate = |rng: &mut Rng| {
        let n = 1 + rng.below(30);
        let mut keys = Vec::new();
        let messages: Vec<MessageRow> = (0..n)
            .map(|k| {
                let tool = rng.chance(30);
                let role = if tool && rng.chance(50) {
                    "tool"
                } else if k % 2 == 0 {
                    "user"
                } else {
                    "assistant"
                };
                let atoms = if rng.chance(30) { 500 } else { 60 };
                let mut text = random_text(rng, atoms);
                if rng.chance(20) {
                    let key = planted_key(rng);
                    text.push_str(&plant_after_strip(rng, &key));
                    keys.push(key);
                }
                MessageRow {
                    id: k as i64 + 1,
                    role: role.into(),
                    text_len: text.encode_utf16().count(),
                    text,
                    tool_name: (tool && role == "assistant").then(|| random_text(rng, 3)),
                    ts: Some(1_700_000_000 + k as i64),
                    model: None,
                }
            })
            .collect();
        let detail = ThreadDetail {
            id: 1,
            source: "claude_code".into(),
            external_id: "t".into(),
            title: Some(random_text(rng, 6)),
            project_path: Some(format!("/repo/{}", random_text(rng, 4))),
            project_key: None,
            git_branch: None,
            created_at: Some(1_700_000_000),
            updated_at: Some(1_700_000_100),
            starred: false,
            tags: Vec::new(),
            messages,
        };
        (1_500 + rng.below(8_000), detail, keys)
    };
    check(
        "pack_thread_detail",
        0x9AC4,
        600,
        generate,
        |(budget, detail, keys)| {
            let budget = *budget;
            let out = pack_thread_detail(detail, budget);
            let chars = out.chars().count();
            cut.set(cut.get() + usize::from(out.contains("<!-- ")));
            with_keys.set(with_keys.get() + usize::from(!keys.is_empty()));
            after_leads.set(
                after_leads.get()
                    + keys
                        .iter()
                        .filter(|k| detail.messages.iter().any(|m| led(&m.text, k)))
                        .count(),
            );
            if chars > budget {
                return Err(format!("{chars} chars over a budget of {budget}"));
            }
            if out.contains('\u{FFFD}') {
                return Err("a cut split a char".to_string());
            }
            for key in keys {
                if let Some(leak) = leaked(&out, key) {
                    return Err(leak);
                }
            }
            let shown = strip_controls(&out);
            let body = shown
                .strip_suffix("</reference_thread>")
                .ok_or("the envelope is not closed last")?;
            let body = &body[body.find('>').map_or(0, |i| i + 1)..];
            if let Some(tag) = fence_tag_in(body) {
                return Err(format!("fence tag {tag:?} inside the envelope"));
            }
            Ok(())
        },
    );
    assert!(
        cut.get() > 150,
        "only {} threads were cut to their budget",
        cut.get()
    );
    assert!(
        with_keys.get() > 300,
        "only {} threads had a planted key",
        with_keys.get()
    );
    assert!(
        after_leads.get() > 100,
        "only {} keys followed an escape or control",
        after_leads.get()
    );
}

#[test]
fn memory_blocks_respect_their_budget_and_redact_every_fact() {
    #[derive(Debug)]
    struct Case {
        budget: MemoryBudget,
        project: String,
        brief: Option<String>,
        mem: ProjectMemory,
        keys: Vec<String>,
    }
    let (with_keys, overflowed, after_leads) = (Cell::new(0), Cell::new(0), Cell::new(0));
    let generate = |rng: &mut Rng| {
        let mut keys = Vec::new();
        let mut fact = |rng: &mut Rng, kind: &str, id: i64| {
            let atoms = if rng.chance(15) { 200 } else { 25 };
            let mut text = random_text(rng, atoms);
            if rng.chance(25) {
                let key = planted_key(rng);
                text.push_str(&plant_after_strip(rng, &key));
                keys.push(key);
            }
            MemoryFact {
                id,
                thread_id: 1,
                text,
                kind: kind.into(),
                pinned: rng.chance(20),
                extractor: rng.pick(&["llm", "agent", "repo", "user"]).into(),
                created_at: 1_700_000_000,
                decided_at: 1_700_000_000 + id,
                source: "claude_code".into(),
                support: 1,
                ..Default::default()
            }
        };
        let mut facts = |rng: &mut Rng, kind: &str| -> Vec<MemoryFact> {
            (0..rng.below(14))
                .map(|k| fact(rng, kind, k as i64 + 1))
                .collect()
        };
        let mem = ProjectMemory {
            project: "/repo/app".into(),
            decisions: facts(rng, "decision"),
            gotchas: facts(rng, "gotcha"),
            open_todos: facts(rng, "todo"),
            superseded: Vec::new(),
            flagged: Vec::new(),
            thread_count: 3,
            distilled_count: 2,
            pending_count: 1,
        };
        let budget = match rng.below(5) {
            0 => MemoryBudget::HOOK,
            1 => MemoryBudget::COMPACT,
            2 => MemoryBudget::AGENTS_FILE,
            3 => MemoryBudget::MCP,
            _ => MemoryBudget {
                max_chars: rng.below(4_000),
                max_decisions: rng.below(10),
                max_gotchas: rng.below(10),
                max_todos: rng.below(3),
                fact_chars: rng.below(300),
                ..MemoryBudget::HOOK
            },
        };
        let project = format!("/repo/{}", random_text(rng, 4));
        let brief = rng.chance(30).then(|| random_text(rng, 30));
        Case {
            budget,
            project,
            brief,
            mem,
            keys,
        }
    };
    check(
        "agent_memory_md_budgeted",
        0x3E30B,
        1500,
        generate,
        |case| {
            let out = agent_memory_md_budgeted(
                &case.project,
                &case.mem,
                case.brief.as_deref(),
                &case.budget,
            );
            let chars = out.chars().count();
            with_keys.set(with_keys.get() + usize::from(!case.keys.is_empty()));
            let facts = || {
                let m = &case.mem;
                m.decisions.iter().chain(&m.gotchas).chain(&m.open_todos)
            };
            after_leads.set(
                after_leads.get()
                    + case
                        .keys
                        .iter()
                        .filter(|k| facts().any(|f| led(&f.text, k)))
                        .count(),
            );
            overflowed.set(overflowed.get() + usize::from(out.contains(" more (run `cal memory`")));
            if chars > case.budget.max_chars {
                return Err(format!("{chars} chars over {}", case.budget.max_chars));
            }
            if out.contains('\u{FFFD}') {
                return Err("a cut split a char".to_string());
            }
            for key in &case.keys {
                if let Some(leak) = leaked(&out, key) {
                    return Err(format!("{leak} in the block"));
                }
            }
            if let Some(c) = out.chars().find(|&c| is_stripped(c)) {
                return Err(format!("{c:?} reached the block"));
            }
            if let Some(tag) = fence_tag_in(&out) {
                return Err(format!("fence tag {tag:?} in the block"));
            }
            Ok(())
        },
    );
    assert!(
        with_keys.get() > 500,
        "{} blocks had a planted key",
        with_keys.get()
    );
    assert!(
        overflowed.get() > 100,
        "{} blocks overflowed",
        overflowed.get()
    );
    assert!(
        after_leads.get() > 100,
        "only {} keys followed an escape or control",
        after_leads.get()
    );
}
