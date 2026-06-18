---
"callimachus": patch
"callimachus-vscode": patch
---

Make the VS Code / Cursor extension work without manual setup, and fail gracefully when it can't.

The extension is a thin client over the `cal` CLI, so without it nothing worked — and `cal` wasn't installed by anything. Now:

- **The desktop app installs `cal`.** The one-click "Enable for Claude Code" action symlinks `~/.local/bin/cal` to the app, which runs in `cal` mode when invoked by that name (same dual-mode trick as `--mcp`). No separate binary to ship, no cargo.
- **The extension auto-discovers `cal`** in the app's known install locations (`~/.local/bin`, `/Applications/Callimachus.app/...`, Homebrew, Windows install dirs) before falling back to PATH — zero-config for app users.
- **Friendly empty state.** If `cal` is missing or the index hasn't been built, the extension shows a "Download Callimachus" prompt instead of a raw error, and points to the download page.
