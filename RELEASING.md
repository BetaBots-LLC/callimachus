# Releasing

How versions, changelogs, and release candidates work for both shippable
artifacts in this monorepo — the **desktop app** (macOS / Windows / Linux
installers + standalone `cal` / `callimachus-mcp` binaries + auto-updater) and the
**VS Code / Cursor extension** (`.vsix` on the marketplaces). Both ship together
on **one shared version**. Off the same desktop tag, the **`callimachus-mcp` npm
wrapper** is also published to npm and the
[official MCP registry](https://registry.modelcontextprotocol.io)
(`io.github.betabots-llc/callimachus`); it ships on the shared version too, so it
downloads the matching `v<ver>` release binaries at runtime.

## How it fits together

```
PR with a changeset ─┐
                     ▼
   push to main → version.yml ──(changesets)──► opens "Version Packages" PR
                                                         │ merge
                                                         ▼
                       changeset version  → bumps the version (desktop + extension
                                            together, via the Changesets fixed group)
                       sync-versions.mjs  → tauri.conf.json + Cargo.toml (desktop)
                       release-tag.mjs    → one tag: v<ver>
                                                       │
              ┌────────────────────────────────────── ┴ v<ver> ──────────────────────────────┐
              ▼                                                                                ▼
      build.yml → tauri-action                                              publish-extension.yml
        → signed mac/win/linux installers                                     → VS Code Marketplace (vsce)
        → standalone cal-<triple> / callimachus-mcp-<triple> binaries         → Open VSX  (Cursor / VSCodium)
        → GitHub Release                                                      → same GitHub Release (.vsix)
              │
              ▼
      installed apps auto-update
```

**The desktop app and the extension share one version** and ride a single `v<ver>`
tag. They are kept in lockstep by the Changesets **fixed** group
(`["callimachus", "callimachus-vscode"]`): any changeset that bumps one bumps both,
so every release pushes exactly one `v<ver>` tag that fans out to **both**
`build.yml` (desktop) and `publish-extension.yml` (extension).
`scripts/sync-versions.mjs` then keeps the desktop version in
`apps/desktop/package.json` in lockstep with `tauri.conf.json` + `Cargo.toml` so
the installer, the updater manifest, and the git tag never drift.

## One-time setup

Do these once before the first release.

### 1. Generate the updater signing keypair

```bash
pnpm --filter callimachus tauri signer generate -w ~/.tauri/callimachus.key
```

- Copy the **public key** it prints into `apps/desktop/src-tauri/tauri.conf.json`
  → `plugins.updater.pubkey` (replace the `REPLACE_ME…` placeholder). This is the
  literal key content, not a path.
- Keep the **private key** secret. Never commit it.

### 2. Updater endpoint

`tauri.conf.json` → `plugins.updater.endpoints` already points at this repo:

```
https://github.com/BetaBots-LLC/callimachus/releases/latest/download/latest.json
```

`releases/latest` always resolves to the newest **non-prerelease**, so the stable
updater never picks up a release candidate.

### 3. Add the GitHub repo secrets

Settings → Secrets and variables → Actions:

| Secret | What |
| --- | --- |
| `TAURI_SIGNING_PRIVATE_KEY` | Contents of `~/.tauri/callimachus.key` |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | The password you set (empty string if none) |
| `RELEASE_TOKEN` | A PAT with `contents: write` + `workflows`. Lets the auto-pushed `v<ver>` tag trigger `build.yml` + `publish-extension.yml` (the default `GITHUB_TOKEN` cannot trigger another workflow). |
| `VSCE_PAT` | *(optional)* Azure DevOps PAT for the VS Code Marketplace. Absent → that publish step is skipped. |
| `OVSX_PAT` | *(optional)* [Open VSX](https://open-vsx.org) access token — this is the registry **Cursor** & VSCodium install from. Absent → that publish step is skipped. |
| `NPM_TOKEN` | *(optional)* npm automation token with publish rights to `callimachus-mcp`. Absent → the npm + MCP-registry publish job is skipped. The MCP registry itself uses GitHub OIDC, no secret. |

> The extension always lands as a downloadable `.vsix` on the GitHub Release even
> with no marketplace tokens set.
>
> The first MCP-registry publish should be done by hand (see "MCP registry"
> below) so you can claim the `io.github.betabots-llc/*` namespace and confirm the
> npm package validates; after that, every release refreshes it automatically.

## Normal release (stable)

1. With each PR that changes the app, add a changeset:
   ```bash
   pnpm changeset          # pick patch/minor/major, write a one-liner
   ```
   Commit the generated `.changeset/*.md` file.
2. Merge PRs to `main`. `version.yml` opens (and keeps updating) a
   **"Version Packages"** PR that accumulates the pending changesets.
3. **Merge the Version Packages PR.** That bumps the (shared) version, writes
   `CHANGELOG.md`, and pushes a single `vX.Y.Z` tag.
4. That one `v<ver>` tag fans out to **both** workflows:
   - `build.yml` builds signed installers for **macOS (Apple Silicon),
     Windows (x64) and Linux (x64)**, also builds the standalone
     `cal-<triple>` / `callimachus-mcp-<triple>` binaries (for CLI/MCP-only users),
     publishes one GitHub Release, and uploads `latest.json`. Installed apps
     auto-update. Then its `publish-mcp` job publishes the `callimachus-mcp` npm
     wrapper and refreshes the MCP registry entry (only when `NPM_TOKEN` is set;
     skipped on prereleases).
   - `publish-extension.yml` packages the extension and pushes it to the VS Code
     Marketplace + Open VSX, then attaches the `.vsix` to **that same Release**.

## Extensions (VS Code & Cursor)

There is one extension (`apps/vscode`) published to two registries — **Cursor and
VSCodium install from Open VSX**, official VS Code from the Marketplace.

- It shares the desktop app's version (the Changesets **fixed** group bumps both
  together), so it ships on the same `v<ver>` tag — there is no separate extension
  tag. `publish-extension.yml` triggers on `v[0-9]*` alongside `build.yml`.
- `publish-extension.yml` runs `vsce` (Marketplace) and `ovsx` (Open VSX); each is
  skipped if its token secret is missing, and the `.vsix` is always attached to the
  same GitHub Release for manual install.
- The extension publish is skipped on `-rc` / `-beta` tags (the Marketplace doesn't
  accept prerelease versions), so RCs ship the desktop installers only.
- One-time, before the first publish: create the `BetaBotsLLC` publisher on the
  [VS Code Marketplace](https://marketplace.visualstudio.com/manage) and the
  matching namespace on [Open VSX](https://open-vsx.org), then add `VSCE_PAT` /
  `OVSX_PAT` (the publish workflow auto-creates the Open VSX namespace).

## MCP registry

The bundled MCP server is published to the
[official MCP registry](https://registry.modelcontextprotocol.io) as
`io.github.betabots-llc/callimachus`, distributed through the **`callimachus-mcp`
npm package** (`packages/callimachus-mcp`). That package carries no binary: its
`bin` downloads the prebuilt `callimachus-mcp-<triple>` asset from the matching
`v<ver>` GitHub Release on first run and caches it. Because it's in the Changesets
**fixed** group, its version always equals the release version, so the download
URL always resolves. `scripts/sync-versions.mjs` keeps `server.json` (the registry
manifest) on the same version.

- Per release, `build.yml`'s `publish-mcp` job (after the binaries are attached)
  runs `npm publish` then `mcp-publisher publish`, authenticating to the registry
  with GitHub OIDC (`id-token: write`, no secret). It needs the `NPM_TOKEN` secret
  and is skipped on `-rc` / `-beta` tags.
- **One-time, before the first publish**, claim the namespace and verify the npm
  package by hand from a clean checkout of the tagged commit:

  ```bash
  # 1. publish the npm wrapper (its package.json carries the matching mcpName)
  cd packages/callimachus-mcp && npm publish --access public && cd -

  # 2. install the registry CLI
  brew install mcp-publisher        # or grab the release binary

  # 3. authenticate (device flow) and publish server.json from the repo root
  mcp-publisher login github        # opens a GitHub device-code prompt
  mcp-publisher publish             # reads ./server.json

  # 4. verify
  curl "https://registry.modelcontextprotocol.io/v0.1/servers?search=io.github.betabots-llc/callimachus"
  ```

  `mcp-publisher login github` must authenticate as a member/owner of the
  **BetaBots-LLC** org, since the `io.github.betabots-llc/*` namespace maps to that
  GitHub org. After this first run, the CI job keeps the entry current.

## Release candidates

RC versioning rides on Changesets' prerelease mode.

```bash
# Enter RC mode (commit the generated .changeset/pre.json to main)
pnpm rc:enter            # = changeset pre enter rc

# From here, every Version Packages merge produces vX.Y.Z-rc.0, -rc.1, …
# (e.g. v0.6.0-rc.1). build.yml marks these as GitHub *prereleases* automatically,
# and publish-extension.yml skips them (no extension prerelease channel).

# When the RC is good, leave prerelease mode:
pnpm rc:exit             # = changeset pre exit
# The next Version Packages merge produces the stable vX.Y.Z.
```

- RC builds are published as **prereleases**, so the stable auto-updater
  (`releases/latest`) ignores them. Testers install RCs by downloading them from
  the GitHub Releases page.
- To give RC testers **auto-updates too** (a real "beta channel"), build the RC
  with an endpoint that points at a moving `rc` manifest and have testers run a
  build configured against it. Not wired yet — see "Adding an RC auto-update
  channel" below. The prerelease model above needs no extra infra.

## Manual / re-run build

Tag exists but the run failed or you skipped automation. Both workflows take the
same `v<ver>` tag:

- Desktop — Actions → **Build & Release** → Run workflow → enter the tag (e.g. `v0.5.0`).
- Extension — Actions → **Publish Extension** → Run workflow → enter the same tag (e.g. `v0.5.0`).

Or cut the tag by hand — one tag triggers **both** `build.yml` and
`publish-extension.yml`:

```bash
git tag v0.5.0 && git push origin v0.5.0    # → build.yml + publish-extension.yml
```

## Adding an RC auto-update channel (later)

When you want RC testers to auto-update from a beta channel:

1. Maintain a moving `rc` GitHub release whose `latest.json` always reflects the
   newest RC.
2. Ship a separate RC build whose `plugins.updater.endpoints` points at that
   `rc` manifest (e.g. via a `--config` override in `build.yml` for `-rc` tags).
3. Stable builds keep pointing at `releases/latest`.

Until then, RCs are opt-in downloads — simpler and zero extra hosting.
