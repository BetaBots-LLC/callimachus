#!/bin/sh
# PreCompact: snapshot the session and keep Claude Code's compaction summary.
[ -x ~/.local/bin/cal ] || exit 0
exec ~/.local/bin/cal snapshot-hook
