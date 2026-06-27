---
"callimachus": patch
---

Stream the Claude Code CLI chat engine token-by-token. The keyless `claude` backend now runs with `--output-format stream-json --include-partial-messages` and forwards each text/thinking delta as it arrives, so replies stream in smoothly instead of popping in all at once when the response completes. (Codex CLI still emits its reply in one chunk.)
