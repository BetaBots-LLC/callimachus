# Callimachus for Claude Code

The Callimachus plugin gives Claude Code your past AI coding sessions as memory:

- the `/callimachus:recall` skill, which tells Claude when and how to search your history;
- the `callimachus` MCP server, run with `node` from the launcher in the plugin's `mcp/` folder
  (a copy of the `callimachus-mcp` npm package, pinned to the plugin's release): search,
  recall, `check_decision`, project memory and the write-back tools;
- the memory hooks: SessionStart injects the repo's memory (or a digest of its recent
  sessions) and keeps the index fresh when the desktop app is not running, PreCompact
  snapshots the session and keeps Claude Code's compaction summary, and SessionEnd queues the
  finished session for distillation.

## What it needs

- **The Callimachus desktop app, 0.11.4 or later** (https://callimachus.app), or the `cal` CLI
  on its own (see [Without the desktop app](#without-the-desktop-app)). The app indexes your
  agent history and the plugin reads that index. Older apps lack the
  `cal session-end` command the SessionEnd hook runs and do not step aside for the plugin, so
  memory would be injected twice. Keep the app updated: the marketplace serves the plugin from
  the latest release's tag, and its MCP server is pinned to that release.
- **The `cal` CLI at `~/.local/bin/cal`**, which the hooks run (each hook is a short shell
  script in the plugin's `scripts/` folder; read them to see exactly what runs). Open the app and click
  **Install the cal CLI** in Settings > Agents (with the plugin enabled, the Claude Code
  button installs only `cal`), or run `npx callimachus-mcp --install-cal`. Until `cal`
  exists, the hooks exit quietly and do nothing.
- Node.js 18 or later, which runs the MCP launcher inside the plugin (`mcp/bin/callimachus-mcp.mjs`,
  plain JavaScript with no dependencies; read it to see exactly what runs). It downloads the MCP
  server binary from the plugin's GitHub release, checks it against that release's
  `SHA256SUMS` before running it, and caches it, as the npm package does (which carries the
  checksum file in its tarball instead). Nothing is fetched from the npm registry.

## Install

```bash
claude plugin marketplace add BetaBots-LLC/callimachus
```

Then, inside Claude Code:

```text
/plugin install callimachus@callimachus
```

## Without the desktop app

The plugin works on a Mac or Linux machine that never installs the app (a server, an SSH box,
a machine where you only use Claude Code):

```bash
npx callimachus-mcp --install-cal   # download cal from the release, link it at ~/.local/bin/cal
cal index                           # build the index from your agents' history (once)
cal embed                           # optional: the semantic index (downloads a 128 MB model once)
```

`--install-cal` fetches `cal-<target>` from the same GitHub release as the MCP server and checks
it against the same `SHA256SUMS`. It never replaces a `~/.local/bin/cal` that belongs to another
program (a regular file, or a link to something else), and it leaves a link to a Callimachus
build (the app's, or one you built) alone. The system calendar's `cal(1)` lives in `/usr/bin`,
so it is never touched, but in a shell where `~/.local/bin` comes first on your `PATH` the new
link shadows it: run `/usr/bin/cal` for the calendar. If `~/.local/bin` is not on your `PATH`,
it prints the line to add; the hooks call the full path, so they work either way.

From then on the plugin keeps the index fresh by itself: at each session start or resume, its
SessionStart hook runs `cal index --if-stale`, which starts a catch-up pass in the background
when no full pass finished in the last hour and neither the app nor `cal watch` is running.
The session never waits on the pass, and the hook never creates the index (that first `cal
index` is yours). The pass appends its output to `index.db.index.log` next to the index, and
a start the hook declines because of the index's schema adds a line there saying why. A `cal`
from an app release without `cal index` makes the hook do nothing. For an index that is fresh
to the minute, run `cal watch` in a terminal or as a login item instead; it exits with code 3
when a newer Callimachus upgraded the index, so a login item can restart it.

What you give up without the app: the Projects page, Chat, Settings, and the harvest controls
and review for memories imported from Claude Code's own memory files (`cal watch` still imports
them on its minute tick). Memory distillation (decisions and gotchas extracted from your
sessions) is switched on in the app's Settings > AI engine, so without the app the hooks inject
a digest of the repo's recent sessions plus anything you record with `cal remember` or the MCP
write tools. Search, recall, `check_decision`, snapshots and the hooks work the same.

**Version coupling.** The plugin pins its MCP server to its own release (the launcher in
`mcp/` downloads that release's binary), and each time that server starts it moves a `cal` linked
by `--install-cal` up to the same release, so `cal` and the server open the index with the same
schema. The exception is another, newer launcher on the machine (an unpinned `npx -y
callimachus-mcp` in another MCP client): it moves `cal` ahead, nothing moves it back, and its
next pass upgrades the index past this plugin's server, which then says to update; updating
the plugin brings the two together again. The launcher never touches the app's link.

When a release upgrades the index schema, the MCP server and the memory hooks leave the index
as it is and the server says so. Without the app, the next session start applies the upgrade:
the background pass of a `cal` that `--install-cal` linked is the one hook allowed to upgrade
the index, and only while the app is not running. A `cal` linked to the app never upgrades it
from a hook; open the app to apply the upgrade.

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
