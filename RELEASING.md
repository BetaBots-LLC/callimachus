# Releasing

How versions, changelogs, and release candidates work for every shippable
artifact in this monorepo: the **desktop app** (macOS / Windows / Linux
installers + standalone `cal` / `callimachus-mcp` binaries + auto-updater), the
**VS Code / Cursor extension** (`.vsix` on the marketplaces), and the
**`callimachus-mcp` npm wrapper**, which is also listed on the
[official MCP registry](https://registry.modelcontextprotocol.io) as
`io.github.BetaBots-LLC/callimachus`. All three ship together on **one shared
version**, so the npm wrapper always downloads the matching `v<ver>` release
binaries at runtime.

## How it fits together

```
PR with a changeset ─┐
                     ▼
   push to main → version.yml ──(changesets)──► opens "Version Packages" PR
                                                         │ merge
                                                         ▼
                       changeset version  → bumps the shared version of callimachus,
                                            callimachus-vscode and callimachus-mcp
                                            (one Changesets fixed group)
                       sync-versions.mjs  → tauri.conf.json, Cargo.toml, Cargo.lock,
                                            server.json, the Claude Code plugin.json
                       release-tag.mjs    → one tag: v<ver>
                                                       │
              ┌────────────────────────────────────── ┴ v<ver> ─────────────────────────┐
              ▼                                                                           ▼
      build.yml                                                         publish-extension.yml
        create-release → DRAFT GitHub Release (notes from CHANGELOG)      → VS Code Marketplace (vsce)
        release (mac/win/linux) → signed installers + latest.json,        → Open VSX (Cursor / VSCodium)
            standalone cal-<triple> / callimachus-mcp-<triple>            → waits for the draft, then
        finalize → checks every asset + latest.json, adds SHA256SUMS,        attaches the .vsix
            then PUBLISHES the release (marks it Latest)
        publish-mcp → npm (trusted publishing) + MCP registry
              │
              ▼
      installed apps auto-update
```

**One version, one tag.** The Changesets **fixed** group
(`["callimachus", "callimachus-vscode", "callimachus-mcp"]`) means any changeset
that bumps one package bumps all three, so every release pushes exactly one
`v<ver>` tag that fans out to **both** `build.yml` and `publish-extension.yml`.
`scripts/sync-versions.mjs` then copies the version from `apps/desktop/package.json`
into `tauri.conf.json`, `Cargo.toml`, the root `callimachus` entry in `Cargo.lock`,
`server.json` (the MCP registry manifest) and `plugins/claude-code/.claude-plugin/plugin.json`
(Claude Code keeps plugin users on a plugin's `version` until it changes), so the installer,
the updater manifest, the registry entry, the plugin and the git tag never drift. It also
rewrites the plugin's pinned `npx -y callimachus-mcp@<ver>` in `plugins/claude-code/.mcp.json`
and the `"ref": "v<ver>"` of the plugin's `git-subdir` source in `.claude-plugin/marketplace.json`.

**The plugin follows the release, not main.** Claude Code reads the marketplace catalog from
the default branch but fetches the plugin itself from the `v<ver>` tag the catalog names. Once
the version PR merges, the catalog on main names a tag that does not exist until the release
workflow pushes it, so a `/plugin install` or update in that window fails to fetch; it resolves
as soon as the tag exists. Users on the previous version keep it until then. The pinned
`callimachus-mcp@<ver>` likewise resolves once the npm publish of that release lands.

**Nothing is public until it is complete.** The release starts as a draft. Every
platform uploads into that draft, and `finalize` publishes it only after all
three legs succeeded, all six standalone binaries are attached and `latest.json`
covers every platform. So the updater (`releases/latest/download/latest.json`)
never sees a half-uploaded release, and a failed leg leaves a draft to fix and
re-run instead of a broken "Latest". The extension is the exception:
`publish-extension.yml` doesn't wait for `build.yml`, so it reaches the VS Code
Marketplace and Open VSX as soon as the tag is pushed, even if a desktop leg
fails later.

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

`releases/latest` always resolves to the newest published **non-prerelease**, so
the stable updater never picks up a release candidate or a draft.

### 3. Add the GitHub repo secrets and variables

Settings → Secrets and variables → Actions.

**Secrets:**

| Secret | What |
| --- | --- |
| `TAURI_SIGNING_PRIVATE_KEY` | Contents of `~/.tauri/callimachus.key` |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | The password you set (empty string if none) |
| `RELEASE_TOKEN` | A PAT with `contents: write` + `workflows`. Lets the auto-pushed `v<ver>` tag trigger `build.yml` + `publish-extension.yml` (the default `GITHUB_TOKEN` cannot trigger another workflow). |
| `VSCE_PAT` | *(optional)* Azure DevOps PAT for the VS Code Marketplace. Absent → that publish step is skipped. |
| `OVSX_PAT` | *(optional)* [Open VSX](https://open-vsx.org) access token. This is the registry **Cursor** & VSCodium install from. Absent → that publish step is skipped. |
| Apple, Azure and GPG signing secrets | *(optional)* See [SIGNING.md](SIGNING.md). Each one only turns signing on; absent → unsigned artifact, no failure. |

**Variables** (not secrets; a PostHog project token is a public, write-only
ingestion key):

| Variable | What |
| --- | --- |
| `POSTHOG_PROJECT_TOKEN` | *(optional)* PostHog project token for the desktop app's **opt-in** analytics. Compiled into release builds only (via `option_env!`, on the tauri-action step), never into the CLI or MCP server. Absent → release builds contain no token and can't send analytics at all. |
| `POSTHOG_HOST` | *(optional)* PostHog ingestion host. Absent → the app's default, `https://us.i.posthog.com`. |

If you turn analytics on, also enable **Discard client IP data** in that PostHog
project: the [privacy policy](apps/web/src/content/legal.ts) says IP addresses
are discarded. The website's PostHog setup is documented in
[apps/web/README.md](apps/web/README.md).

> The extension always lands as a downloadable `.vsix` on the GitHub Release even
> with no marketplace tokens set.

### 4. npm trusted publishing

The `callimachus-mcp` wrapper is published from CI with
[npm trusted publishing](https://docs.npmjs.com/trusted-publishers) (GitHub OIDC,
no long-lived token, provenance attached automatically). Once, on npmjs.com:
**callimachus-mcp → Settings → Trusted publishing → GitHub Actions**, with
organization `BetaBots-LLC`, repository `callimachus`, workflow `build.yml`, and
no environment. Then delete any old `NPM_TOKEN` repo secret; nothing reads it.

The first npm and MCP-registry publish should be done by hand (see
"MCP registry" below) so you can claim the `io.github.BetaBots-LLC/*` namespace
and confirm the npm package validates; after that, every release refreshes both
automatically.

The registry step waits for npm to serve the new version (its package document lags a
publish by 15+ minutes behind the CDN, the per-version endpoint by about a minute) and
treats a "duplicate version" answer as success, so a re-run of `build.yml` for a published
tag is always safe.

## Normal release (stable)

1. With each PR that changes the app, add a changeset:
   ```bash
   pnpm changeset          # default to patch; write a one-liner
   ```
   Commit the generated `.changeset/*.md` file. The website (`callimachus-web`) is
   ignored by Changesets, so mention user-visible website changes, especially
   privacy-policy changes, in the `callimachus` changeset of the same PR.
2. Merge PRs to `main`. `version.yml` opens (and keeps updating) a
   **"Version Packages"** PR that accumulates the pending changesets.
3. **Merge the Version Packages PR.** That bumps the shared version, writes the
   changelogs, and pushes a single `vX.Y.Z` tag.
4. That one `v<ver>` tag fans out to **both** workflows:
   - `build.yml` opens a **draft** release whose notes are this version's
     section of `apps/desktop/CHANGELOG.md`, builds signed installers for
     **macOS (Apple Silicon), Windows (x64) and Linux (x64)** plus the standalone
     `cal-<triple>` / `callimachus-mcp-<triple>` binaries (for CLI/MCP-only
     users), and uploads `latest.json`. `finalize` then verifies the draft,
     attaches `SHA256SUMS`, and publishes it. Installed apps auto-update. Finally
     `publish-mcp` publishes the `callimachus-mcp` npm wrapper (bundling that
     `SHA256SUMS`, which the wrapper checks every download against) and refreshes
     the MCP registry entry (skipped on prereleases).
   - `publish-extension.yml` packages the extension, pushes it to the VS Code
     Marketplace + Open VSX, then waits for the draft and attaches the `.vsix` to
     **that same Release**.

If a platform leg fails, the release stays a draft, and `finalize` and
`publish-mcp` don't run (the extension is already on the marketplaces; only its
`.vsix` waits in the draft).
Fix the cause and use **Re-run failed jobs**: the legs upload into the same draft
(assets are replaced), and `finalize` publishes once everything is there.

## Extensions (VS Code & Cursor)

There is one extension (`apps/vscode`) published to two registries: **Cursor and
VSCodium install from Open VSX**, official VS Code from the Marketplace.

- It shares the desktop app's version (the Changesets **fixed** group bumps both
  together), so it ships on the same `v<ver>` tag; there is no separate extension
  tag. `publish-extension.yml` triggers on `v[0-9]*` alongside `build.yml`.
- `publish-extension.yml` runs `vsce` (Marketplace) and `ovsx` (Open VSX); each is
  skipped if its token secret is missing, and the `.vsix` is always attached to the
  same GitHub Release for manual install. It never creates the release itself.
- The extension publish is skipped on `-rc` / `-beta` tags (the Marketplace doesn't
  accept prerelease versions), so RCs ship the desktop installers only.
- One-time, before the first publish: create the `BetaBotsLLC` publisher on the
  [VS Code Marketplace](https://marketplace.visualstudio.com/manage) and the
  matching namespace on [Open VSX](https://open-vsx.org), then add `VSCE_PAT` /
  `OVSX_PAT` (the publish workflow auto-creates the Open VSX namespace).

## MCP registry

The bundled MCP server is published to the
[official MCP registry](https://registry.modelcontextprotocol.io) as
`io.github.BetaBots-LLC/callimachus`, distributed through the **`callimachus-mcp`
npm package** (`packages/callimachus-mcp`). That package carries no binary: its
`bin` downloads the prebuilt `callimachus-mcp-<triple>` asset from the matching
`v<ver>` GitHub Release on first run, checks it against the `SHA256SUMS` that
`publish-mcp` writes into `lib/` right before `npm publish`, and caches it.
Because it's in the Changesets **fixed** group, its version always equals the
release version, so the download URL always resolves. `scripts/sync-versions.mjs`
keeps `server.json` (the registry manifest) on the same version.

- Per release, `build.yml`'s `publish-mcp` job (after `finalize` published the
  release) runs `npm publish` with trusted publishing, then a pinned,
  checksum-verified `mcp-publisher publish`, authenticating to the registry with
  GitHub OIDC (`id-token: write`, no secret). Re-runs skip an npm version that
  is already published, but the registry step has no such check: the registry
  needs a new version string for every publish, so on a re-run after a
  successful publish, `mcp-publisher publish` fails. That failure is harmless
  (the entry is already current); only a run that never reached the registry
  needs it to succeed. Skipped on `-rc` / `-beta` tags.
- **One-time, before the first publish**, claim the namespace and verify the npm
  package by hand from a clean checkout of the tagged commit:

  ```bash
  # 1. bundle the release checksums, then publish the npm wrapper
  #    (its package.json carries the matching mcpName)
  gh release download v<ver> -p SHA256SUMS -O packages/callimachus-mcp/lib/SHA256SUMS
  cd packages/callimachus-mcp && npm publish --access public && cd -

  # 2. install the registry CLI
  brew install mcp-publisher        # or grab the release binary

  # 3. authenticate (device flow) and publish server.json from the repo root
  mcp-publisher login github        # opens a GitHub device-code prompt
  mcp-publisher publish             # reads ./server.json

  # 4. verify
  curl "https://registry.modelcontextprotocol.io/v0.1/servers?search=io.github.BetaBots-LLC/callimachus"
  ```

  `mcp-publisher login github` must authenticate as a member/owner of the
  **BetaBots-LLC** org, since the `io.github.BetaBots-LLC/*` namespace maps to that
  GitHub org. After this first run, the CI job keeps the entry current.

## Release candidates

RC versioning rides on Changesets' prerelease mode.

```bash
# Enter RC mode (commit the generated .changeset/pre.json to main)
pnpm rc:enter            # = changeset pre enter rc

# From here, every Version Packages merge produces vX.Y.Z-rc.0, -rc.1, …
# (e.g. v0.6.0-rc.1). build.yml publishes these as GitHub *prereleases*
# (never marked Latest), and publish-extension.yml skips them. Each one moves
# the changesets it used into .changeset/pre/ (Changesets v3).

# When the RC is good, leave prerelease mode:
pnpm rc:exit             # = changeset pre exit
# Review .changeset/pre/ first: its changesets become the stable changelog, so
# edit or delete any that no longer apply.
# The next Version Packages merge produces the stable vX.Y.Z.
```

- RC builds are published as **prereleases**, so the stable auto-updater
  (`releases/latest`) ignores them. Testers install RCs by downloading them from
  the GitHub Releases page.
- To give RC testers **auto-updates too** (a real "beta channel"), build the RC
  with an endpoint that points at a moving `rc` manifest and have testers run a
  build configured against it. Not wired yet; see "Adding an RC auto-update
  channel" below. The prerelease model above needs no extra infra.

## Manual / re-run build

Tag exists but the run failed or you skipped automation. Both workflows take the
same `v<ver>` tag:

- Desktop: Actions → **Build & Release** → Run workflow → enter the tag (e.g. `v0.5.0`).
- Extension: Actions → **Publish Extension** → Run workflow → enter the same tag.

Re-running `build.yml` for a tag reuses its existing release. While it is still a
draft, the platform legs rebuild and replace its assets. Once it is published, a
re-run never rebuilds or re-uploads anything: the build legs are skipped,
`finalize` only checks that `SHA256SUMS` and `latest.json` are attached and leaves
the release and its "Latest" flag alone, and only `publish-mcp` runs again (npm skips
a version that is already published; the registry step fails if that version is
already listed, which is harmless). Codesigned builds are not
byte-reproducible, so fresh binaries would stop matching the `SHA256SUMS` bundled
in the published `callimachus-mcp` package, and every `npx` download of that
version would fail its checksum. To ship a fixed build, cut a new patch version
instead.

Or cut the tag by hand. One tag triggers **both** `build.yml` and
`publish-extension.yml`:

```bash
git tag v0.5.0 && git push origin v0.5.0    # → build.yml + publish-extension.yml
```

## CI maintenance

- **Rust toolchain.** CI and release builds use a pinned toolchain
  (`RUST_TOOLCHAIN` in `ci.yml`, `build.yml` and `release-cache.yml`), so a new
  stable release and its new clippy lints never break unrelated PRs. Bump all
  three together, on purpose.
- **Release build cache.** Caches saved on a tag can't be restored by any later
  tag, so `release-cache.yml` builds the release dependency set on `main`
  (whenever `Cargo.toml`/`Cargo.lock` change, and weekly to keep it from being
  evicted) and `build.yml` only restores it. Keep its matrix and cargo
  invocations in step with `build.yml`.
- **Dependencies and actions.** Dependabot (`.github/dependabot.yml`) opens weekly
  grouped minor/patch PRs for npm, Cargo and GitHub Actions. Third-party actions
  are pinned to commit SHAs with a `# vX.Y.Z` comment, which Dependabot updates.
  The `mcp-publisher` and `artifact-signing-cli` downloads in `build.yml` are
  pinned by version and SHA-256; bump those by hand.
- **Docker image.** `docker.yml` builds the `Dockerfile` (used by MCP registries
  such as Glama to verify the server) and checks it answers `tools/list`. It
  builds `-p callimachus-standalone` in `rust:1-trixie` (no GTK or WebKit: the
  standalone member links only the Tauri-free `callimachus-core`) and copies only
  the binary (plus its entrypoint script) into `debian:trixie-slim`
  (glibc must stay >= 2.38 for ONNX Runtime). If the server starts linking a new
  shared library, the runtime stage's `ldd` check fails the build: add the
  library's package to that stage's `apt-get install` line.
- **What each installer holds.** The Rust workspace in `apps/desktop/src-tauri` has
  three crates: `core` (`callimachus-core`, everything without Tauri, including
  `cal`, the MCP server and the background jobs), the app crate (window, Tauri
  commands, telemetry) and `standalone`. `tauri build` bundles every bin of the app
  crate, so the app crate has one (`callimachus`, which also serves `cal` and
  `--mcp`). The standalone `cal` and `callimachus-mcp` are the
  `callimachus-standalone` workspace member (`apps/desktop/src-tauri/standalone`,
  built with `cargo build --release -p callimachus-standalone`, linking core only),
  so the macOS `.app` and the Linux packages carry only the app binary. Neither
  `core` nor `standalone` is versioned (both stay 0.0.0): core's build.rs stamps
  the app crate's version into `cal --version` and the MCP server, so
  `pnpm version-packages` still edits only the app crate's `Cargo.toml`. Windows installs still put `cal.exe`
  and `callimachus-mcp.exe` next to the app: `build.yml` builds the standalone
  bins first, copies them to `src-tauri/binaries/<name>-<triple>.exe`, and passes
  the CI-only `tauri.windows-sidecars.conf.json` overlay (`bundle.externalBin`;
  not in `tauri.conf.json`, because tauri-build fails any Windows `cargo build`
  when an externalBin file is missing). To check a macOS bundle locally:
  `pnpm -C apps/desktop tauri build --debug --bundles app`, then
  `ls apps/desktop/src-tauri/target/debug/bundle/macos/Callimachus.app/Contents/MacOS` lists
  only `callimachus`.
- **Windows uninstaller.** `bundle.windows.nsis.installerHooks` runs
  `windows/hooks.nsh`: before an uninstall it runs
  `callimachus.exe --uninstall-integration` (removing the `/recall` skill, MCP
  entries, Claude Code hooks and `cal.exe`). In-app updates, which run the
  uninstaller with `/UPDATE`, skip it. The `.msi` has no equivalent step (Tauri
  takes installer hooks only for NSIS), so uninstalling an MSI install leaves the
  hooks pointing at the deleted exe. Test both installers on Windows after
  touching either.

## Adding an RC auto-update channel (later)

When you want RC testers to auto-update from a beta channel:

1. Maintain a moving `rc` GitHub release whose `latest.json` always reflects the
   newest RC.
2. Ship a separate RC build whose `plugins.updater.endpoints` points at that
   `rc` manifest (e.g. via a `--config` override in `build.yml` for `-rc` tags).
3. Stable builds keep pointing at `releases/latest`.

Until then, RCs are opt-in downloads: simpler and zero extra hosting.
