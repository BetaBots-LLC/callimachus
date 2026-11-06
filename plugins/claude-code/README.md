# Callimachus for Claude Code

The Callimachus plugin gives Claude Code your past AI coding sessions as memory:

- the `/callimachus:recall` skill, which tells Claude when and how to search your history;
- the `callimachus` MCP server (`npx -y callimachus-mcp@<version>`, pinned to the plugin's
  release): search, recall, `check_decision`, project memory and the write-back tools;
- three hooks: SessionStart injects the repo's memory (or a digest of its recent sessions),
  PreCompact snapshots the session and keeps Claude Code's compaction summary, and SessionEnd
  queues the finished session for distillation.

## What it needs

- **The Callimachus desktop app, 0.11.4 or later** (https://callimachus.app). Only the app
  indexes your agent history; the plugin reads the index it builds. Older apps lack the
  `cal session-end` command the SessionEnd hook runs and do not step aside for the plugin, so
  memory would be injected twice. Keep the app updated: the marketplace serves the plugin from
  the latest release's tag, and its MCP server is pinned to that release.
- **The `cal` CLI at `~/.local/bin/cal`**, which the hooks run. Open the app and click
  **Install the cal CLI** in Settings > Agents (with the plugin enabled, the Claude Code
  button installs only `cal`). Until `cal` exists, the hooks exit quietly and do nothing.
- Node.js 18 or later for `npx`, which downloads the MCP server release binary and checks it
  against the release's `SHA256SUMS`.

## Install

```bash
claude plugin marketplace add BetaBots-LLC/callimachus
```

Then, inside Claude Code:

```text
/plugin install callimachus@callimachus
```

## Using it with the app's own integration

The app can also install the skill, the MCP entry and the hooks into your Claude Code config
directly. Both at once would inject memory twice and start two MCP servers, so while
`callimachus@callimachus` is enabled in your Claude Code user settings the app installs only
`cal`, removes the copies it had installed, and Settings > Agents says the integration is
managed by the Claude Code plugin. Disable the plugin and click **Enable for Claude Code** to
go back. The opt-in proactive recall hook is not part of the plugin and keeps working.

**On Windows, use Settings > Agents instead of the plugin.** Claude Code runs plugin hooks
through Git Bash, and without Git Bash through PowerShell, where the hooks' shell syntax fails
and shows a hook error at every session. The app also keeps its own skill, MCP entry and hooks
on Windows even while the plugin is enabled, so the two would load the skill and the MCP
server twice. Leave the plugin off there and click **Enable for Claude Code** in the app.

## Privacy

Everything stays on your machine. The hooks and the MCP server read and write the local index
only. See https://callimachus.app/privacy.
