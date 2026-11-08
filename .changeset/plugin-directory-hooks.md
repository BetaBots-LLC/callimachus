---
"callimachus": patch
---

The Claude Code plugin's hooks are now short shell scripts inside the plugin (`scripts/hook.sh`, `index.sh`, `snapshot-hook.sh`, `session-end.sh`) instead of inline commands. Anthropic's plugin directory allows no variable other than the plugin root in a hook command, and the scripts are easier to read. What they run is unchanged: the `cal` CLI at `~/.local/bin/cal`, quietly skipped when it is not installed.
