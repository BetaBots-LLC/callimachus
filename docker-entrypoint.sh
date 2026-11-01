#!/bin/sh
# Entrypoint of the callimachus-mcp image (see the Dockerfile). Starts the server as the
# owner of the index ($CALLIMACHUS_DB, or its directory before the file exists), so a host
# folder mounted at /data stays writable and the files SQLite creates there (index.db and
# its -wal / -shm sidecars) belong to the host user rather than root or a fixed image uid.
# With nothing mounted that owner is the image's unprivileged `callimachus` user. A
# root-owned index (rootless Docker and Podman show the host user's files as root) and a
# run with `--user` start the server as they are.
set -eu

server=/usr/local/bin/callimachus-mcp
db=${CALLIMACHUS_DB:-/data/index.db}

if [ "$(id -u)" = 0 ]; then
  if [ -e "$db" ]; then target=$db; else target=$(dirname "$db"); fi
  if [ -e "$target" ]; then
    uid=$(stat -c %u "$target")
    gid=$(stat -c %g "$target")
    # The probe keeps a runtime that withholds setuid (--cap-drop ALL) working as before.
    if [ "$uid" != 0 ] && setpriv --reuid="$uid" --regid="$gid" --clear-groups true 2>/dev/null; then
      # A host uid has no passwd entry here, and root's HOME is closed to it.
      export HOME=/tmp
      exec setpriv --reuid="$uid" --regid="$gid" --clear-groups "$server" "$@"
    fi
  fi
fi
exec "$server" "$@"
