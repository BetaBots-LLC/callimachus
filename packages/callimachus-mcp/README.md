# callimachus-mcp

The [Callimachus](https://github.com/BetaBots-LLC/callimachus) MCP server, packaged for `npx`.

Callimachus keeps a **local** index of your AI coding-agent threads (Claude Code,
Codex, Cursor, Gemini CLI, Qwen Code, Goose, OpenCode, Continue, Cline, Roo Code,
Kilo Code) and exposes search/recall over them as MCP tools. Everything stays on
your machine.

This npm package is a thin launcher: on first run it downloads the prebuilt
`callimachus-mcp` binary for your platform from the matching GitHub Release,
caches it under `~/.cache/callimachus-mcp/<version>/`, and execs it over stdio.

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

## Supported platforms

Prebuilt binaries are released for:

- macOS (Apple Silicon) — `aarch64-apple-darwin`
- Linux (x64) — `x86_64-unknown-linux-gnu`
- Windows (x64) — `x86_64-pc-windows-msvc`

On any other platform, build from source
(`cargo install --git https://github.com/BetaBots-LLC/callimachus --bin callimachus-mcp`)
and point the launcher at it with `CALLIMACHUS_MCP_BIN=/path/to/callimachus-mcp`.

## Environment

- `CALLIMACHUS_MCP_BIN` — use this binary instead of downloading (e.g. the one the
  desktop app installs on your PATH).
- `XDG_CACHE_HOME` — override the cache location (defaults to `~/.cache`).

## License

AGPL-3.0-or-later
