#!/bin/sh
# sessionEnd: queue the finished Cursor chat for distillation. Same guards as hook.sh.
[ -x ~/.local/bin/cal ] || exit 0
~/.local/bin/cal help 2>/dev/null | grep -q ' or cursor$' || exit 0
exec ~/.local/bin/cal session-end --for cursor
