//! `cal`: companion terminal CLI for Callimachus. A thin entry point around
//! `callimachus_core::cli_core`. The desktop app can also act as `cal` (when its
//! argv0 is `cal` or it's given a cal subcommand), so the installer can symlink
//! the app to `~/.local/bin/cal`; this binary is for CLI-only installs and isn't bundled
//! into the macOS or Linux app. Windows installs ship it as a sidecar next to the app
//! (build.yml, tauri.windows-sidecars.conf.json; see ../Cargo.toml).
//!
//!   cal search "vector index" -y   # keyword + semantic search
//!   cal recent -n 10               # most recent threads
//!   cal cat 42 | pbcopy            # packed thread context to the clipboard
//!   cal stats                      # corpus overview
//!   cal export 42 --vault ~/Vault  # write an Obsidian note

use callimachus_core::cli_core;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(cli_core::run_and_exit(&args));
}
