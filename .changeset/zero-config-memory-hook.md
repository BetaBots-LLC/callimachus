---
"callimachus": minor
---

**Zero-config memory injection.** The one-click Claude Code integration now also installs a **SessionStart hook**, so each repo's distilled memory (decisions, gotchas, open TODOs) is automatically injected at the start of every Claude Code session — no manual hook setup, and nothing to remember to run.

- The "Enable for Claude Code" action now writes a Callimachus `SessionStart` hook into `~/.claude/settings.json` alongside the `/recall` skill, the MCP server, and the `cal` CLI. It's merged safely (preserves your other settings and hooks, refuses to touch an unparseable file) and is fully idempotent — re-installing never duplicates it.
- "Remove" cleanly strips the hook (and only ours) back out.
- The Settings card shows the hook's status and Reinstall picks it up for anyone who set up the integration before this release.

**One-click multi-agent setup.** A new "Other agents" section in Settings registers the `callimachus` MCP server with the *other* agents you have installed — **Codex** (`~/.codex/config.toml`), **Cursor** (`~/.cursor/mcp.json`), and **Gemini CLI** (`~/.gemini/settings.json`) — so they can search your history too. It only touches agents whose config already exists (never creates one for an agent you don't use), merges safely (preserves the rest of each config, refuses unparseable files, format-preserving for Codex's TOML), is idempotent, and is fully removable. The per-repo "Update AGENTS.md" action already covers agents that read `AGENTS.md`.
