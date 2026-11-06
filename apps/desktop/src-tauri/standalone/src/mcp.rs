//! Standalone `callimachus-mcp` binary: a thin entry point around
//! `callimachus_lib::mcp_server::serve_stdio`. Register with a client, e.g.:
//!   claude mcp add callimachus -- callimachus-mcp
//! (Optionally set CALLIMACHUS_DB to point at a specific index.db.)
//!
//! The desktop app runs the same server via `--mcp`, so the in-app installer registers the
//! app itself; this binary is for MCP-only installs and isn't bundled into the macOS or Linux
//! app. Windows installs ship it as a sidecar next to the app (build.yml,
//! tauri.windows-sidecars.conf.json).

fn main() {
    // This binary never indexes; only the desktop app does. serve_stdio says so in the
    // client's server log when there is no index yet (an npx / MCP-registry install).
    std::process::exit(callimachus_lib::mcp_server::serve_stdio(
        "callimachus-mcp",
        &callimachus_lib::db::default_index_path(),
    ));
}
