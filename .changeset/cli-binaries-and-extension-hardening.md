---
"callimachus": patch
"callimachus-vscode": patch
---

Ship standalone `cal` and `callimachus-mcp` binaries on every release for CLI/MCP-only users, and make the bundled `cal` resolve on Windows — the desktop app now places `cal.exe` in its install directory, where the VS Code / Cursor extension already looks, so the extension works on Windows without a manual PATH edit.

Harden the extension's webview RPC: unknown methods now raise an error instead of silently returning nothing, `cal --json` output is parsed defensively (a clear message instead of a raw `SyntaxError`), and transcript attribute matching escapes its pattern.
