# Contributing to Callimachus

Thanks for your interest. Issues, feature ideas, and PRs are all welcome.

## Prerequisites

- **macOS** (the app is macOS-first today — keychain + "Open in CLI" are macOS-specific)
- **Node 20+** and **pnpm** (`corepack enable` or `npm i -g pnpm`)
- **Rust** (stable, via [rustup](https://rustup.rs))

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
apps/desktop/    Tauri 2 app + the `cal` CLI and `callimachus-mcp` server (src-tauri)
apps/vscode/     VS Code extension (shells out to `cal`)
apps/web/        marketing + download site (reserved)
scripts/         version-sync, release tagging
.changeset/      versioning + changelog
```

## Checks before a PR

```bash
pnpm typecheck                         # all apps (tsc)
pnpm build                             # all apps
cd apps/desktop/src-tauri && cargo test   # Rust unit tests
```

`cargo test -- --ignored` runs real-data smoke tests against your own history
(read-only) and downloads the embedding model on first run.

## Adding support for another agent

Each indexed source is a small, documented contract — usually one indexer module
+ a migration + a few wiring points (source seed, watcher, frontend label). Start
at [`apps/desktop/src-tauri/src/indexer/README.md`](apps/desktop/src-tauri/src/indexer/README.md).

## Conventions

- **Commits:** [Conventional Commits](https://www.conventionalcommits.org)
  (`feat:`, `fix:`, `chore:`, `docs:`, `refactor:`, `test:`).
- **Changesets:** if your change is user-facing, run `pnpm changeset` and commit
  the generated file so it lands in the changelog / version bump.
- **Style:** functional and concise; validate at system boundaries; don't add
  comments or type annotations to unchanged code.
- **Releases:** maintainers cut releases via the flow in [RELEASING.md](RELEASING.md).

## License of contributions

By contributing, you agree your contributions are licensed under the project's
[Apache-2.0](LICENSE) license.
