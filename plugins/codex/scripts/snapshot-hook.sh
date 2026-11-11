#!/bin/sh
# PreCompact: a rolling snapshot of the Codex session before its context is compacted. Same
# guards as hook.sh.
[ -x ~/.local/bin/cal ] || exit 0
~/.local/bin/cal help 2>/dev/null | grep -q 'codex or cursor, one run per event' || exit 0
exec ~/.local/bin/cal snapshot-hook --for codex
