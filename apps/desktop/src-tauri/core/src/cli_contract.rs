//! The versioned `cal --json` contract. The fifteen query outputs other tools read (the VS Code
//! extension, PR auditors, scripts) come as these envelopes, each opening with
//! `schema_version`, when the caller asks for a version: `--json-version 1`, or
//! `CAL_JSON_VERSION=1` in the environment beside `--json`. Plain `--json` keeps printing the
//! bare arrays and objects it printed before versioning, because the VS Code extension
//! already released reads those and updates on its own schedule, not with the app; that bare
//! output is unversioned and may change in a future major release. The JSON Schemas of the
//! envelopes are generated from the same types into `schemas/cal/` (served at
//! https://callimachus.app/schemas/cal/). A drift test fails when a generated schema differs
//! from the checked-in file, so every shape change lands as a reviewed diff.
//!
//! Versioning: `schema_version` is a major version. Adding a field is not a new version
//! (readers ignore fields they don't know); removing or renaming one, or changing its type,
//! bumps [`CAL_SCHEMA_VERSION`] and publishes `<command>.v<N>.json` next to the old files.

use schemars::generate::SchemaSettings;
use schemars::JsonSchema;
use serde::Serialize;

use crate::{cost, issues, knowledge, report, search};

/// The major version every `cal --json` envelope carries in `schema_version`.
pub const CAL_SCHEMA_VERSION: u32 = 1;

/// The environment variable that asks for an envelope version, like `--json-version`. A
/// caller that must also run an older `cal` (the VS Code extension) sets this instead of the
/// flag: an older `cal` ignores the variable and prints its bare output, where it would
/// refuse the unknown flag.
pub const JSON_VERSION_ENV: &str = "CAL_JSON_VERSION";

/// Where the schemas are published (`apps/web/public/schemas/cal/`).
pub const SCHEMA_BASE_URL: &str = "https://callimachus.app/schemas/cal/";

/// A list output (`search`, `recent`, `related`, `decisions`, `gotchas`, `commits`, `issues`).
#[derive(Debug, Serialize, JsonSchema)]
pub struct Listing<'a, T> {
    /// Major version of this payload's shape. A reader refuses a version newer than it knows.
    pub schema_version: u32,
    /// The rows, best match (or newest) first.
    pub results: &'a [T],
    /// Present only when `results` is empty because this machine couldn't run the query (no
    /// embedding model for `decisions` / `gotchas`), so a reader can tell that from no match.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub notice: Option<String>,
}

impl<'a, T> Listing<'a, T> {
    pub fn new(results: &'a [T]) -> Self {
        Self {
            schema_version: CAL_SCHEMA_VERSION,
            results,
            notice: None,
        }
    }

    pub fn with_notice(mut self, notice: impl Into<String>) -> Self {
        self.notice = Some(notice.into());
        self
    }
}

/// An object output (`stats`, `memory`): the object's own fields after `schema_version`, so
/// a reader of the unversioned object keeps working.
#[derive(Debug, Serialize, JsonSchema)]
pub struct Versioned<'a, T> {
    /// Major version of this payload's shape. A reader refuses a version newer than it knows.
    pub schema_version: u32,
    #[serde(flatten)]
    pub value: &'a T,
}

impl<'a, T> Versioned<'a, T> {
    pub fn new(value: &'a T) -> Self {
        Self {
            schema_version: CAL_SCHEMA_VERSION,
            value,
        }
    }
}

/// `cal cat <id> --json`: the thread's identity plus the packed transcript `cal cat` prints.
#[derive(Debug, Serialize, JsonSchema)]
pub struct CatOutput {
    /// Major version of this payload's shape. A reader refuses a version newer than it knows.
    pub schema_version: u32,
    pub id: i64,
    pub source: String,
    pub title: Option<String>,
    #[serde(rename = "projectPath")]
    pub project_path: Option<String>,
    #[serde(rename = "updatedAt")]
    pub updated_at: Option<i64>,
    /// The packed markdown transcript (secrets redacted), trimmed to the CLI's char budget.
    pub markdown: String,
}

