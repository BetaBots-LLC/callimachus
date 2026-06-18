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
    let conn = db::open(&db::default_index_path())?;
    mcp_server::serve(conn).await?;
    Ok(())
}
