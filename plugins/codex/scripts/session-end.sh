#!/bin/sh
# SessionEnd: queue the finished Codex session for distillation. Same guards as hook.sh; Codex
# gives a SessionEnd hook at most 3 s.
[ -x ~/.local/bin/cal ] || exit 0
~/.local/bin/cal help 2>/dev/null | grep -q 'codex or cursor, one run per event' || exit 0
exec ~/.local/bin/cal session-end --for codex
