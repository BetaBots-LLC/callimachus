# Callimachus for Codex (experimental)

The Callimachus plugin gives Codex your past AI coding sessions as memory:

- the `recall` skill, which tells the agent when and how to search your history;
- the `callimachus` MCP server, run with `npx -y callimachus-mcp@<version>`, pinned to the
  plugin's release: search, recall, `check_decision`, project memory and the write-back tools;
- the memory hooks: `SessionStart` (startup, clear, compact) injects the repo's memory (or a
  digest of its recent sessions), `SessionStart` (startup, resume) also keeps the index fresh
  when the desktop app is not running (`cal index --if-stale`), `PreCompact` snapshots the
  session before compaction, and `SessionEnd` queues the finished session for distillation.

**Experimental.** The plugin is built from Codex's plugin loader source (tag `rust-v0.167.0`)
and the layout of the plugins bundled with Codex. It has not been install-tested in Codex yet,
and Codex runs its hooks only after you trust them (see below).

Everything stays on your machine. The hooks and the MCP server read and write the local index
only. See https://callimachus.app/privacy.

## What it needs

- **The Callimachus desktop app from the plugin's release or later** (https://callimachus.app),
  or the `cal` CLI on its own (`npx callimachus-mcp --install-cal`, then `cal index`). The app
  indexes your agent history as it grows and the plugin reads that index. Without the app, the
  plugin's `cal index --if-stale` hook starts a pass in the background at a session start or
  resume when none ran in the last hour and neither the app nor `cal watch` runs, as the
  Claude Code plugin's does. With an older app the plugin's memory hooks stay silent: its
  `cal` does not claim each event, so beside the app's own Codex hooks it would inject the
  memory twice.
- **The `cal` CLI at `~/.local/bin/cal`**, which the hooks run. Open the app and click
  **Install the cal CLI** in Settings > Agents. Each hook is a short shell script in the
  plugin's `scripts/` folder; read them to see exactly what runs. Until `cal` exists, or while
  it is too old to claim each event (the scripts ask `cal help` first), the memory hooks exit
  quietly and do nothing.
- **Node.js 18 or later** for `npx`, which fetches the `callimachus-mcp` launcher from npm.
  The launcher downloads the MCP server binary from the matching GitHub release, checks it
  against that release's `SHA256SUMS` before running it, and caches it.
- **macOS or Linux.** The hooks are `sh` scripts; on Windows, use Settings > Agents instead.

## Install

Codex's plugin docs (developers.openai.com/codex/plugins/build) add a marketplace from a Git
repository with the CLI, then install and test the plugin in the ChatGPT desktop app:

```bash
codex plugin marketplace add BetaBots-LLC/callimachus
```

Then install **Callimachus** from that marketplace in the ChatGPT desktop app and restart it
(the docs say lifecycle hooks are supported for plugins installed manually in Codex desktop).
`codex plugin marketplace upgrade` refreshes the marketplace. It serves the plugin from the
latest release tag, so keep the app on the latest release too.

Installing a plugin does not trust its hooks: Codex skips them until you review and trust
them (run `/hooks` in Codex and approve the Callimachus entries), and asks again whenever an
update changes the plugin's files. Trusting them is what makes the plugin's hooks run.

## Using it with the app's own integration

Settings > Agents in the app can also write Codex hooks to `~/.codex/hooks.json` and the MCP
entry `[mcp_servers.callimachus]` to `~/.codex/config.toml`.

- **The MCP entry** goes while the plugin is enabled (`[plugins."callimachus@<marketplace>"]`
  in `~/.codex/config.toml`, with `enabled` not `false`) and installed in Codex's plugin
  folder, since Codex then starts the plugin's server and two servers of one name would list
  every tool twice. The app puts its entry back once the plugin is off or uninstalled, with
  any keys you had added to it. An entry you add back while the plugin is on stays.
- **The hooks** stay: the app's hooks work whether or not you trust the plugin's. Once you do,
  Codex runs both for every event and the first to run claims it, so the memory is injected
  once and the session snapshotted once. While the plugin is enabled, trusted or not, a
  second clear or compaction of one session within 5 seconds injects nothing.

This repository's `.agents/plugins/marketplace.json` lists this plugin. Codex reads the first
of `.agents/plugins/marketplace.json`, `.agents/plugins/api_marketplace.json`,
`.claude-plugin/marketplace.json` and `.cursor-plugin/marketplace.json` in a repository, so it
offers this plugin and no longer the Claude Code one. Claude Code and Cursor read their own
catalogs as before.
