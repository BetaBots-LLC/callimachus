// Checks on the Cursor plugin (plugins/cursor) and the root marketplace that lists it. Run with:
//   node --test plugins/test/*.test.mjs
// The Rust test `integration::tests::cursor_plugin_mirrors_the_app_install` pins the hooks to
// the app's CURSOR_* HookSpecs; this one guards the manifests, the scripts, the skill, the MCP
// pin, and that cursor.directory's repo parser finds the plugin, with no Rust toolchain needed.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync, statSync } from "node:fs";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..", "..");
const plugin = join(root, "plugins", "cursor");
const read = (...parts) => readFileSync(join(...parts), "utf8");
const readJson = (...parts) => JSON.parse(read(...parts));
const releaseVersion = readJson(root, "apps", "desktop", "package.json").version;

/** What the plugin's skill copy adds after the bundled resource. */
const PLUGIN_SKILL_SUFFIX = `
## With the Cursor plugin

The \`cal\` CLI is \`~/.local/bin/cal\`. If a bare \`cal\` is not found or prints a calendar (the system \`cal(1)\` winning on PATH), call it by that full path. Cursor lists the plugin's MCP server as \`plugin-callimachus-callimachus\`.
`;

/** The `cal` subcommands, parsed from `COMMANDS` in cli_core.rs (the dispatcher's list). */
function calCommands() {
  const src = read(root, "apps", "desktop", "src-tauri", "core", "src", "cli_core.rs");
  const block = src.match(/pub const COMMANDS: &\[&str\] = &\[([\s\S]*?)\];/);
  assert.ok(block, "COMMANDS not found in cli_core.rs");
  return new Set([...block[1].matchAll(/"([^"]+)"/g)].map((m) => m[1]));
}

test("plugin.json names the plugin and carries the release metadata", () => {
  const manifest = readJson(plugin, ".cursor-plugin", "plugin.json");
  // Cursor: "Lowercase, kebab-case (alphanumerics, hyphens, and periods)".
  assert.equal(manifest.name, "callimachus");
  assert.equal(manifest.displayName, "Callimachus");
  for (const key of ["version", "description", "homepage", "repository", "license", "logo"]) {
    assert.equal(typeof manifest[key], "string", `plugin.json ${key}`);
    assert.ok(manifest[key].length > 0, `plugin.json ${key} is empty`);
  }
  assert.deepEqual(manifest.author, { name: "Ari Shaller", url: "https://callimachus.app" });
  assert.equal(manifest.license, "AGPL-3.0-or-later");
  assert.ok(Array.isArray(manifest.keywords) && manifest.keywords.length > 0);
  assert.equal(manifest.version, releaseVersion);
  // The logo is a relative path in the plugin (Cursor resolves it to raw.githubusercontent),
  // the site's own SVG icon.
  assert.equal(manifest.logo, "assets/logo.svg");
  assert.equal(read(plugin, manifest.logo), read(root, "apps", "web", "public", "icon.svg"));
});

test("the root marketplace lists the plugin dir and carries what cursor.directory lists", () => {
  const market = readJson(root, ".cursor-plugin", "marketplace.json");
  assert.equal(market.name, "callimachus");
  assert.equal(typeof market.owner?.name, "string");
  assert.equal(market.metadata?.version, releaseVersion);
  assert.equal(typeof market.metadata?.description, "string");
  assert.deepEqual(
    market.plugins.map((p) => [p.name, p.source]),
    [["callimachus", "./plugins/cursor"]],
  );
  // cursor.directory merges no sub-plugin metadata once the root manifest has a name, so the
  // listing's fields come from here (see the parser replay below). Keep them in step.
  const manifest = readJson(plugin, ".cursor-plugin", "plugin.json");
  assert.equal(market.version, releaseVersion);
  for (const key of ["homepage", "license"]) assert.equal(market[key], manifest[key], key);
  assert.deepEqual(market.keywords, manifest.keywords);
  assert.deepEqual(market.author, manifest.author);
  assert.equal(market.logo, `plugins/cursor/${manifest.logo}`);
  // The Claude Code catalog stays where Claude Code reads it.
  assert.equal(
    readJson(root, ".claude-plugin", "marketplace.json").plugins[0].source.path,
    "plugins/claude-code",
  );
});

test("every hook is a plain sh script that runs a real cal subcommand behind the guards", () => {
  const known = calCommands();
  const { hooks } = readJson(plugin, "hooks", "hooks.json");
  assert.deepEqual(Object.keys(hooks).sort(), ["sessionEnd", "sessionStart"]);
  // Plugin hooks run from the plugin's install dir (cursor.com/docs/hooks), so the command
  // is a relative path to the script. Cursor's entry is flat: command, timeout, matcher.
  const command = /^\.\/scripts\/([a-z-]+)\.sh$/;
  const run = /^exec ~\/\.local\/bin\/cal ([a-z-]+) --for cursor$/;
  const probe = "~/.local/bin/cal help 2>/dev/null | grep -q ' or cursor$' || exit 0";
  const expected = { sessionStart: ["hook", 10], sessionEnd: ["session-end", 5] };
  for (const [event, entries] of Object.entries(hooks)) {
    assert.equal(entries.length, 1, event);
    const [entry] = entries;
    assert.deepEqual(Object.keys(entry).sort(), ["command", "timeout"], `${event}: no matcher`);
    const m = entry.command.match(command);
    assert.ok(m, `${event}: unexpected command ${entry.command}`);
    assert.deepEqual([m[1], entry.timeout], expected[event], event);
    const path = join(plugin, "scripts", `${m[1]}.sh`);
    assert.ok(statSync(path).mode & 0o111, `${m[1]}.sh is executable`);
    const lines = readFileSync(path, "utf8").trimEnd().split("\n");
    assert.equal(lines[0], "#!/bin/sh", `${m[1]}.sh: a plain sh script`);
    const code = lines.filter((l) => !l.startsWith("#"));
    assert.deepEqual(code.slice(0, 2), ["[ -x ~/.local/bin/cal ] || exit 0", probe], m[1]);
    const last = code.at(-1).match(run);
    assert.ok(last, `${m[1]}.sh: does not end by running cal ... --for cursor: ${code.at(-1)}`);
    assert.equal(last[1], m[1], `${m[1]}.sh: the script is named after another command`);
    assert.ok(known.has(last[1]), `\`cal ${last[1]}\` is not in COMMANDS`);
  }
  // The released 0.12.0 cal ends its `--for` help line with "codex or gemini" and has no
  // Cursor protocol; this version's help ends a line with " or cursor", and only that line.
  const usage = read(root, "apps", "desktop", "src-tauri", "core", "src", "cli_core.rs")
    .split("pub(crate) const USAGE")[1]
    .split('";')[0];
  assert.equal(usage.split("\n").filter((l) => / or cursor$/.test(l)).length, 1);
});

test("the MCP server is the npm launcher pinned to the release", () => {
  // npx rather than a vendored copy under ${CURSOR_PLUGIN_ROOT}: cursor.directory's "Add to
  // Cursor" installs this server config on its own, outside the plugin, where a plugin path
  // would not resolve. sync-versions.mjs stamps the pin.
  assert.deepEqual(readJson(plugin, "mcp.json"), {
    mcpServers: {
      callimachus: { command: "npx", args: ["-y", `callimachus-mcp@${releaseVersion}`] },
    },
  });
  assert.equal(
    readJson(root, "packages", "callimachus-mcp", "package.json").version,
    releaseVersion,
  );
});

test("the skill is the bundled resource plus the plugin's fixed Cursor note", () => {
  assert.equal(
    read(plugin, "skills", "recall", "SKILL.md"),
    read(root, "apps", "desktop", "src-tauri", "resources", "recall", "SKILL.md") +
      PLUGIN_SKILL_SUFFIX,
  );
});

// --- cursor.directory's parser, replayed -------------------------------------------------
// The rules of apps/cursor/src/lib/github-plugin/parse.ts in github.com/cursor/community-plugins
// (parseGitHubPlugin), run against this repo's files instead of GitHub's tree and raw APIs.
// A file "exists" only at its exact repo path, as raw.githubusercontent.com serves it.

/** The repo tree as GitHub would list it: tracked files plus new ones not ignored. */
function repoTree() {
  const out = execFileSync(
    "git",
    ["ls-files", "-z", "--cached", "--others", "--exclude-standard"],
    {
      cwd: root,
      encoding: "utf8",
    },
  );
  const deleted = new Set(
    execFileSync("git", ["ls-files", "-z", "--deleted"], { cwd: root, encoding: "utf8" })
      .split("\0")
      .filter(Boolean),
  );
  return [...new Set(out.split("\0").filter((p) => p && !deleted.has(p)))];
}

function parseLikeCursorDirectory(tree) {
  const files = new Set(tree);
  const fetchFile = (path) => (files.has(path) ? read(root, path) : null);
  let manifest = {};
  for (const mp of [
    ".plugin/plugin.json",
    ".cursor-plugin/plugin.json",
    ".claude-plugin/plugin.json",
    ".cursor-plugin/marketplace.json",
  ]) {
    const content = fetchFile(mp);
    if (content) {
      manifest = JSON.parse(content);
      break;
    }
  }
  const subPluginDirs = new Set();
  if (Array.isArray(manifest.plugins)) {
    for (const entry of manifest.plugins) {
      const source = entry?.source;
      if (typeof source === "string" && source && !source.includes("..")) {
        subPluginDirs.add(`${source.replace(/^\/+|\/+$/g, "")}/`);
      }
    }
  }
  for (const path of tree) {
    const m = path.match(/^(.+)\/\.cursor-plugin\/plugin\.json$/);
    if (m) subPluginDirs.add(`${m[1]}/`);
  }
  const components = [];
  for (const prefix of ["", ...subPluginDirs]) {
    const at = (kind, re) => {
      for (const path of tree.filter((p) => new RegExp(`^${prefix}${re}`).test(p))) {
        components.push({ type: kind, path });
      }
    };
    at("rule", "rules/.*\\.mdc$");
    for (const mcpPath of [`${prefix}.mcp.json`, `${prefix}mcp.json`]) {
      const content = fetchFile(mcpPath);
      if (!content) continue;
      const config = JSON.parse(content);
      for (const name of Object.keys(config.mcpServers ?? config)) {
        components.push({ type: "mcp_server", path: mcpPath, name });
      }
    }
    at("agent", "agents/.*\\.md$");
    at("skill", "skills/[^/]+/SKILL\\.md$");
    at("command", "commands/.*\\.md$");
    if (fetchFile(`${prefix}hooks/hooks.json`)) {
      JSON.parse(fetchFile(`${prefix}hooks/hooks.json`));
      components.push({ type: "hook", path: `${prefix}hooks/hooks.json` });
    }
    if (fetchFile(`${prefix}.lsp.json`))
      components.push({ type: "lsp_server", path: `${prefix}.lsp.json` });
  }
  return { manifest, subPluginDirs: [...subPluginDirs], components };
}

test("cursor.directory's parser finds the plugin, its MCP server, skill and hooks", () => {
  const tree = repoTree();
  assert.ok(tree.includes(".cursor-plugin/marketplace.json"), "the marketplace is in the tree");
  const { manifest, subPluginDirs, components } = parseLikeCursorDirectory(tree);
  // The root manifest it settles on is ours, not a stray plugin.json earlier in its list.
  assert.equal(manifest.name, "callimachus");
  assert.ok(Array.isArray(manifest.plugins), "the root manifest is the Cursor marketplace");
  assert.ok(subPluginDirs.includes("plugins/cursor/"), `${subPluginDirs}`);
  assert.deepEqual(
    components.map((c) => `${c.type} ${c.path}${c.name ? ` ${c.name}` : ""}`).sort(),
    [
      "hook plugins/cursor/hooks/hooks.json",
      "mcp_server plugins/cursor/mcp.json callimachus",
      "skill plugins/cursor/skills/recall/SKILL.md",
    ],
    "exactly the Cursor plugin's components, nothing from the repo root or the Claude plugin",
  );
  // The listing's metadata, as parse.ts maps it.
  assert.equal(manifest.version, releaseVersion);
  assert.equal(typeof manifest.description, "string");
  assert.ok(
    !/^https?:/.test(manifest.logo) && tree.includes(manifest.logo.replace(/^\.?\//, "")),
    "the logo resolves to a file at HEAD",
  );
  assert.equal(manifest.author?.name, "Ari Shaller");
});
