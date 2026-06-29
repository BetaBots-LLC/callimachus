---
"callimachus": patch
---

feat: index VS Code-native / GitHub Copilot chat and capture per-message models. A new
`copilot` source reads `chatSessions` and `emptyWindowChatSessions` across VS Code-family
editors (Code, Cursor, VSCodium, Windsurf, Insiders), extracting user/assistant turns, the
project, timestamps, and the model that produced each assistant turn (e.g. `gpt-5.3-codex`).
The per-message model is now surfaced in the thread view. Also fixes the file watcher's
live-reindex routing on Windows (it matched forward-slash paths only), so auto-reindex now
works on Windows for every source.
