// Checks on the Claude Code plugin and its marketplace catalog. Run with:
//   node --test plugins/test/*.test.mjs
// The Rust test `integration::tests::claude_plugin_mirrors_the_app_install` pins the hooks to
// the app's own HookSpecs; this one guards the manifests and that every hook runs a real `cal`
// subcommand, with no Rust toolchain needed.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..", "..");
const plugin = join(root, "plugins", "claude-code");
const read = (...parts) => readFileSync(join(...parts), "utf8");
const readJson = (...parts) => JSON.parse(read(...parts));
const releaseVersion = readJson(root, "apps", "desktop", "package.json").version;

/** What the plugin's skill copy adds after the bundled resource. The Rust test
 *  `integration::tests::claude_plugin_mirrors_the_app_install` pins the same suffix. */
const PLUGIN_SKILL_SUFFIX = `
## With the Claude Code plugin

The \`cal\` CLI is \`~/.local/bin/cal\` (on Windows, \`cal.exe\` next to the Callimachus app). If a bare \`cal\` is not found or prints a calendar (the system \`cal(1)\` winning on PATH), call it by that full path. The MCP prompts' slash commands carry the plugin's prefix, for example \`/mcp__plugin_callimachus_callimachus__recall\`.
`;

/** The `cal` subcommands, parsed from `COMMANDS` in cli_core.rs (the dispatcher's list). */
function calCommands() {
  const src = read(root, "apps", "desktop", "src-tauri", "core", "src", "cli_core.rs");
  const block = src.match(/pub const COMMANDS: &\[&str\] = &\[([\s\S]*?)\];/);
  assert.ok(block, "COMMANDS not found in cli_core.rs");
  return new Set([...block[1].matchAll(/"([^"]+)"/g)].map((m) => m[1]));
}

test("plugin.json names the plugin and carries the release metadata", () => {
  const manifest = readJson(plugin, ".claude-plugin", "plugin.json");
  assert.equal(manifest.name, "callimachus");
  for (const key of ["version", "description", "homepage", "repository", "license"]) {
    assert.equal(typeof manifest[key], "string", `plugin.json ${key}`);
    assert.ok(manifest[key].length > 0, `plugin.json ${key} is empty`);
  }
  assert.equal(typeof manifest.author?.name, "string");
  assert.equal(manifest.license, "AGPL-3.0-or-later");
  assert.ok(Array.isArray(manifest.keywords) && manifest.keywords.length > 0);
  // sync-versions.mjs keeps it on the release version; Claude Code pins users to it.
  assert.equal(manifest.version, releaseVersion);
});

test("marketplace.json serves the plugin from the release tag, not main", () => {
  const market = readJson(root, ".claude-plugin", "marketplace.json");
  assert.equal(market.name, "callimachus");
  assert.equal(typeof market.owner?.name, "string");
  assert.equal(market.plugins.length, 1);
  const [entry] = market.plugins;
  assert.equal(entry.name, "callimachus");
  // sync-versions.mjs moves the ref with every release, so the hooks and the skill a user
  // gets match the app release they installed.
  assert.deepEqual(entry.source, {
    source: "git-subdir",
    url: "BetaBots-LLC/callimachus",
    path: "plugins/claude-code",
    ref: `v${releaseVersion}`,
  });
});

test("every hook is a plain sh script in the plugin that runs a real cal subcommand behind the missing-cal guard", () => {
  const known = calCommands();
  const { hooks } = readJson(plugin, "hooks", "hooks.json");
  const events = Object.keys(hooks).sort();
  assert.deepEqual(events, ["PreCompact", "SessionEnd", "SessionStart"]);
  // Anthropic's directory allows no variable but ${CLAUDE_PLUGIN_ROOT} in a hook command, so
  // the command names a script and the script holds the guard: exit quietly without cal, ask
  // `cal help` first when the command is newer than the app may be, then run one subcommand
  // with at most one flag (`index --if-stale`, the freshness guard).
  const command = /^"\$\{CLAUDE_PLUGIN_ROOT\}"\/scripts\/([a-z-]+)\.sh$/;
  const run = /^exec ~\/\.local\/bin\/cal ([a-z-]+)( --[a-z-]+)?$/;
  const probe =
    /^~\/\.local\/bin\/cal help 2>\/dev\/null \| grep -q '\^ {2}cal ([a-z-]+) ' \|\| exit 0$/;
  for (const event of events) {
    for (const group of hooks[event]) {
      for (const hook of group.hooks) {
        assert.equal(hook.type, "command");
        const m = hook.command.match(command);
        assert.ok(m, `${event}: unexpected command ${hook.command}`);
        const lines = read(plugin, "scripts", `${m[1]}.sh`).trimEnd().split("\n");
        assert.equal(lines[0], "#!/bin/sh", `${m[1]}.sh: a plain sh script`);
        assert.ok(
          lines.includes("[ -x ~/.local/bin/cal ] || exit 0"),
          `${m[1]}.sh: no missing-cal guard`,
        );
        const last = lines.at(-1).match(run);
        assert.ok(last, `${m[1]}.sh: does not end by running one cal subcommand: ${lines.at(-1)}`);
        assert.equal(last[1], m[1], `${m[1]}.sh: the script is named after another command`);
        assert.ok(known.has(last[1]), `${event}: \`cal ${last[1]}\` is not in COMMANDS`);
        const probed = lines.map((l) => l.match(probe)).find(Boolean);
        if (probed)
          assert.equal(probed[1], last[1], `${m[1]}.sh: the help probe checks another command`);
        assert.ok(Number.isInteger(hook.timeout) && hook.timeout > 0, `${event}: timeout`);
      }
    }
  }
});

test("SessionStart also runs the freshness guard, which starts cal index in the background", () => {
  const { hooks } = readJson(plugin, "hooks", "hooks.json");
  const scripts = hooks.SessionStart.flatMap((group) => group.hooks.map((h) => h.command));
  assert.ok(scripts.some((c) => c.endsWith("/scripts/hook.sh")));
  const guard = hooks.SessionStart.find((group) =>
    group.hooks.some((h) => h.command.endsWith("/scripts/index.sh")),
  );
  assert.ok(guard, "no `cal index --if-stale` SessionStart hook");
  // Only where a session's work starts; clear and compact come after one of these checked.
  assert.equal(guard.matcher, "startup|resume");
  // Released apps before this one have no `cal index`: their `cal` answered "unknown
  // command" with exit 1, a hook error at every session start. The script asks `cal help`
  // first and stays silent when the line is missing.
  const script = read(plugin, "scripts", "index.sh");
  assert.match(
    script,
    /^~\/\.local\/bin\/cal help 2>\/dev\/null \| grep -q '\^ {2}cal index ' \|\| exit 0$/m,
  );
  assert.match(script, /^exec ~\/\.local\/bin\/cal index --if-stale$/m);
  const usage = read(root, "apps", "desktop", "src-tauri", "core", "src", "cli_core.rs");
  assert.match(usage, /^ {2}cal index {2,}\S/m, "USAGE lists `  cal index` as the probe expects");
});

test("the MCP server is the npm launcher pinned to the release, under the app's server name", () => {
  const mcp = readJson(plugin, ".mcp.json");
  assert.deepEqual(mcp.mcpServers, {
    callimachus: { command: "npx", args: ["-y", `callimachus-mcp@${releaseVersion}`] },
  });
});

test("the skill is the bundled resource plus the plugin's fixed cal note", () => {
  assert.equal(
    read(plugin, "skills", "recall", "SKILL.md"),
    read(root, "apps", "desktop", "src-tauri", "resources", "recall", "SKILL.md") +
      PLUGIN_SKILL_SUFFIX,
  );
});
