// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Dual-mode: when launched with `--mcp`, act as the stdio MCP server instead
    // of opening the GUI. This lets the installed app register *itself* as an MCP
    // server for Claude Code (no separate binary to ship).
    if std::env::args().any(|a| a == "--mcp") {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let result = rt.block_on(async {
            let conn = callimachus_lib::db::open(&callimachus_lib::db::default_index_path())?;
            callimachus_lib::mcp_server::serve(conn).await
        });
        if let Err(e) = result {
            eprintln!("callimachus --mcp: {e}");
            std::process::exit(1);
        }
        return;
    }
    callimachus_lib::run()
}
