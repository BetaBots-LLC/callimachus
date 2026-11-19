---
"callimachus": patch
---

Pick the terminal Resume opens on macOS.

- **Settings > General > Terminal.** Resume and Open in CLI always opened Terminal.app. On a Mac with another terminal installed you can now choose it: Ghostty, iTerm2, WezTerm, kitty or Alacritty. The new window opens in the session's folder, runs the agent in your login shell (so the `claude`, `codex` and `cursor-agent` your shell finds are the ones that start) and stays open on a shell when the agent exits.
- **Nothing changes until you pick.** The default is still Terminal.app, and a chosen terminal that was later uninstalled falls back to it instead of failing. The row only shows when there is more than one terminal to choose from. Linux still reads `$TERMINAL`, and Windows still opens a console.
- Launching was checked on Terminal and Ghostty 1.4; iTerm2, WezTerm, kitty and Alacritty follow their documented command flags.
