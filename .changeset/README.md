# Changesets

This folder is managed by [Changesets](https://github.com/changesets/changesets).
It drives version bumps and the changelog for everything in the monorepo.

## TL;DR

- **Made a user-facing change?** Run `pnpm changeset`, pick the bump (patch/minor/major),
  write a one-line summary. Commit the generated file with your PR.
- **Releasing?** See [`RELEASING.md`](../RELEASING.md) at the repo root.

The version you bump here is the single source of truth — `scripts/sync-versions.mjs`
propagates it into `tauri.conf.json` and `Cargo.toml` so the desktop installers,
the updater manifest, and the git tag all agree.
