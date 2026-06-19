// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let argv: Vec<String> = std::env::args().collect();

    // Dual-mode: when launched with `--mcp`, act as the stdio MCP server instead
    // of opening the GUI. This lets the installed app register *itself* as an MCP
    // server for Claude Code (no separate binary to ship).
    if argv.iter().any(|a| a == "--mcp") {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let result = rt.block_on(async {
            let conn = callimachus_lib::db::open_readonly(&callimachus_lib::db::default_index_path())?;
            callimachus_lib::mcp_server::serve(conn).await
        });
        if let Err(e) = result {
            eprintln!("callimachus --mcp: {e}");
            std::process::exit(1);
        }
        return;
    }

    // Tri-mode: act as the `cal` CLI when invoked as `cal` (argv0 — i.e. via the
    // installer's ~/.local/bin/cal symlink) or given a cal subcommand directly.
    let invoked_as_cal = std::path::Path::new(&argv[0])
        .file_stem()
        .and_then(|s| s.to_str())
        == Some("cal");
    let rest: Vec<String> = argv[1..].to_vec();
    let is_cal_subcommand = rest
        .first()
        .map(|a| callimachus_lib::cli_core::COMMANDS.contains(&a.as_str()))
        .unwrap_or(false);
    if invoked_as_cal || is_cal_subcommand {
        std::process::exit(callimachus_lib::cli_core::run_and_exit(&rest));
    }

    callimachus_lib::run()
}