/// `cal audit-pr`: the one-call bundle for an external PR reviewer.
#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AuditPrOutput {
    /// Major version of this payload's shape. A reader refuses a version newer than it knows.
    #[serde(rename = "schema_version")]
    pub schema_version: u32,
    /// The repo's project key (worktrees resolve to the main repo).
    pub repo: String,
    /// Per input sha, as given: the threads inferred to have produced it (empty when none).
    #[schemars(with = "std::collections::BTreeMap<String, AuditSha>")]
    pub by_sha: serde_json::Map<String, serde_json::Value>,
    /// Per changed file, as given: prior threads that mentioned it, newest first.
    #[schemars(with = "std::collections::BTreeMap<String, AuditFile>")]
    pub by_file: serde_json::Map<String, serde_json::Value>,
    /// The repo's recurring errors over the last 180 days.
    pub recurring_errors: Vec<issues::IssueCluster>,
    /// The repo's decisions, gotchas and open TODOs (null when it can't be read).
    pub project_memory: Option<knowledge::ProjectMemory>,
    pub notes: AuditNotes,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AuditSha {
    pub threads_produced: Vec<AuditCommitThread>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AuditCommitThread {
    pub id: i64,
    pub title: Option<String>,
    /// Files the thread and the commit share (a confidence proxy, not normalized).
    pub overlap: i64,
    pub knowledge: Option<AuditKnowledge>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AuditFile {
    pub threads: Vec<AuditFileThread>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AuditFileThread {
    pub id: i64,
    pub title: Option<String>,
    pub knowledge: Option<AuditKnowledge>,
}

/// A thread's distilled reasoning, null when it could not be read.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AuditKnowledge {
    pub summary: Option<String>,
    pub decisions: Vec<String>,
    pub gotchas: Vec<String>,
    pub open_todos: Vec<String>,
    /// Whether the thread was distilled at all.
    pub extracted: bool,
    /// The thread changed after it was distilled, so its facts predate the latest turns.
    pub stale: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AuditNotes {
    pub distillation_enabled: bool,
    /// Thread-commit links refreshed for the branch before answering.
    pub links_refreshed: usize,
    /// False when distillation is off: knowledge fields are null, the rest still fills in.
    pub thread_knowledge_available: bool,
}

/// One published schema: the command, what its output is, and the schema itself.
pub struct CalSchema {
    pub command: &'static str,
    pub schema: serde_json::Value,
}

impl CalSchema {
    /// The file name under `schemas/cal/` (and the URL under [`SCHEMA_BASE_URL`]).
    pub fn file_name(&self) -> String {
        format!("{}.v{CAL_SCHEMA_VERSION}.json", self.command)
    }
}

/// The JSON Schema of every versioned `cal --json` output, in `COMMANDS` order.
pub fn schemas() -> Vec<CalSchema> {
    use crate::gitlink::TimelineRow;
    vec![
        schema::<Listing<search::SearchHit>>(
            "search",
            "Message-level matches for a query, best first. Snippets keep the \\u0001 / \\u0002 \
             match markers around highlighted terms.",
        ),
        schema::<Listing<search::ThreadSummary>>(
            "related",
            "Threads semantically related to the given text, best first.",
        ),
        schema::<Listing<search::ThreadSummary>>("recent", "Threads, most recently updated first."),
        schema::<CatOutput>("cat", "One thread as a packed markdown transcript."),
        schema::<Versioned<search::Stats>>("stats", "An overview of the whole index."),
        schema::<Listing<knowledge::RecallHit>>(
            "decisions",
            "Decisions semantically recalled for a query, best first.",
        ),
        schema::<Listing<knowledge::RecallHit>>(
            "gotchas",
            "Gotchas semantically recalled for a query, best first.",
        ),
        schema::<Versioned<knowledge::ProjectMemory>>(
            "memory",
            "A project's decisions, gotchas and open TODOs.",
        ),
        schema::<Listing<TimelineRow>>(
            "commits",
            "Commits inferred from this repo's threads, newest first.",
        ),
        schema::<crate::why::WhyOutput>(
            "why",
            "The commits that last changed a file or line (git blame), newest first, each with \
             the sessions inferred to have produced it and their decisions and gotchas.",
        ),
        schema::<crate::trace::TraceExport>(
            "trace",
            "What `cal trace export` wrote (or, with --dry-run, would write): Agent Trace \
             records (agent-trace.dev) for the commits the project's sessions produced.",
        ),
        schema::<Listing<issues::Issue>>(
            "issues",
            "Recurring errors over the last 180 days, most frequent first, each with the \
             sessions that hit it and, once a linked commit fixed it, the fix and its playbook.",
        ),
        schema::<AuditPrOutput>(
            "audit-pr",
            "Provenance, per-file history, recurring errors and memory for a pull request.",
        ),
        schema::<Versioned<cost::CostReport>>(
            "cost",
            "Estimated spend (list-price equivalents) per model, plus the breakdown --by asks \
             for: sessions with waste signals, commits, projects or local days.",
        ),
        schema::<Versioned<report::Report>>(
            "report",
            "Your agents, so far: sessions and hours per agent, estimated spend, the most \
             frequent recurring errors and their fixes, tasks asked again, project memory, the \
             files agents changed most and the value ledger. A section with no data is left out.",
        ),
    ]
}

/// A root schema for `T` describing how it serializes (an `Option` field is required and
/// nullable, a `skip_serializing_if` field is optional), with the published `$id` first.
fn schema<T: JsonSchema>(command: &'static str, description: &str) -> CalSchema {
    let generated = SchemaSettings::draft2020_12()
        .for_serialize()
        .into_generator()
        .into_root_schema_for::<T>();
    let mut generated = generated.to_value();
    drop_numeric_formats(&mut generated);
    let serde_json::Value::Object(body) = generated else {
        unreachable!("a struct's schema is an object");
    };
    let mut out = serde_json::Map::new();
    if let Some(meta) = body.get("$schema") {
        out.insert("$schema".into(), meta.clone());
    }
    out.insert(
        "$id".into(),
        format!("{SCHEMA_BASE_URL}{command}.v{CAL_SCHEMA_VERSION}.json").into(),
    );
    out.insert("title".into(), format!("cal {command} --json").into());
    out.insert("description".into(), description.into());
    for (k, v) in body {
        if !matches!(k.as_str(), "$schema" | "title" | "description") {
            out.insert(k, v);
        }
    }
    CalSchema {
        command,
        schema: serde_json::Value::Object(out),
    }
}

/// Remove the Rust-width `format`s schemars puts on numbers (`int64`, `uint32`, `float`...).
/// They are no JSON Schema format, and Ajv, the most used validator, refuses to compile a
/// schema with an unknown format in its default strict mode. The `minimum: 0` of an unsigned
/// field stays. Only string values are removed: a property named `format` is an object.
fn drop_numeric_formats(schema: &mut serde_json::Value) {
    const RUST_FORMATS: &[&str] = &[
        "int8", "int16", "int32", "int64", "uint8", "uint16", "uint32", "uint64", "uint", "int",
        "float", "double",
    ];
    match schema {
        serde_json::Value::Object(obj) => {
            if obj
                .get("format")
                .and_then(|f| f.as_str())
                .is_some_and(|f| RUST_FORMATS.contains(&f))
            {
                obj.remove("format");
            }
            obj.values_mut().for_each(drop_numeric_formats);
        }
        serde_json::Value::Array(list) => list.iter_mut().for_each(drop_numeric_formats),
        _ => {}
    }
}

/// The drift check both contracts use (`schemas/cal/` here, `schemas/mcp/` in mcp_server):
/// regenerate the files, rewrite any that differ and report them, so a shape change shows up
/// as a diff to review and commit (rerun and it passes).
#[cfg(test)]
pub(crate) mod drift {
    use std::path::{Path, PathBuf};

    /// The repo root, when the crate is built from a checkout (not from a packaged crate).
    pub fn repo_root() -> Option<PathBuf> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../..");
        root.join("apps/web")
            .is_dir()
            .then(|| root.canonicalize().unwrap())
    }

    /// Write each `(file name, schema)` into every dir in `dirs` (relative to the repo root)
    /// where its pretty JSON differs, and return what changed. A file in a dir that `ours`
    /// claims but no generated schema writes is reported too (a stale schema to delete).
    /// Files compare after CRLF is normalized: a Windows checkout with core.autocrlf=true
    /// (which `.gitattributes` also overrides for these dirs) must not fail and rewrite all.
    pub fn sync(
        dirs: &[&str],
        files: &[(String, serde_json::Value)],
        ours: impl Fn(&str) -> bool,
    ) -> Vec<String> {
        let Some(root) = repo_root() else {
            return Vec::new();
        };
        let mut changed = Vec::new();
        for dir in dirs {
            let dir = root.join(dir);
            std::fs::create_dir_all(&dir).unwrap();
            for (name, schema) in files {
                let path = dir.join(name);
                let text = format!("{}\n", serde_json::to_string_pretty(schema).unwrap());
                let on_disk = std::fs::read_to_string(&path).ok();
                if !on_disk.is_some_and(|t| same_text(&t, &text)) {
                    std::fs::write(&path, text).unwrap();
                    changed.push(path.display().to_string());
                }
            }
            for entry in std::fs::read_dir(&dir).unwrap() {
                let name = entry.unwrap().file_name().to_string_lossy().into_owned();
                if ours(&name) && !files.iter().any(|(f, _)| *f == name) {
                    changed.push(format!(
                        "{} (nothing generates it: delete it)",
                        dir.join(name).display()
                    ));
                }
            }
        }
        changed
    }

    /// A checked-in file equals the generated text, whatever its line endings.
    pub fn same_text(on_disk: &str, generated: &str) -> bool {
        on_disk.replace("\r\n", "\n") == generated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The canonical copy is `schemas/cal/`; the site serves an identical copy from its
    /// public dir, since a static host can't reach outside its app (and a symlink breaks on a
    /// Windows checkout without core.symlinks).
    const SCHEMA_DIRS: [&str; 2] = ["schemas/cal", "apps/web/public/schemas/cal"];

    /// The drift test ([`drift::sync`]). A stale current-version file for a command that no
    /// longer has a schema fails too.
    #[test]
    fn cal_json_schemas_match_the_checked_in_files() {
        let files: Vec<_> = schemas()
            .into_iter()
            .map(|s| (s.file_name(), s.schema))
            .collect();
        let current = format!(".v{CAL_SCHEMA_VERSION}.json");
        let changed = drift::sync(&SCHEMA_DIRS, &files, |name| name.ends_with(&current));
        assert!(
            changed.is_empty(),
            "the cal --json contract changed; rewrote:\n  {}\nReview the diff and commit it. A \
             removed or renamed field or a changed type also bumps CAL_SCHEMA_VERSION.",
            changed.join("\n  ")
        );
    }

    #[test]
    fn every_schema_is_published_under_its_command() {
        let names: Vec<_> = schemas().iter().map(|s| s.command).collect();
        assert_eq!(names.len(), 15);
        for s in schemas() {
            assert!(
                crate::cli_core::COMMANDS.contains(&s.command),
                "{} is not a cal command",
                s.command
            );
            assert_eq!(
                s.schema["$id"],
                format!("{SCHEMA_BASE_URL}{}", s.file_name()),
                "{}",
                s.command
            );
            // Every envelope opens with the version, required, so a reader can check it first.
            assert_eq!(
                s.schema["properties"]["schema_version"]["type"], "integer",
                "{}",
                s.command
            );
            // Ajv's default strict mode refuses an unknown format.
            assert!(
                !s.schema.to_string().contains(r#""format":"#),
                "{}",
                s.command
            );
            let required = s.schema["required"].as_array().unwrap();
            assert!(
                required.iter().any(|r| r == "schema_version"),
                "{}",
                s.command
            );
        }
    }

    #[test]
    fn envelopes_put_the_version_first() {
        let rows = [1, 2];
        let json = serde_json::to_string(&Listing::new(&rows)).unwrap();
        assert_eq!(json, r#"{"schema_version":1,"results":[1,2]}"#);
        let json =
            serde_json::to_string(&Listing::<i64>::new(&[]).with_notice("no model")).unwrap();
        assert_eq!(
            json,
            r#"{"schema_version":1,"results":[],"notice":"no model"}"#
        );

        #[derive(Serialize)]
        struct S {
            threads: i64,
        }
        let json = serde_json::to_string(&Versioned::new(&S { threads: 3 })).unwrap();
        assert_eq!(json, r#"{"schema_version":1,"threads":3}"#);
    }

    #[test]
    fn a_crlf_checkout_matches_its_schema() {
        assert!(drift::same_text(
            "{\r\n  \"a\": 1\r\n}\r\n",
            "{\n  \"a\": 1\n}\n"
        ));
        assert!(!drift::same_text(
            "{\n  \"a\": 2\n}\n",
            "{\n  \"a\": 1\n}\n"
        ));
    }
}
