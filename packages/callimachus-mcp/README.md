# callimachus-mcp

The [Callimachus](https://github.com/BetaBots-LLC/callimachus) MCP server, packaged for `npx`.

Callimachus keeps a **local** index of your AI coding-agent threads (Claude Code,
Codex, Cursor, Gemini CLI, Qwen Code, Goose, OpenCode, Continue, Cline, Roo Code,
Kilo Code, and Copilot Chat in VS Code-family editors) and exposes search and
recall over them as MCP tools. Everything stays on your machine, and the MCP
server never sends analytics.

This npm package is a thin launcher: on first run it downloads the prebuilt
`callimachus-mcp` binary for your platform from the matching GitHub Release,
checks its SHA-256 against the checksum manifest shipped inside this package,
caches it under `~/.cache/callimachus-mcp/<version>/`, and execs it over stdio.
A cached version older than the one being installed is removed once nothing has
used it for 30 days, so MCP clients pinned to different versions don't keep
deleting each other's binary.

## Build the index first

The MCP server reads the index that the **Callimachus desktop app** builds; it
does not index your agent history by itself. Install the app from
[Releases](https://github.com/BetaBots-LLC/callimachus/releases/latest), open it
once and let it index (or hit **Reindex**). Until it has indexed, the tools have
nothing to return. On a machine without the desktop app (a headless server, an
SSH box, a Codespace) there is no index to search yet.

## Use

Register it with any MCP client:

```bash
claude mcp add callimachus -- npx -y callimachus-mcp
```

Or add it to a client config:

```json
{
  "mcpServers": {
    "callimachus": {
      "command": "npx",
      "args": ["-y", "callimachus-mcp"]
    }
  }
}
```

Already running the desktop app? You don't need this package: **Settings → Claude
Code → Enable for Claude Code** registers the app itself (launched with `--mcp`)
as the MCP server.

## Supported platforms

Prebuilt binaries are released for:

- macOS (Apple Silicon): `aarch64-apple-darwin`
- Linux (x64): `x86_64-unknown-linux-gnu`, which needs **glibc 2.38 or newer**
  (Ubuntu 24.04+, Debian 13+, Fedora 39+). Older distros such as Ubuntu 22.04,
  Debian 12 and RHEL 9, and musl-based distros such as Alpine, are not supported
  by the prebuilt binary; the launcher stops with a clear error instead of
  downloading it.
- Windows (x64): `x86_64-pc-windows-msvc`

Building from source doesn't help on an Intel Mac or below glibc 2.38: the ONNX
Runtime the server links has no Intel macOS build, and on Linux it needs glibc
2.38 too. On an older distro, run it in a container based on a
newer one (the repo's [Dockerfile](https://github.com/BetaBots-LLC/callimachus/blob/main/Dockerfile)
builds it on Debian 13) with `CALLIMACHUS_DB` pointing at your mounted `index.db`.

On another platform that ONNX Runtime supports, you can build it yourself from
the tag that matches the launcher. You need Rust and, on Linux, the
`pkg-config`, OpenSSL, `webkit2gtk-4.1` and `librsvg2` development packages
(the server lives in the desktop app's Tauri crate). As in the repo's
Dockerfile, an empty `dist` directory stands in for the desktop frontend, which
the MCP server never uses:

```bash
git clone --depth 1 --branch v<version> https://github.com/BetaBots-LLC/callimachus
cd callimachus/apps/desktop
mkdir dist
cargo build --release --locked --manifest-path src-tauri/Cargo.toml --bin callimachus-mcp
```

Then point the launcher at it with
`CALLIMACHUS_MCP_BIN=/path/to/callimachus/apps/desktop/src-tauri/target/release/callimachus-mcp`.

## Environment

- `CALLIMACHUS_MCP_BIN`: use this binary instead of downloading (for example a
  `cargo build` output, or the standalone `callimachus-mcp-<triple>` binary from
  the Releases page). It is not checksum-verified, since you chose it.
- `XDG_CACHE_HOME`: override the cache location (defaults to `~/.cache`).

## License

AGPL-3.0-or-later
