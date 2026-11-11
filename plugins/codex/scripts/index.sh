#!/bin/sh
# SessionStart (startup, resume): keep the index fresh when the desktop app is not running.
# Only when this cal lists `cal index` in its help: the plugin can update ahead of the app, and
# an older cal answers an unknown command with exit 1, a hook error in every session.
[ -x ~/.local/bin/cal ] || exit 0
~/.local/bin/cal help 2>/dev/null | grep -q '^  cal index ' || exit 0
exec ~/.local/bin/cal index --if-stale
