#!/bin/sh
# SessionEnd: queue the finished session for distillation.
[ -x ~/.local/bin/cal ] || exit 0
exec ~/.local/bin/cal session-end
