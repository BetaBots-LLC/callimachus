# Changesets

This folder is managed by [Changesets](https://github.com/changesets/changesets).
It drives version bumps and the changelog for everything in the monorepo.

## TL;DR

- **Made a user-facing change?** Run `pnpm changeset`, pick the bump (patch by
  default), write a one-line summary. Commit the generated file with your PR.
- **Releasing?** See [`RELEASING.md`](../RELEASING.md) at the repo root.

`callimachus` (desktop), `callimachus-vscode` (extension) and `callimachus-mcp`
(npm wrapper) are one `fixed` group: a changeset for any of them bumps all three
to the same version. `callimachus-web` (the website) is ignored, so describe
user-visible website changes, such as privacy-policy updates, in a `callimachus`
changeset.

The version you bump here is the single source of truth. `scripts/sync-versions.mjs`
propagates it into `tauri.conf.json`, `Cargo.toml`, `Cargo.lock` and `server.json`
so the desktop installers, the updater manifest, the MCP registry entry and the
git tag all agree.
