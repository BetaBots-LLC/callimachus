# Contributing to Callimachus

Thanks for your interest. Issues, feature ideas, and PRs are all welcome.

## Prerequisites

- **macOS, Windows or Linux.** Everything builds and runs on all three; only the
  "Open in CLI" / "Resume" launchers are macOS-only today (they drive Terminal via
  AppleScript). On Linux, install the WebKitGTK build deps first
  ([Tauri's prerequisites](https://v2.tauri.app/start/prerequisites/#linux)), e.g.
  on Ubuntu 24.04+ / Debian 13+:
  ```bash
  sudo apt-get install -y libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev \
    build-essential pkg-config libssl-dev
  ```
  The ONNX Runtime prebuilt needs glibc 2.38 or newer, so older distros (Ubuntu
  22.04, Debian 12) can't build or run it.
- **Node 22+** and **pnpm** (`corepack enable` or `npm i -g pnpm`)
- **Rust** via [rustup](https://rustup.rs). CI pins a specific stable release
  (`RUST_TOOLCHAIN` in `.github/workflows/ci.yml`); use that or newer.

## Setup

```bash
git clone <repo-url> callimachus && cd callimachus
pnpm install
pnpm desktop:dev        # launches the desktop window (tauri dev)
```

First launch the index is empty — open **Settings → Reindex** to index your
sources, then **Build semantic index** for hybrid search.

## Repo layout

This is a [Turborepo](https://turborepo.com) + pnpm workspace.

```
apps/desktop/              Tauri 2 app + the `cal` CLI and `callimachus-mcp` server (src-tauri)
apps/vscode/               VS Code extension (shells out to `cal`)
apps/web/                  marketing + download site (TanStack Start)
packages/callimachus-mcp/  npm launcher for the MCP server (`npx -y callimachus-mcp`)
scripts/                   version-sync, release tagging
.changeset/                versioning + changelog
```

## Checks before a PR

```bash
pnpm check                             # Biome lint + format check
pnpm typecheck                         # all apps (tsc)
pnpm build                             # all apps
node --test packages/callimachus-mcp/test/*.test.mjs   # npm launcher tests
cd apps/desktop/src-tauri && cargo test   # Rust unit tests
```

CI runs the same checks, plus `cargo clippy -D warnings` on macOS and
`cargo check --all-targets` on Linux and Windows. Rust builds use `--locked`, so
commit `Cargo.lock` with any dependency change. The Biome step is advisory for
now (there are existing errors to clear), but please don't add new ones.

`cargo test -- --ignored` runs real-data smoke tests against your own history
(read-only) and downloads the embedding model on first run.

## Adding support for another agent

Each indexed source is a small, documented contract — usually one indexer module
+ a migration + a few wiring points (source seed, watcher, frontend label). Start
at [`apps/desktop/src-tauri/src/indexer/README.md`](apps/desktop/src-tauri/src/indexer/README.md).

## Conventions

- **Commits:** [Conventional Commits](https://www.conventionalcommits.org)
  (`feat:`, `fix:`, `chore:`, `docs:`, `refactor:`, `test:`).
- **Changesets:** if your change is user-facing, run `pnpm changeset` (patch by
  default) and commit the generated file so it lands in the changelog / version
  bump.
- **Privacy:** if a change adds network traffic or changes what analytics send,
  update the privacy policy (`apps/web/src/content/legal.ts`, bump
  `LEGAL_UPDATED`) and `SECURITY.md` in the same PR, and say so in the changeset.
- **Style:** functional and concise; validate at system boundaries; don't add
  comments or type annotations to unchanged code. Run `pnpm check` (Biome lint +
  format, or `pnpm check:fix` to apply) before opening a PR.
- **Releases:** maintainers cut releases via the flow in [RELEASING.md](RELEASING.md).

## License of contributions

Callimachus is **dual-licensed**: [AGPL-3.0-or-later](LICENSE) for everyone, plus
commercial licenses sold by the copyright holder (see [COMMERCIAL.md](COMMERCIAL.md)).
For that to work, the maintainer must be able to relicense the whole codebase.

**By submitting a contribution you agree that:**

1. Your contribution is licensed under **AGPL-3.0-or-later**, and
2. You grant **Ari Shaller** (the copyright holder) a perpetual, worldwide,
   non-exclusive, royalty-free right to **also license your contribution under other
   terms**, including the commercial license (so it can ship in commercial builds), and
3. You have the right to grant this (the work is yours or you're authorized).

If you can't agree to this, please open an issue instead of sending code.
