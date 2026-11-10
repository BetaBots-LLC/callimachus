//! Source-scan tests for promises the type system cannot keep: what may reach the network
//! (SECURITY.md, "What the app sends over the network") and what may write stdout, which is
//! the MCP server's JSON-RPC stream (a stray line there breaks the agent's connection).
//! `#![deny(clippy::print_stdout)]` in lib.rs covers `print!` / `println!`; this also covers
//! a raw stdout handle. The app crate's network clients are checked in its `telemetry.rs`.

/// Every `.rs` file under `dir`, as (path relative to it with '/' separators, source with
/// comment lines removed, so a doc comment may name a client without counting as one).
fn rust_sources(dir: &std::path::Path) -> Vec<(String, String)> {
    fn walk(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<(String, String)>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .expect("read the source folder")
            .flatten()
            .collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let path = e.path();
            if path.is_dir() {
                walk(root, &path, out);
            } else if path.extension().is_some_and(|x| x == "rs") {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                let code = std::fs::read_to_string(&path)
                    .unwrap()
                    .lines()
                    .filter(|l| !l.trim_start().starts_with("//"))
                    .collect::<Vec<_>>()
                    .join("\n");
                out.push((rel, code));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out
}

/// (file, needle) pairs.
type Uses = Vec<(String, String)>;

/// For each needle, the files (from `sources`) that use it outside `allowed`, and the
/// allowed (file, needle) pairs that no longer occur.
fn confinement_violations(
    sources: &[(String, String)],
    needles: &[&str],
    allowed: &[(&str, &str)],
) -> (Uses, Uses) {
    let mut stray = Vec::new();
    for (file, code) in sources {
        for needle in needles {
            if code.contains(needle) && !allowed.contains(&(file.as_str(), *needle)) {
                stray.push((file.clone(), needle.to_string()));
            }
        }
    }
    let stale = allowed
        .iter()
        .filter(|(file, needle)| {
            !sources
                .iter()
                .any(|(f, code)| f == file && code.contains(needle))
        })
        .map(|(f, n)| (f.to_string(), n.to_string()))
        .collect();
    (stray, stale)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spellings of a network client: an HTTP client crate, a raw socket, the model download
    /// (fastembed's `try_new` fetches from Hugging Face; `try_new_from_user_defined` reads
    /// local files), a shelled-out downloader. This file is skipped: it names them all.
    const NETWORK: &[&str] = &[
        "reqwest::",
        "genai::",
        "ureq::",
        "hyper::",
        "hf_hub::",
        "std::net",
        "tokio::net",
        "TcpStream",
        "UdpSocket",
        "TcpListener",
        "TextEmbedding::try_new(",
        "Command::new(\"curl\"",
        "Command::new(\"wget\"",
    ];

    /// The core's egress, one row per path in SECURITY.md: the LLM providers (chat, distill,
    /// Ask, briefs, synthesis) through `agent`, and the embedding-model download in `embed`
    /// (the app, and `cal embed` without `--no-download`; hooks and the MCP server load it
    /// cached-only).
    const EGRESS: &[(&str, &str)] = &[
        ("agent/mod.rs", "reqwest::"),
        ("agent/mod.rs", "genai::"),
        ("embed/mod.rs", "TextEmbedding::try_new("),
    ];

    #[test]
    fn egress_is_confined() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let sources: Vec<_> = rust_sources(&src)
            .into_iter()
            .filter(|(f, _)| f != "invariants.rs")
            .collect();
        assert!(sources.len() > 50, "scanned {} files", sources.len());
        let (stray, stale) = confinement_violations(&sources, NETWORK, EGRESS);
        assert!(
            stray.is_empty(),
            "a network client outside the reviewed egress paths: {stray:?}. Everything that \
             leaves the machine is listed in SECURITY.md (\"What the app sends over the \
             network\"); if this one is meant to, describe it there and add it to EGRESS here"
        );
        assert!(
            stale.is_empty(),
            "EGRESS lists a client that is gone: {stale:?}. Remove it here and from SECURITY.md"
        );
    }

    /// stdout belongs to `cal` (`cli_core`, `headless`) and to the MCP server's transport. A
    /// raw handle anywhere else would interleave with the JSON-RPC stream when that code runs
    /// inside the server (the print macros are denied crate-wide by clippy).
    #[test]
    fn stdout_is_confined() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let sources: Vec<_> = rust_sources(&src)
            .into_iter()
            .filter(|(f, _)| f != "invariants.rs")
            .collect();
        let allowed = [
            ("cli_core.rs", "stdout()"),
            ("headless.rs", "stdout()"),
            ("mcp_server.rs", "stdout()"),
        ];
        let (stray, _) = confinement_violations(&sources, &["stdout()"], &allowed);
        assert!(
            stray.is_empty(),
            "stdout written outside cal and the MCP transport: {stray:?}. Use app_log! (stderr)"
        );
        assert!(
            sources
                .iter()
                .any(|(f, code)| f == "mcp_server.rs" && code.contains("tokio::io::stdout()")),
            "the scan reads the MCP transport"
        );
    }

    #[test]
    fn the_scan_skips_comments_and_flags_strays() {
        let sources = vec![
            (
                "a.rs".to_string(),
                "let c = reqwest::Client::new();".to_string(),
            ),
            ("b.rs".to_string(), "fn f() {}".to_string()),
        ];
        let (stray, stale) =
            confinement_violations(&sources, &["reqwest::"], &[("b.rs", "reqwest::")]);
        assert_eq!(stray, vec![("a.rs".to_string(), "reqwest::".to_string())]);
        assert_eq!(stale, vec![("b.rs".to_string(), "reqwest::".to_string())]);
        let dir = crate::db::test_support::TestDir::new("scan");
        let root = dir.parent().unwrap();
        std::fs::write(root.join("c.rs"), "// reqwest:: in a comment\nfn f() {}\n").unwrap();
        let scanned = rust_sources(root);
        assert_eq!(scanned.len(), 1);
        assert!(!scanned[0].1.contains("reqwest"));
    }
}
