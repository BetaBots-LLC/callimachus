---
"callimachus": patch
---

**Use your agent CLI instead of an API key.** Distillation, Ask, project memory, and the in-app chat can now run on your logged-in **Claude Code** or **Codex** CLI subscription, no raw API key required. Pick "Claude Code CLI (subscription · no key)" as the distillation engine, or select it in the chat provider dropdown, and Callimachus shells out to the CLI in non-interactive print mode (tools off, neutral cwd) to get the completion.

- **Keyless, like Ollama:** CLI backends need no stored key. The engine/provider pickers offer whichever CLIs are installed, and the key field disappears when one is selected.
- **PATH-aware:** a GUI app launched from Finder doesn't inherit your shell PATH (nvm/homebrew/npm dirs), so the CLI is resolved via your login shell, the same `claude` you use in the terminal.
- **Honest privacy note:** unlike Ollama, CLI distillation still sends thread text to that CLI's provider (via your subscription); the UI says so. It's "no key", not "on-device".
- **Knowledge completions** (distill, ask, project brief, conflict review) route through the CLI cleanly. **In-app chat** runs as a single completion per turn, the genai history-search tools and token streaming are keyed-provider only, so CLI chat is plain Q&A.

Claude Code is verified end-to-end; Codex is wired to `codex exec` but untested locally. Refactors the five LLM call sites onto one `complete()` helper that branches CLI vs genai.
