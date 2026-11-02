# Code signing

What gets signed, per platform. All of it is **gated on secrets**: a missing
secret means that artifact is built unsigned, not that the build fails. Forks and
local builds therefore work without any of these credentials.

| Artifact | Signing | Status |
| --- | --- | --- |
| Updater (`*.sig` + `latest.json`) | minisign (`TAURI_SIGNING_PRIVATE_KEY`) | ✅ live |
| macOS `.dmg` / `.app` | Developer ID + notarization (App Store Connect API key), stapled | ✅ live |
| macOS standalone `cal-…` / `callimachus-mcp-…` | Developer ID (hardened runtime) + notarization | ✅ wired (same secrets) |
| Windows `.exe` / `.msi` | Authenticode via Azure Trusted Signing | ✅ live |
| Windows standalone `cal-….exe` / `callimachus-mcp-….exe` | Authenticode via Azure Trusted Signing | ✅ live |
| Linux `.AppImage` | GPG (this doc) | ✅ live |
| Linux `.deb` / `.rpm` | not signed by Tauri | n/a (apt/dnf repos sign at the repo level) |

Secrets are never set job-wide: each one is passed to the steps that use it,
and the jobs only see booleans (`HAS_APPLE_CERT`, `HAS_AZURE`, …) to decide what
to run. So `pnpm install` and its third-party lifecycle scripts run with no
signing material at all.

That isolation has a limit. The `tauri-action` build step has to hold signing
credentials, and it also runs third-party build code: the tauri CLI, `pnpm build`
(vite and its plugins) and every crate's `build.rs`. A compromised npm or cargo
dependency could read whatever that step holds. To narrow that, each platform's
credentials are gated on `runner.os`: the macOS leg gets only the Apple
certificate and API key, the Windows leg only the Azure credentials, and the
Linux leg only the GPG key ID and passphrase (an earlier step imports the GPG
private key into that runner's keyring). Every leg also gets the updater key
(`TAURI_SIGNING_PRIVATE_KEY`), because every platform's updater artifacts are
signed with it.

---

## Linux: AppImage (GPG)

Per the [Tauri docs](https://v2.tauri.app/distribute/sign/linux/), the AppImage is
GPG-signed at build time. Note this is a **soft** signature: AppImage doesn't verify
on launch; users validate manually (see below).

### 1. Generate a signing key (once)

```bash
gpg2 --full-gen-key          # RSA 4096, no expiry or a long one; set a passphrase
gpg --list-secret-keys --keyid-format=long   # note the long KEY ID (after rsa4096/)
```

### 2. Export

```bash
KEYID=<your-long-key-id>
gpg --armor --export-secret-keys "$KEYID" > callimachus-signing-private.asc   # SECRET
gpg --armor --export             "$KEYID" > callimachus-signing-public.asc    # publish
```

- Commit the **public** key (`assets/callimachus-signing-public.asc`) and serve it over TLS
  so users can confirm the key ID over an authenticated channel. It is published at
  **https://callimachus.app/callimachus-signing-public.asc** (served from `apps/web/public/`).
- **Never** commit the private key.

### 3. Add the GitHub repo secrets

Settings → Secrets and variables → Actions:

| Secret | Value |
| --- | --- |
| `LINUX_GPG_PRIVATE_KEY` | Contents of `callimachus-signing-private.asc` (the whole ASCII block). **Gates signing**: present = sign. |
| `LINUX_GPG_PASSPHRASE` | The key's passphrase |
| `LINUX_GPG_KEY_ID` | *(optional)* The long key ID. If omitted, the only imported key is used. |

`build.yml` imports the key on the Linux runner and sets `SIGN=1` +
`SIGN_KEY` (from `LINUX_GPG_KEY_ID`) + `APPIMAGETOOL_SIGN_PASSPHRASE` +
`APPIMAGETOOL_FORCE_SIGN=1` automatically, only on Linux and only when
`LINUX_GPG_PRIVATE_KEY` is set.

### 4. Verify a build

Inspect the embedded signature:

```bash
./Callimachus_<version>_amd64.AppImage --appimage-signature
```

End-user verification (from your download page):

```bash
# one-time: grab the validator
# https://github.com/AppImageCommunity/AppImageUpdate/releases/tag/continuous
chmod +x validate-x86_64.AppImage
./validate-x86_64.AppImage Callimachus_<version>_amd64.AppImage
```

Confirm the reported key ID matches the one published at
**https://callimachus.app/callimachus-signing-public.asc**.

---

## macOS: Developer ID + notarization

Needs an Apple Developer membership and a **Developer ID Application** certificate.
Notarization uses an **App Store Connect API key**, not an Apple ID + app-specific
password (repeated failed CI logins with a password can lock the Apple account).

| Secret | Value |
| --- | --- |
| `APPLE_CERTIFICATE` | The Developer ID Application certificate + private key, exported as `.p12` and base64-encoded. **Gates signing.** |
| `APPLE_CERTIFICATE_PASSWORD` | The `.p12` export password |
| `APPLE_SIGNING_IDENTITY` | e.g. `Developer ID Application: Your Name (TEAMID)` |
| `APPLE_API_KEY` | The App Store Connect API **key ID**. **Gates notarization.** |
| `APPLE_API_ISSUER` | The API key's issuer ID |
| `APPLE_API_KEY_BASE64` | The downloaded `AuthKey_<id>.p8`, base64-encoded |

- **The app** (`.app` / `.dmg`): tauri-action signs with hardened runtime,
  notarizes and staples it.
- **The standalone `cal` / `callimachus-mcp` binaries**: `build.yml` imports the
  same certificate into a throwaway keychain, signs both with
  `codesign --options runtime --timestamp`, and notarizes them with
  `xcrun notarytool` using the same API key. Bare Mach-O files can't be stapled,
  so Gatekeeper checks the ticket online the first time a downloaded copy runs.
  Without the API key they are signed but not notarized (a warning, not a failure).

Check a downloaded binary:

```bash
codesign -dv --verbose=2 cal-aarch64-apple-darwin      # Authority=Developer ID Application: …
spctl -a -t exec -vv cal-aarch64-apple-darwin           # accepted, source=Notarized Developer ID
```

## Windows: Azure Trusted Signing

Authenticode via [Azure Trusted Signing](https://learn.microsoft.com/azure/trusted-signing/)
(now called Azure Artifact Signing). The publisher shown to users is the validated
legal name on the signing account. SmartScreen warnings fade as the certificate
builds reputation, not immediately.

| Secret | Value |
| --- | --- |
| `AZURE_CLIENT_ID` | Service principal client ID. **Gates signing.** |
| `AZURE_TENANT_ID` | Tenant ID |
| `AZURE_CLIENT_SECRET` | Service principal secret (needs access to the signing account) |

- The `signCommand` that runs `artifact-signing-cli` lives in a CI-only overlay,
  `apps/desktop/src-tauri/tauri.windows-sign.conf.json`, which `build.yml` passes
  to `tauri build` (`--config`) only when `AZURE_CLIENT_ID` is set. Local and fork
  Windows builds don't need the CLI and produce unsigned installers.
- `build.yml` downloads a pinned, SHA-256-verified `artifact-signing-cli.exe`
  (0.10.0) instead of compiling it, and also signs the standalone `.exe` binaries
  with it. Upstream has been renamed to `artifact-signing-cli`; moving to it means
  changing the command in the overlay and the standalone signing step together,
  and its environment variable names changed too.
