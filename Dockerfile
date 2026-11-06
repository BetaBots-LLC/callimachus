# Builds and runs the standalone `callimachus-mcp` server over stdio. Used by MCP
# registries (e.g. Glama) to verify the server starts and answers introspection
# (tools/list). The index is created empty on first open, so no real data is
# needed to pass verification. CI builds this image and runs that check
# (.github/workflows/docker.yml).
#
# Build from the repo root. Run it as is (it keeps an index inside the container), or
# against an existing index (for example on a distro too old for the prebuilt binary) by
# mounting the folder that holds index.db at /data:
#   docker build -t callimachus-mcp .
#   docker run -i --rm -v "<folder with index.db>:/data" callimachus-mcp
#
# Debian 13 "trixie" (glibc 2.41) in both stages on purpose: the ONNX Runtime
# prebuilt that `ort` (fastembed's backend) links needs glibc >= 2.38. On bookworm
# (2.36) the link fails with undefined `__isoc23_*` symbols, the same reason
# release builds run on Ubuntu 24.04. The runtime stage must not be older than
# the build stage, or the binary won't load.
#
# Two stages: GTK/WebKit are needed to *build* the server (it shares the desktop
# crate, which depends on Tauri), not to run it. The linker's --as-needed drops
# them, so the binary needs only libc, libstdc++, libgcc_s, libssl/libcrypto and
# libdbus at runtime (the DT_NEEDED list of the released Linux binary). The
# runtime image carries none of the Rust toolchain, -dev packages or target/.

FROM rust:1-trixie AS build

WORKDIR /app

# Build-only dependencies. Tauri's -sys crates find GTK/WebKit through pkg-config
# even though the server never opens a window; libssl-dev is for native-tls and
# libdbus-1-dev for keyring's Secret Service backend.
RUN apt-get update \
  && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev libdbus-1-dev libwebkit2gtk-4.1-dev \
  && rm -rf /var/lib/apt/lists/*

# Only the Rust crate is needed (it is self-contained, with its own Cargo.lock).
COPY apps/desktop/src-tauri/ ./

# tauri::generate_context!() resolves frontendDist ("../dist") at compile time;
# the MCP server never serves it, so an empty placeholder is enough.
# --locked: build exactly the dependency versions in Cargo.lock.
RUN mkdir -p /dist \
  && cargo build --release --locked -p callimachus-standalone --bin callimachus-mcp

FROM debian:trixie-slim

# The binary's runtime libraries, plus CA roots for native-tls HTTPS. libssl3t64
# and libstdc++6 already ship in the slim base; naming them keeps the image from
# relying on that.
RUN apt-get update \
  && apt-get install -y --no-install-recommends \
    ca-certificates libdbus-1-3 libssl3t64 libstdc++6 \
  && rm -rf /var/lib/apt/lists/*

COPY --from=build /app/target/release/callimachus-mcp /usr/local/bin/callimachus-mcp
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh

# Fail the build here, listing what's missing, if the binary ever links a library
# this stage doesn't install (rather than at `docker run` with a loader error).
RUN ! ldd /usr/local/bin/callimachus-mcp | grep 'not found'

# No fixed `USER`: docker-entrypoint.sh starts the server as the owner of the index, so a
# mounted host folder (owned by the host user, often uid 1000) stays writable. A fixed uid
# made SQLite fail there with "attempt to write a readonly database". With nothing
# mounted, the index lives in /data (the default path is a per-user data dir), owned by
# the unprivileged `callimachus` user and sticky world-writable like /tmp, so
# `--user <uid>` works without a mount too. `db::open` creates the file on first run.
RUN chmod 755 /usr/local/bin/docker-entrypoint.sh \
  && useradd --uid 10001 --no-create-home callimachus \
  && mkdir -p /data \
  && chown callimachus:callimachus /data \
  && chmod 1777 /data
ENV CALLIMACHUS_DB=/data/index.db

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
