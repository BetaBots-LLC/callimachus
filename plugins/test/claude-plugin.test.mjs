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
  const src = read(root, "apps", "desktop", "src-tauri", "src", "cli_core.rs");
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

test("every hook runs a real cal subcommand behind the missing-cal guard", () => {
  const known = calCommands();
  const { hooks } = readJson(plugin, "hooks", "hooks.json");
  const events = Object.keys(hooks).sort();
  assert.deepEqual(events, ["PreCompact", "SessionEnd", "SessionStart"]);
  const guard =
    /^\[ ! -x "\$HOME\/\.local\/bin\/cal" \] \|\| "\$HOME\/\.local\/bin\/cal" ([a-z-]+)$/;
  for (const event of events) {
    for (const group of hooks[event]) {
      for (const hook of group.hooks) {
        assert.equal(hook.type, "command");
        const m = hook.command.match(guard);
        assert.ok(m, `${event}: unguarded or unexpected command ${hook.command}`);
        assert.ok(known.has(m[1]), `${event}: \`cal ${m[1]}\` is not in COMMANDS`);
        assert.ok(Number.isInteger(hook.timeout) && hook.timeout > 0, `${event}: timeout`);
      }
    }
  }
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
