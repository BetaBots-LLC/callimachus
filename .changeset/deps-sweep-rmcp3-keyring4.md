---
"callimachus": patch
---

Dependency sweep: every direct dependency is current and the security audits are clean (0 Rust advisories, 0 production npm advisories).

- **MCP server:** moved to rmcp 3.8. The initialize handshake and the 22-tool surface are unchanged (a test pins the negotiated protocol versions), so Claude Code, Codex and Cursor connect as before.
- **Keychain:** keyring 4. On Linux the Secret Service backend changed to the zbus-based store; macOS Keychain and Windows Credential Manager behave the same.
- **Release tooling:** Changesets 3 (private packages stay versioned), changesets/action 2, and the actions/checkout, setup-node and pnpm/action-setup majors, all pinned to commit SHAs. tauri-action stays on 0.6.2 on purpose, so updater downloads keep using browser URLs instead of rate-limited API asset URLs.
- Held back: fastembed 7 (its ONNX runtime build needs compiler-rt linking proven on CI and raises the macOS minimum), rusqlite 0.41 (rusqlite_migration does not support it yet), and prerelease-only crates.
