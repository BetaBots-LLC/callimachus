---
"callimachus": patch
---

**Fix the macOS keychain prompt that re-popped on every Deny.** The in-memory key cache only stored successful reads and "no entry" results, a denied / cancelled / locked-keychain read returned an error without caching, so the next `has_key` probe re-read the keychain and macOS re-prompted, on and on. Denials are now cached for the session (respecting your Deny), so it asks at most once; re-enter a key or restart to retry.

Paired with the new CLI backends, keyless engines stay out of the keychain entirely: `provider_has_key` short-circuits for Ollama and the CLI providers without a keychain read, and the chat view skips the key probe when a keyless engine is selected. Net effect: pick a CLI (or Ollama) and Callimachus never touches your keychain.
