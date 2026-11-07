//! Standalone `callimachus-mcp` binary: a thin entry point around
//! `callimachus_core::mcp_server::serve_stdio`. Register with a client, e.g.:
//!   claude mcp add callimachus -- callimachus-mcp
//! (Optionally set CALLIMACHUS_DB to point at a specific index.db.)
//!
//! The desktop app runs the same server via `--mcp`, so the in-app installer registers the
//! app itself; this binary is for MCP-only installs and isn't bundled into the macOS or Linux
//! app. Windows installs ship it as a sidecar next to the app (build.yml,
//! tauri.windows-sidecars.conf.json).

fn main() {
    // This binary never indexes; the desktop app, or `cal index` / `cal watch`, builds the
    // index. serve_stdio says so in the client's server log when there is no index yet (an
    // npx / MCP-registry install).
    std::process::exit(callimachus_core::mcp_server::serve_stdio(
        "callimachus-mcp",
        &callimachus_core::db::default_index_path(),
    ));
}
