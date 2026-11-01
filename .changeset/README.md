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

`"privatePackages": { "version": true, "tag": false }` in `config.json` is
load-bearing. Changesets v3 skips private packages by default, and the desktop
app and the extension are both private, so without it `changeset version` leaves
them at their old versions (a changeset that names only `callimachus` bumps
nothing at all). Tagging stays off because `scripts/release-tag.mjs` pushes the
single `v<version>` tag.

In RC mode (`pnpm rc:enter`), each `changeset version` moves the changesets it
used into `.changeset/pre/` and `pre.json` only records the mode and tag. Edit
or delete files in `.changeset/pre/` before `pnpm rc:exit` to shape the stable
changelog.
