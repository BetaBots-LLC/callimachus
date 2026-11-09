# Callimachus for Cursor

The Callimachus plugin gives Cursor your past AI coding sessions as memory:

- the `recall` skill, which tells the agent when and how to search your history;
- the `callimachus` MCP server (Cursor lists it as `plugin-callimachus-callimachus`), run with
  `npx -y callimachus-mcp@<version>`, pinned to the plugin's release: search, recall,
  `check_decision`, project memory and the write-back tools;
- the memory hooks: `sessionStart` injects the repo's memory (or a digest of its recent
  sessions) into each new chat, and `sessionEnd` queues the finished chat for distillation.

Everything stays on your machine. The hooks and the MCP server read and write the local index
only. See https://callimachus.app/privacy.

## What it needs

- **The Callimachus desktop app, 0.12.1 or later** (https://callimachus.app), or the `cal` CLI
  on its own (`npx callimachus-mcp --install-cal`, then `cal index` once). The app indexes your
  agent history and the plugin reads that index. Older apps have no `cal hook --for cursor`, so
  with them the hooks stay silent.
- **The `cal` CLI at `~/.local/bin/cal`**, which the hooks run. Open the app and click
  **Install the cal CLI** in Settings > Agents. Each hook is a short shell script in the
  plugin's `scripts/` folder; read them to see exactly what runs. Until `cal` exists, or while
  it is too old to speak Cursor's hook protocol (the scripts ask `cal help` first), the hooks
  exit quietly and do nothing.
- **Node.js 18 or later** for `npx`, which fetches the `callimachus-mcp` launcher from npm.
  The launcher downloads the MCP server binary from the matching GitHub release, checks it
  against that release's `SHA256SUMS` before running it, and caches it.

## Install

From the Cursor Marketplace, once the plugin is listed there: open **Customize**, search for
Callimachus and click **Install**.

Until then, install it as a local plugin (Cursor loads plugins from `~/.cursor/plugins/local`,
and only folders inside it, not symlinks to elsewhere):

```bash
git clone --depth 1 https://github.com/BetaBots-LLC/callimachus /tmp/callimachus
mkdir -p ~/.cursor/plugins/local
cp -R /tmp/callimachus/plugins/cursor ~/.cursor/plugins/local/callimachus
```

Then run **Developer: Reload Window** and check **Customize** for the skill, the MCP server and
the hooks. On cursor.directory, **Add to Cursor** on the MCP server adds only the server; the
skill and the hooks come with the plugin.

## Using it with the app's own integration

Settings > Agents in the app can also write the Cursor hooks to `~/.cursor/hooks.json` and the
MCP entry to `~/.cursor/mcp.json`. Cursor runs every matching hook from every source, so with
both the memory hook runs twice; `cal hook --for cursor` claims each chat once (a marker file
under `~/.callimachus/recall`), so the memory still arrives once. Both would also start two MCP
servers, `callimachus` and `plugin-callimachus-callimachus`, so keep one: with the plugin
installed, switch the app's `callimachus` server off in Cursor's **Customize** page.

**On Windows, use Settings > Agents instead of the plugin.** The hooks are `sh` scripts, which
Windows runs only through a POSIX shell such as Git Bash; the app writes PowerShell-ready hooks
there instead.
