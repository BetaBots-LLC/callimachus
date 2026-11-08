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

The MCP server reads the index that the **Callimachus desktop app** or the `cal`
CLI builds; it does not index your agent history by itself. Install the app from
[Releases](https://github.com/BetaBots-LLC/callimachus/releases/latest), open it
once and let it index (**Index my history** on first launch), or, on a machine
without the app (a headless server, an SSH box, a Codespace), install `cal` with
this package and run `cal index` (see below). Until something has indexed, every
read tool answers with a short setup notice instead of results.

Keep this launcher and the app on the same release. Whichever opens the index
first after an update upgrades it, and an older build can't open an upgraded
index: the server exits with an error, and an older app asks you to update.
`npx -y callimachus-mcp` follows the newest release, so install the app's update
when it arrives, or pin `npx -y callimachus-mcp@<app version>`.

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
as Claude Code's MCP server, and **Settings → Other agents** does the same for
Codex, Cursor and Gemini CLI.

## The `cal` CLI without the app

On macOS and Linux, this package also installs the `cal` CLI from the same
release:

```bash
npx callimachus-mcp --install-cal
cal index      # build the index (once)
cal embed      # optional: the semantic index (downloads a 128 MB model once)
cal watch      # optional: keep it fresh as agents write (a terminal or a login item)
```

`--install-cal` downloads `cal-<triple>` into the same cache, checks it against
the same SHA-256 manifest, and links it at `~/.local/bin/cal`, where the Claude
Code plugin's hooks and the VS Code extension look for it. It never replaces a
`~/.local/bin/cal` that belongs to another program (a regular file, or a link to
something else) and leaves a link to a Callimachus build (the desktop app, or
one built from source) alone. In a shell where `~/.local/bin` comes first on
`PATH`, the link shadows the system calendar's `/usr/bin/cal`. Each time the MCP
server starts, a `cal` linked this way moves up to the server's release (never to
an older one), so the two agree on the index schema unless a newer launcher on
the same machine (an unpinned `npx -y callimachus-mcp` in another client) has
moved `cal` ahead of a pinned server. The version `cal` runs from is never pruned
from the cache. On Windows, install `cal` from the app
(**Settings > Agents**) or download `cal-x86_64-pc-windows-msvc.exe` from the
release.

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
2.38 too. On an older distro, run it in a container based on a newer one: the
repo's [Dockerfile](https://github.com/BetaBots-LLC/callimachus/blob/main/Dockerfile)
builds it on Debian 13 with `CALLIMACHUS_DB=/data/index.db`. Mount the folder
that holds your `index.db` at `/data`:

```bash
docker build -t callimachus-mcp .   # in a clone of the repo
docker run -i --rm -v ~/.local/share/dev.shaller.callimachus:/data callimachus-mcp
```

No extra flags are needed. The image's entrypoint starts the server as the owner
of the mounted folder, so SQLite can write the index and its `-wal` / `-shm`
files there, and those files stay yours. (`--user "$(id -u):$(id -g)"` works too.)
With nothing mounted, the image keeps an empty index of its own in `/data`.

On another platform that ONNX Runtime supports, you can build it yourself from
the tag that matches the launcher. You need Rust and, on Linux, the
`pkg-config`, OpenSSL and D-Bus development packages (on Debian/Ubuntu:
`pkg-config libssl-dev libdbus-1-dev`). As in the repo's Dockerfile, the server
builds from the Tauri-free `callimachus-standalone` crate, so neither GTK nor
WebKit nor the desktop frontend is needed (`--bin cal` builds the CLI):

```bash
git clone --depth 1 --branch v<version> https://github.com/BetaBots-LLC/callimachus
cd callimachus
cargo build --release --locked --manifest-path apps/desktop/src-tauri/Cargo.toml -p callimachus-standalone --bin callimachus-mcp
```

Then point the launcher at it with
`CALLIMACHUS_MCP_BIN=/path/to/callimachus/apps/desktop/src-tauri/target/release/callimachus-mcp`.

## Environment

- `CALLIMACHUS_MCP_BIN`: use this binary instead of downloading (for example a
  `cargo build` output, or the standalone `callimachus-mcp-<triple>` binary from
  the Releases page). It is not checksum-verified, since you chose it.
- `XDG_CACHE_HOME`: override the cache location (defaults to `~/.cache`).
- `CALLIMACHUS_DB`: the `index.db` to serve, when it isn't in the desktop app's
  default location (for example a mounted index in a container, or a second
  profile). The server creates an empty index there if the file doesn't exist
  yet.

## License

AGPL-3.0-or-later
