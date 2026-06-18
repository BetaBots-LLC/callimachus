# Code signing

What gets signed, per platform. All of this is **gated on secrets** — absent secrets
just mean an unsigned artifact, never a failed build.

| Artifact | Signing | Status |
| --- | --- | --- |
| Updater (`*.sig` + `latest.json`) | minisign (`TAURI_SIGNING_PRIVATE_KEY`) | ✅ live |
| Linux `.AppImage` | GPG (this doc) | ⏳ add secrets |
| macOS `.dmg`/`.app` | Apple notarization | ☐ TODO (needs Apple Developer membership) |
| Windows `.exe`/`.msi` | Authenticode | ☐ TODO (needs a code-signing cert) |
| Linux `.deb` | not signed by Tauri | n/a (apt repos sign at the repo level) |

---

## Linux — AppImage (GPG)

Per the [Tauri docs](https://v2.tauri.app/distribute/sign/linux/), the AppImage is
GPG-signed at build time. Note this is a **soft** signature: AppImage doesn't verify
on launch — users validate manually (see below).

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
| `LINUX_GPG_PRIVATE_KEY` | Contents of `callimachus-signing-private.asc` (the whole ASCII block). **Gates signing** — present = sign. |
| `LINUX_GPG_PASSPHRASE` | The key's passphrase |
| `LINUX_GPG_KEY_ID` | *(optional)* The long key ID. If omitted, the only imported key is used. |

`build.yml` imports the key on the Linux runner and sets `SIGN=1` +
`APPIMAGETOOL_SIGN_PASSPHRASE` + `APPIMAGETOOL_FORCE_SIGN=1` automatically — only on
Linux, and only when `LINUX_GPG_PRIVATE_KEY` is set.

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

## macOS / Windows (TODO)

Added on this branch as the certs become available:

- **macOS** — needs an Apple Developer membership (Developer ID Application cert);
  set `APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`,
  `APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID`. tauri-action notarizes when present.
- **Windows** — needs a code-signing cert (Azure Trusted Signing is cheapest);
  wire `windows.signCommand` in `tauri.conf.json` or the action's signing inputs.
