#!/bin/sh
# SessionStart (startup, clear, compact): inject this repo's memory, or a digest of its recent
# sessions. The plugin ships no binary: it runs the cal CLI the Callimachus app links at
# ~/.local/bin/cal, and exits quietly when that link is missing.
[ -x ~/.local/bin/cal ] || exit 0
exec ~/.local/bin/cal hook
