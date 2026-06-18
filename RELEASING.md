# Releasing

How versions, changelogs, and release candidates work for both shippable
artifacts in this monorepo — the **desktop app** (macOS / Windows / Linux
installers + auto-updater) and the **VS Code / Cursor extension** (`.vsix` on the
marketplaces).

## How it fits together

```
PR with a changeset ─┐
                     ▼
   push to main → version.yml ──(changesets)──► opens "Version Packages" PR
                                                         │ merge
                                                         ▼
                       changeset version  → bumps each changed package
                       sync-versions.mjs  → tauri.conf.json + Cargo.toml (desktop)
                       release-tag.mjs    → one tag per bumped artifact:
                                              v<ver>         (desktop)
                                              vscode-v<ver>  (extension)
              ┌───────────────────────────────┴───────────────────────────────┐
              ▼ v<ver>                                                          ▼ vscode-v<ver>
      build.yml → tauri-action                                  publish-extension.yml
        → signed mac/win/linux installers                        → VS Code Marketplace (vsce)
        → GitHub Release                                          → Open VSX  (Cursor / VSCodium)
              │                                                   → GitHub Release (.vsix)
              ▼
      installed apps auto-update
```

**Each artifact has its own version** (independent). A release that only touches
the extension pushes only `vscode-v<ver>`; one that only touches the desktop
pushes only `v<ver>`. `scripts/sync-versions.mjs` keeps the desktop version in
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
| `RELEASE_TOKEN` | A PAT with `contents: write` + `workflows`. Lets the auto-pushed tags trigger `build.yml` / `publish-extension.yml` (the default `GITHUB_TOKEN` cannot trigger another workflow). |
| `VSCE_PAT` | *(optional)* Azure DevOps PAT for the VS Code Marketplace. Absent → that publish step is skipped. |
| `OVSX_PAT` | *(optional)* [Open VSX](https://open-vsx.org) access token — this is the registry **Cursor** & VSCodium install from. Absent → that publish step is skipped. |

> The extension always lands as a downloadable `.vsix` on the GitHub Release even
> with no marketplace tokens set.

## Normal release (stable)

1. With each PR that changes the app, add a changeset:
   ```bash
   pnpm changeset          # pick patch/minor/major, write a one-liner
   ```
   Commit the generated `.changeset/*.md` file.
2. Merge PRs to `main`. `version.yml` opens (and keeps updating) a
   **"Version Packages"** PR that accumulates the pending changesets.
3. **Merge the Version Packages PR.** That bumps the version, writes
   `CHANGELOG.md`, and pushes `vX.Y.Z`.
4. The pushed tags fan out:
   - `v<ver>` → `build.yml` builds signed installers for **macOS (universal),
     Windows (x64) and Linux (x64)**, publishes one GitHub Release, uploads
     `latest.json`. Installed apps auto-update.
   - `vscode-v<ver>` → `publish-extension.yml` packages the extension and pushes it
     to the VS Code Marketplace + Open VSX, and attaches the `.vsix` to a Release.

## Extensions (VS Code & Cursor)

There is one extension (`apps/vscode`) published to two registries — **Cursor and
VSCodium install from Open VSX**, official VS Code from the Marketplace.

- It versions independently of the desktop app: add a changeset that bumps
  `callimachus-vscode`, and the Version Packages merge pushes only `vscode-v<ver>`.
- `publish-extension.yml` runs `vsce` (Marketplace) and `ovsx` (Open VSX); each is
  skipped if its token secret is missing, and the `.vsix` is always attached to the
  GitHub Release for manual install.
- One-time, before the first publish: create the `betabots` publisher on the
  [VS Code Marketplace](https://marketplace.visualstudio.com/manage) and the
  matching namespace on [Open VSX](https://open-vsx.org), then add `VSCE_PAT` /
  `OVSX_PAT` (the publish workflow auto-creates the Open VSX namespace).

## Release candidates

RC versioning rides on Changesets' prerelease mode.

```bash
# Enter RC mode (commit the generated .changeset/pre.json to main)
pnpm rc:enter            # = changeset pre enter rc

# From here, every Version Packages merge produces vX.Y.Z-rc.0, -rc.1, …
# build.yml marks these as GitHub *prereleases* automatically.

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

Tag exists but the run failed or you skipped automation:

- Desktop — Actions → **Build & Release** → Run workflow → enter the tag (e.g. `v0.2.0-rc.1`).
- Extension — Actions → **Publish Extension** → Run workflow → enter the tag (e.g. `vscode-v0.1.1`).

Or cut tags by hand:

```bash
git tag v0.2.0        && git push origin v0.2.0           # → build.yml (desktop)
git tag vscode-v0.1.1 && git push origin vscode-v0.1.1    # → publish-extension.yml
```

## Adding an RC auto-update channel (later)

When you want RC testers to auto-update from a beta channel:

1. Maintain a moving `rc` GitHub release whose `latest.json` always reflects the
   newest RC.
2. Ship a separate RC build whose `plugins.updater.endpoints` points at that
   `rc` manifest (e.g. via a `--config` override in `build.yml` for `-rc` tags).
3. Stable builds keep pointing at `releases/latest`.

Until then, RCs are opt-in downloads — simpler and zero extra hosting.
