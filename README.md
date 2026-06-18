<p align="center">
  <img src="assets/hero.png" alt="Callimachus — the catalogue for your AI coding history" width="100%">
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-2C436C.svg" alt="License: Apache-2.0"></a>
  <img src="https://img.shields.io/badge/platform-macOS-1B3252.svg" alt="Platform: macOS">
  <img src="https://img.shields.io/badge/Tauri-2-24C8DB?logo=tauri&logoColor=white" alt="Tauri 2">
  <img src="https://img.shields.io/badge/Rust-stable-E0A93C?logo=rust&logoColor=white" alt="Rust">
  <img src="https://img.shields.io/badge/React-19-61DAFB?logo=react&logoColor=white" alt="React 19">
  <img src="https://img.shields.io/badge/data-100%25%20local-3FA34D.svg" alt="100% local">
</p>

> **Local index & search for your AI coding-agent threads** — across **11 tools** (Claude Code, Codex, Cursor, Gemini CLI, Qwen Code, Goose, OpenCode, Continue, Cline, Roo Code, Kilo Code) — plus a provider-agnostic chat, an MCP server, a CLI, and a VS Code / Cursor extension. Everything stays on your machine.

Named for [Callimachus](https://en.wikipedia.org/wiki/Callimachus), who built the first catalogue of the Library of Alexandria.

## Download

Grab the latest signed macOS build (`.dmg`) from **[Releases](../../releases/latest)** — the app auto-updates from there on. Prefer to build it yourself? See [Develop](#develop).

## What it does

- **Indexes** every conversation from 11 coding agents into one local SQLite store — Claude Code, Codex, Cursor, Gemini CLI, Qwen Code, Goose, OpenCode, Continue, Cline, Roo Code, and Kilo Code. Adding another source is a [small, documented contract](apps/desktop/src-tauri/src/indexer/README.md).
- **Searches** them with hybrid ranking: keyword (SQLite FTS5 / BM25) fused with on-device semantic similarity (sqlite-vec KNN, no cloud) via Reciprocal Rank Fusion. Filter by source, project, and subagents.
- **Chats** with an in-app agent (Anthropic / OpenAI / Gemini / OpenRouter / Ollama — your key, your choice) that can **search your own history** and **run shell commands with your approval**; streaming, cancellable, with live model lists. Chats are saved and become searchable too.
- **Carries context across tools** — open any thread in any agent CLI ("Open in Claude / Codex / Gemini …", seeded with the packed transcript), resume a Claude Code / Codex thread in its native CLI, copy context, or export a thread to Obsidian (optionally AI-summarized with decisions / gotchas / TODOs).
- **Surfaces to your agents** — a bundled MCP server (`callimachus-mcp`) exposes the index as tools any agent can call mid-session; the `/recall` skill teaches them when to use it.
- **Stays current** via a background file watcher; **stays private** — API keys live in the OS keychain, nothing is sent anywhere except the LLM provider you pick.

## Stack

- **Shell:** Tauri 2 (Rust) + React 19 + TypeScript + **Vite 8**
- **Store/search:** bundled SQLite + FTS5 (`rusqlite`); on-device embeddings via `fastembed` (bge-small-en-v1.5, 384-dim); KNN in SQL via `sqlite-vec` (vec0)
- **Watcher:** `notify` + debouncer
- **Chat:** multi-provider via the `genai` crate (Anthropic / OpenAI / Gemini / OpenRouter / Ollama), streaming tokens over a Tauri Channel, cancellable, with agent tool-calls (history search + approved shell)
- **Secrets:** macOS Keychain (`keyring-core` + `apple-native-keyring-store`)
- **Sidecars:** `callimachus-mcp` (MCP server) and `cal` (CLI) — both reuse the desktop core lib against the same `index.db`
- **Editor:** a VS Code / Cursor extension (`apps/vscode`, published to the Marketplace + Open VSX) that shells out to `cal`

## Monorepo

This is a [Turborepo](https://turborepo.com) + pnpm workspace.

```
apps/
  desktop/        # the Tauri 2 desktop app + the cal CLI and MCP server (src-tauri)
  vscode/         # VS Code extension (search history from the editor)
  web/            # marketing + download site (reserved, not built yet)
packages/         # shared code, when it appears
.changeset/       # version + changelog management
scripts/          # version-sync, release tagging
```

Releases, versioning, and the auto-updater are documented in [RELEASING.md](RELEASING.md).

## Develop

```bash
pnpm install
pnpm desktop:dev      # launches the desktop window (tauri dev)

# from the repo root, across all apps:
pnpm build            # turbo: build every app's frontend
pnpm typecheck        # turbo: typecheck every app
```

First launch: the index is empty — open **Settings** (or hit **Reindex**) to index your sources, then **Build semantic index** to enable semantic search.

### Tests

```bash
cd apps/desktop/src-tauri
cargo test                                   # fast unit tests
cargo test -- --ignored --nocapture          # real-data + model + keychain smoke tests
```

The `--ignored` tests touch live data on this machine: each source has a `real_<source>_index` smoke test that indexes your real history read-only (`~/.claude`, `~/.codex`, Cursor, `~/.gemini`, `~/.qwen`, Goose, OpenCode, Continue, Cline/Roo/Kilo), plus the embedding-model download (first run, needs network) and a Keychain round-trip.

## Use your history anywhere

Beyond the desktop window, the same local index is reachable from your agents, terminal, and editor — all reading one `index.db`.

**MCP server** — let any agent search its own past work mid-session:

```bash
cargo install --path apps/desktop/src-tauri --bin callimachus-mcp
claude mcp add callimachus -- callimachus-mcp        # or any MCP client
```

Tools: `search_threads`, `search_current_project` (auto-scoped to the repo it runs in), `recent_threads`, `get_thread`. The bundled `/recall` skill ([.claude/skills/recall](.claude/skills/recall/SKILL.md)) tells agents when to reach for them.

**CLI** — `cal`, pipe-friendly:

```bash
cargo install --path apps/desktop/src-tauri --bin cal
cal search "vector index migration" -y    # -y = hybrid (semantic + keyword)
cal recent -n 10
cal cat 42 | pbcopy                        # packed transcript → clipboard
cal stats                                  # index totals + per-source breakdown
cal export 42 --vault ~/Obsidian          # write a thread as an Obsidian note
```

**VS Code / Cursor** — the extension adds a "Callimachus History" sidebar, a status-bar search button, and commands to search / insert / copy threads (it shells out to `cal`). Install from the **[VS Code Marketplace](https://marketplace.visualstudio.com/)** or **[Open VSX](https://open-vsx.org/)** (the registry **Cursor** and VSCodium use), or grab the `.vsix` from [Releases](../../releases). See [apps/vscode/README.md](apps/vscode/README.md).

## Notes / limitations

- macOS-first. The "Open in CLI" / "Resume" launchers and the keychain backend are macOS-specific today.
- Cline / Roo Code / Kilo Code are editor extensions with no CLI, so they are index-only (searchable, but not relaunchable via "Resume").
- Cursor doesn't store a per-thread workspace, so Cursor threads currently have no project path.
- Claude Code subagent transcripts are indexed but hidden behind a "subagents" toggle by default.
- Large first index is a one-time cost (the Claude corpus here was ~90k messages in ~25s); subsequent passes skip unchanged files.
- More sources (Charm Crush, Factory Droid, Copilot CLI) are scoped but not yet integrated — see [the indexer guide](apps/desktop/src-tauri/src/indexer/README.md).

## Contributing

Issues and PRs welcome. [CONTRIBUTING.md](CONTRIBUTING.md) covers local setup, conventions, and the release flow. Adding support for another agent is a [small, documented contract](apps/desktop/src-tauri/src/indexer/README.md) — usually one indexer module + a migration + a few wiring points.

## Security & privacy

Callimachus is local-first by design: your conversation index never leaves your machine, API keys live in the OS keychain (never on disk), and the only outbound traffic is to the LLM provider you explicitly choose. To report a vulnerability, see [SECURITY.md](SECURITY.md).

## License

[Apache-2.0](LICENSE) © Ari Shaller. See [NOTICE](NOTICE) for attributions.

## Acknowledgements

Built on [Tauri](https://tauri.app), [fastembed-rs](https://github.com/Anush008/fastembed-rs), [sqlite-vec](https://github.com/asg017/sqlite-vec), and [genai](https://github.com/jeremychone/rust-genai). Named for [Callimachus of Cyrene](https://en.wikipedia.org/wiki/Callimachus), who catalogued the Library of Alexandria.

<p align="center"><sub>Social preview: <a href="assets/og.png"><code>assets/og.png</code></a> · brand sources in <a href="assets/brand"><code>assets/brand/</code></a></sub></p>
