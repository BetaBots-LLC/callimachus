# Builds and runs the standalone `callimachus-mcp` server over stdio. Used by MCP
# registries (e.g. Glama) to verify the server starts and answers introspection
# (tools/list). The index is created empty on first open, so no real data is
# needed to pass verification.
#
# Build from the repo root:
#   docker build -t callimachus-mcp .
#
# Single stage on purpose: `ort` (fastembed's ONNX backend) links a runtime lib
# that must be present at run time, so we run the binary from the build image
# rather than copying it into a slim base.
FROM rust:1-bookworm

WORKDIR /app

RUN apt-get update \
  && apt-get install -y --no-install-recommends pkg-config libssl-dev \
  && rm -rf /var/lib/apt/lists/*

# Only the Rust crate is needed (it is self-contained, with its own Cargo.lock).
COPY apps/desktop/src-tauri/ ./

RUN cargo build --release --bin callimachus-mcp

# The default index path is a macOS-style location; on Linux point it at a
# writable dir with an existing parent. `db::open` creates the file on first run.
ENV CALLIMACHUS_DB=/data/index.db
RUN mkdir -p /data

ENTRYPOINT ["/app/target/release/callimachus-mcp"]
