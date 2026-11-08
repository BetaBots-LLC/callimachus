---
"callimachus": patch
---

The Claude Code plugin now ships the MCP launcher inside the plugin (`mcp/`, a copy of the `callimachus-mcp` npm package) and runs it with `node` instead of `npx`. It downloads the same release binary and checks it against the release's `SHA256SUMS` before running it, with no npm registry lookup at session start. The plugin also gains a listing icon.
