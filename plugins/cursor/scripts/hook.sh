#!/bin/sh
# sessionStart: inject this repo's memory, or a digest of its recent sessions, into the new
# Cursor chat. The plugin ships no binary: it runs the cal CLI the Callimachus app links at
# ~/.local/bin/cal, and exits quietly when that link is missing. Only when this cal speaks
# Cursor's hook protocol (`--for ... or cursor` in its help): a cal from an older app would
# not, and the plugin can be installed ahead of the app.
[ -x ~/.local/bin/cal ] || exit 0
~/.local/bin/cal help 2>/dev/null | grep -q ' or cursor$' || exit 0
exec ~/.local/bin/cal hook --for cursor
