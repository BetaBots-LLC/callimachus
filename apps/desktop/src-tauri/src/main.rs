// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::Path;

fn main() {
    let argv: Vec<String> = std::env::args().collect();

    // Remove every agent integration (skill, MCP entries, hooks, `cal` link) and exit. The
    // Windows uninstaller runs this so hooks don't keep pointing at a deleted app; it also works
    // by hand on macOS/Linux before trashing the app. Only as the FIRST argument (which is how
    // the uninstaller passes it): matching anywhere would let `cal search --uninstall-integration`
    // wipe every integration instead of searching for the string.
    if argv.get(1).map(String::as_str) == Some("--uninstall-integration") {
        std::process::exit(uninstall_integration());
    }

    // Dual-mode: `--mcp` runs the stdio MCP server instead of the GUI, so the installed app
    // registers *itself* as an MCP server (no separate binary to ship). The same server
    // process as the `callimachus-mcp` binary, so the two can't drift. Only as the FIRST
    // argument, which is how every registration passes it: matching anywhere would turn
    // `cal search --mcp` into an MCP server waiting on stdin instead of a search.
    if argv.get(1).map(String::as_str) == Some("--mcp") {
        std::process::exit(callimachus_lib::mcp_server::serve_stdio(
            "callimachus --mcp",
            &callimachus_lib::db::default_index_path(),
        ));
    }

    // Tri-mode: act as the `cal` CLI when invoked as `cal` (argv0 — i.e. via the
    // installer's ~/.local/bin/cal symlink) or given a cal subcommand directly (which is how
    // the installed Claude Code hooks call the app).
    let argv0 = argv.first().map(String::as_str).unwrap_or_default();
    let rest: Vec<String> = argv.iter().skip(1).cloned().collect();
    let is_cal_subcommand = rest
        .first()
        .map(|a| callimachus_lib::cli_core::COMMANDS.contains(&a.as_str()))
        .unwrap_or(false);
    if invoked_as_cal(argv0) || is_cal_subcommand {
        #[cfg(windows)]
        attach_parent_console();
        std::process::exit(callimachus_lib::cli_core::run_and_exit(&rest));
    }

    callimachus_lib::run()
}

/// True when the process was started under the name `cal`.
fn invoked_as_cal(argv0: &str) -> bool {
    let named_cal = |p: &str| Path::new(p).file_stem().and_then(|s| s.to_str()) == Some("cal");
    if named_cal(argv0) {
        return true;
    }
    // AppImage: the runtime execs the bundled binary by its in-image path, so argv[0] never
    // says `cal`. The name the user (or a hook) actually invoked, e.g. the ~/.local/bin/cal
    // symlink to the .AppImage, arrives in $ARGV0. Trusted only when this binary runs inside
    // the AppImage mount: a deb/rpm binary started from another AppImage app's terminal
    // inherits that app's APPIMAGE/ARGV0, which must not flip the GUI into CLI mode.
    #[cfg(target_os = "linux")]
    {
        if callimachus_lib::integration::running_from_appimage() {
            if let Some(name) = std::env::var_os("ARGV0") {
                return named_cal(&name.to_string_lossy());
            }
        }
    }
    false
}

/// `--uninstall-integration`: best-effort removal of everything the integration installed.
///
/// The Windows uninstaller also runs this when a manually downloaded setup.exe upgrades,
/// downgrades or reinstalls the app (Tauri's "uninstall before installing" path, which isn't
/// update mode). So the removal records what it took out in a marker in the data dir (which
/// survives an uninstall), and the next launch's `integration::refresh_installed` puts those
/// pieces back and deletes the marker. A real uninstall simply never launches again.
fn uninstall_integration() -> i32 {
    #[cfg(windows)]
    attach_parent_console();
    match callimachus_lib::integration::uninstall_all_with_marker() {
        Ok(()) => {
            println!("Removed the Callimachus agent integrations.");
            0
        }
        Err(e) => {
            eprintln!("callimachus: removing the agent integrations: {e:#}");
            1
        }
    }
}

/// Release builds use the GUI subsystem, so a `cal` typed in cmd or PowerShell would start with
/// no console and print nothing. Borrow the parent terminal's console, but only when stdout
/// isn't already connected: hooks, MCP and the VS Code extension pass pipes, which must stay
/// untouched. (cmd doesn't wait for GUI-subsystem programs, so the prompt can come back before
/// the output does; a console-subsystem `cal.exe` sidecar is the full fix.)
#[cfg(windows)]
fn attach_parent_console() {
    type Handle = *mut core::ffi::c_void;
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(std_handle: u32) -> Handle;
        fn AttachConsole(process_id: u32) -> i32;
    }
    // SAFETY: both are plain Win32 calls with no pointer arguments. A failed AttachConsole (no
    // parent console, or one already attached) leaves the process as it was.
    unsafe {
        let stdout = GetStdHandle(STD_OUTPUT_HANDLE);
        if stdout.is_null() || stdout as isize == -1 {
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}
