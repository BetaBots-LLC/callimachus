#!/bin/sh
# SessionStart (startup, clear, compact): inject this repo's memory, or a digest of its recent
# sessions, into the Codex session. The plugin ships no binary: it runs the cal CLI the
# Callimachus app links at ~/.local/bin/cal, and exits quietly when that link is missing. Only
# when this cal claims each event (its help says `codex or cursor, one run per event`, from
# the release that ships this plugin on): the plugin can be installed ahead of an older app,
# whose cal would inject a second time beside the app's own Codex hook.
[ -x ~/.local/bin/cal ] || exit 0
~/.local/bin/cal help 2>/dev/null | grep -q 'codex or cursor, one run per event' || exit 0
exec ~/.local/bin/cal hook --for codex
