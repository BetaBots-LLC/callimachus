---
"callimachus": patch
---

fix: Authenticode-sign the standalone Windows `cal` and `callimachus-mcp` binaries via Azure Trusted Signing. They previously bypassed the Tauri bundler's signCommand and shipped unsigned, tripping SmartScreen's "Unknown publisher" warning. Gated on the Azure secret, so it's a no-op until Trusted Signing is configured.
