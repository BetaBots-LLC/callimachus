//! Standalone `callimachus-mcp` binary — a thin entry point around
//! `callimachus_lib::mcp_server::serve`. Register with a client, e.g.:
//!   claude mcp add callimachus -- callimachus-mcp
//! (Optionally set CALLIMACHUS_DB to point at a specific index.db.)
//!
//! The desktop app can also act as this server via `--mcp`, so the in-app
//! installer registers the app itself — no separate binary required.

use callimachus_lib::{db, mcp_server};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = db::default_index_path();
    // This binary never indexes; only the desktop app does. When there is no index yet (an
    // npx / MCP-registry install without the app), say so in the client's server log. The
    // tools themselves also answer with setup guidance until threads show up.
    if !path.exists() {
        eprintln!(
            "callimachus-mcp: no index at {}; creating an empty one. Install and open the \
             Callimachus desktop app to index your agent history \
             (https://github.com/BetaBots-LLC/callimachus/releases/latest), or set CALLIMACHUS_DB.",
            path.display()
        );
    }
    // Writable: the server exposes write tools (complete_todo, record_*). WAL +
    // busy_timeout let it coexist with the desktop app's writer; the sidecar open also
    // waits out an index upgrade another process is running.
    let conn = match db::open_sidecar(&path) {
        Ok(conn) => conn,
        // A lock held past that wait is almost always an upgrade in progress: say so calmly
        // instead of SQLite's raw "database is locked".
        Err(e) if db::is_busy(&e) => {
            eprintln!(
                "callimachus-mcp: the index is busy (an upgrade may be running); try again shortly"
            );
            std::process::exit(1);
        }
        Err(e) => return Err(e.into()),
    };
    mcp_server::serve(conn).await?;
    Ok(())
}
