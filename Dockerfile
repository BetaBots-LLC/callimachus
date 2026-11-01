# Builds and runs the standalone `callimachus-mcp` server over stdio. Used by MCP
# registries (e.g. Glama) to verify the server starts and answers introspection
# (tools/list). The index is created empty on first open, so no real data is
# needed to pass verification. CI builds this image and runs that check
# (.github/workflows/docker.yml).
#
# Build from the repo root:
#   docker build -t callimachus-mcp .
#
# Debian 13 "trixie" (glibc 2.41) on purpose: the ONNX Runtime prebuilt that `ort`
# (fastembed's backend) links needs glibc >= 2.38. On bookworm (2.36) the link
# fails with undefined `__isoc23_*` symbols, the same reason release builds run
# on Ubuntu 24.04.
#
# Single stage on purpose: the binary links the GTK/WebKit shared libraries the
# desktop crate depends on, so we run it from the build image rather than
# copying it into a slim base.
FROM rust:1-trixie

WORKDIR /app

# The MCP server shares the desktop crate, which links Tauri: its -sys crates
# find GTK/WebKit through pkg-config at build time even though the server never
# opens a window.
RUN apt-get update \
  && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev libwebkit2gtk-4.1-dev librsvg2-dev \
  && rm -rf /var/lib/apt/lists/*

# Only the Rust crate is needed (it is self-contained, with its own Cargo.lock).
COPY apps/desktop/src-tauri/ ./

# tauri::generate_context!() resolves frontendDist ("../dist") at compile time;
# the MCP server never serves it, so an empty placeholder is enough.
# --locked: build exactly the dependency versions in Cargo.lock.
RUN mkdir -p /dist \
  && cargo build --release --locked --bin callimachus-mcp

# The default index path is a per-user data dir; in a container point it at a
# writable dir with an existing parent. `db::open` creates the file on first run.
ENV CALLIMACHUS_DB=/data/index.db
RUN mkdir -p /data

ENTRYPOINT ["/app/target/release/callimachus-mcp"]
