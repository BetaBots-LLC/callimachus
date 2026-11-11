// Checks on the Codex plugin (plugins/codex) and the repo marketplace that lists it. Run with:
//   node --test plugins/test/*.test.mjs
// The Rust test `integration::tests::codex_plugin_mirrors_the_app_install` pins the hooks to
// the app's CODEX_HOOKS; this one replays Codex's own discovery rules over the repo's files,
// with no Rust toolchain and no Codex needed. The rules are Codex's at tag rust-v0.167.0
// (github.com/openai/codex, 2026-11-06): codex-rs/core-plugins/src/{manifest,marketplace,
// loader}.rs, codex-rs/utils/plugins/src/plugin_namespace.rs,
// codex-rs/exec-server-protocol/src/protocol.rs and codex-rs/protocol/src/hooks.rs.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { existsSync, readFileSync, statSync } from "node:fs";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..", "..");
const plugin = join(root, "plugins", "codex");
const read = (...parts) => readFileSync(join(...parts), "utf8");
const readJson = (...parts) => JSON.parse(read(...parts));
const releaseVersion = readJson(root, "apps", "desktop", "package.json").version;

/** `RawPluginManifest` (manifest.rs, camelCase): the legacy manifest's fields. */
const LEGACY_MANIFEST_FIELDS = [
  "name",
  "version",
  "description",
  "keywords",
  "skills",
  "mcpServers",
  "apps",
  "hooks",
  "interface",
  "extensions",
];
/** Fields every manifest Codex bundles carries (~/.codex/plugins/cache/*), which the legacy
 *  parser ignores. */
const BUNDLED_METADATA_FIELDS = ["author", "homepage", "repository", "license"];
/** `RawPluginManifestInterface` (manifest.rs, camelCase, with the `...URL` aliases). */
const INTERFACE_FIELDS = [
  "displayName",
  "shortDescription",
  "longDescription",
  "developerName",
  "category",
  "capabilities",
  "websiteUrl",
  "websiteURL",
  "privacyPolicyUrl",
  "privacyPolicyURL",
  "termsOfServiceUrl",
  "termsOfServiceURL",
  "defaultPrompt",
  "brandColor",
  "composerIcon",
  "logo",
  "logoDark",
  "screenshots",
];
/** `HookEventsToml` (protocol/src/hooks.rs). */
const CODEX_HOOK_EVENTS = [
  "PreModelRequest",
  "PostModelRequest",
  "PreToolUse",
  "PermissionRequest",
  "PostToolUse",
  "PreCompact",
  "PostCompact",
  "SessionStart",
  "SessionEnd",
  "UserPromptSubmit",
  "SubagentStart",
  "SubagentStop",
  "Stop",
  "Interrupt",
];
/** `MARKETPLACE_MANIFEST_RELATIVE_PATHS` (marketplace.rs): Codex reads the first that exists. */
const MARKETPLACE_PATHS = [
  ".agents/plugins/marketplace.json",
  ".agents/plugins/api_marketplace.json",
  ".claude-plugin/marketplace.json",
  ".cursor-plugin/marketplace.json",
];
/** `DISCOVERABLE_PLUGIN_MANIFEST_PATHS` (protocol.rs), tried after a root `plugin.json` whose
 *  `$schema` is an Agent Plugins one (plugin_namespace.rs). */
const MANIFEST_PATHS = [
  ".codex-plugin/plugin.json",
  ".claude-plugin/plugin.json",
  ".cursor-plugin/plugin.json",
];

/** The `cal` subcommands, parsed from `COMMANDS` in cli_core.rs (the dispatcher's list). */
function calCommands() {
  const src = read(root, "apps", "desktop", "src-tauri", "core", "src", "cli_core.rs");
  const block = src.match(/pub const COMMANDS: &\[&str\] = &\[([\s\S]*?)\];/);
  assert.ok(block, "COMMANDS not found in cli_core.rs");
  return new Set([...block[1].matchAll(/"([^"]+)"/g)].map((m) => m[1]));
}

/** The repo tree as a clone would hold it: tracked files plus new ones not ignored. */
function repoTree() {
  const out = execFileSync(
    "git",
    ["ls-files", "-z", "--cached", "--others", "--exclude-standard"],
    { cwd: root, encoding: "utf8" },
  );
  const deleted = new Set(
    execFileSync("git", ["ls-files", "-z", "--deleted"], { cwd: root, encoding: "utf8" })
      .split("\0")
      .filter(Boolean),
  );
  return new Set(out.split("\0").filter((p) => p && !deleted.has(p)));
}

test("Codex reads the repo's own marketplace file first, so it lists the Codex plugin", () => {
  const tree = repoTree();
  const first = MARKETPLACE_PATHS.find((p) => tree.has(p));
  assert.equal(first, ".agents/plugins/marketplace.json");
  const market = readJson(root, first);
  // RawMarketplaceManifest: name, optional interface.displayName, plugins.
  assert.deepEqual(Object.keys(market).sort(), ["interface", "name", "plugins"]);
  assert.match(market.name, /^[A-Za-z0-9_-]+$/, "a marketplace name is a path segment");
  assert.equal(typeof market.interface.displayName, "string");
  assert.equal(market.plugins.length, 1);
  const [entry] = market.plugins;
  for (const key of Object.keys(entry))
    assert.ok(["name", "source", "policy", "category"].includes(key), `entry field ${key}`);
  assert.equal(entry.name, "callimachus");
  // A git-subdir source (owner/repo shorthand, a path inside the repo, a ref), pinned to the
  // release tag as the Claude Code catalog is: sync-versions.mjs moves the ref.
  assert.deepEqual(entry.source, {
    source: "git-subdir",
    url: "BetaBots-LLC/callimachus",
    path: "plugins/codex",
    ref: `v${releaseVersion}`,
  });
  assert.ok(
    ["AVAILABLE", "NOT_AVAILABLE", "INSTALLED_BY_DEFAULT"].includes(entry.policy.installation),
  );
  assert.ok(["ON_INSTALL", "ON_USE"].includes(entry.policy.authentication));
  assert.ok(tree.has(`${entry.source.path}/.codex-plugin/plugin.json`));
  // Claude Code and Cursor keep reading their own catalogs; only Codex stops seeing the
  // Claude Code one in this repo.
  assert.equal(
    readJson(root, ".claude-plugin", "marketplace.json").plugins[0].source.path,
    "plugins/claude-code",
  );
  assert.deepEqual(
    readJson(root, ".cursor-plugin", "marketplace.json").plugins.map((p) => p.source),
    ["./plugins/cursor"],
  );
});

test("the manifest is the legacy .codex-plugin one, holding only fields Codex's legacy parser reads", () => {
  // A root plugin.json with an Agent Plugins $schema would win, and at rust-v0.167.0 that
  // format loads no hooks at all (loader.rs); no other agent's manifest sits here either.
  for (const p of ["plugin.json", ".claude-plugin", ".cursor-plugin", ".plugin"])
    assert.ok(!existsSync(join(plugin, p)), `plugins/codex/${p}`);
  const found = MANIFEST_PATHS.find((p) => existsSync(join(plugin, p)));
  assert.equal(found, ".codex-plugin/plugin.json");
  const manifest = readJson(plugin, found);
  assert.equal(manifest.$schema, undefined);
  for (const key of Object.keys(manifest))
    assert.ok(
      LEGACY_MANIFEST_FIELDS.includes(key) || BUNDLED_METADATA_FIELDS.includes(key),
      `manifest field ${key}`,
    );
  for (const key of Object.keys(manifest.interface))
    assert.ok(INTERFACE_FIELDS.includes(key), `interface field ${key}`);
  assert.equal(manifest.name, "callimachus");
  assert.equal(manifest.version, releaseVersion);
  assert.equal(manifest.license, "AGPL-3.0-or-later");
  // MAX_DEFAULT_PROMPT_COUNT and MAX_DEFAULT_PROMPT_LEN at the tag.
  assert.ok(manifest.interface.defaultPrompt.length <= 3);
  for (const prompt of manifest.interface.defaultPrompt) assert.ok(prompt.length <= 128, prompt);
  // Every path a manifest names starts with `./` and stays in the plugin (resolve_manifest_path).
  for (const path of [manifest.skills, manifest.interface.logo]) {
    assert.match(path, /^\.\/[^.]/);
    assert.ok(!path.split("/").includes(".."), path);
    assert.ok(existsSync(join(plugin, path)), path);
  }
  // No `hooks` and no `mcpServers` in the manifest: Codex then reads the default files,
  // hooks/hooks.json and .mcp.json (DEFAULT_HOOKS_CONFIG_FILE, DEFAULT_MCP_CONFIG_FILE).
  assert.equal(manifest.hooks, undefined);
  assert.equal(manifest.mcpServers, undefined);
  assert.ok(existsSync(join(plugin, "hooks", "hooks.json")));
  assert.ok(existsSync(join(plugin, ".mcp.json")));
});

test("every hook is a plain sh script that runs a real cal subcommand behind the guards", () => {
  const known = calCommands();
  const file = readJson(plugin, "hooks", "hooks.json");
  // HooksFile is deny_unknown_fields: description and hooks only.
  for (const key of Object.keys(file)) assert.ok(["description", "hooks"].includes(key), key);
  const events = Object.keys(file.hooks).sort();
  assert.deepEqual(events, ["PreCompact", "SessionEnd", "SessionStart"]);
  for (const event of events) assert.ok(CODEX_HOOK_EVENTS.includes(event), event);
  // Codex substitutes ${PLUGIN_ROOT} in the command before its shell runs it.
  const command = /^"\$\{PLUGIN_ROOT\}"\/scripts\/([a-z-]+)\.sh$/;
  const run = /^exec ~\/\.local\/bin\/cal ([a-z-]+) --for codex$/;
  // Only a cal whose Codex hooks claim each event: an older one would inject a second time
  // beside the app's own Codex hook.
  const probe =
    "~/.local/bin/cal help 2>/dev/null | grep -q 'codex or cursor, one run per event' || exit 0";
  const expected = {
    SessionStart: [
      ["startup|clear|compact", "hook", 10],
      // The Claude Code plugin's freshness guard, for a machine without the app.
      ["startup|resume", "index", 5],
    ],
    PreCompact: [[undefined, "snapshot-hook", 10]],
    SessionEnd: [[undefined, "session-end", 3]],
  };
  for (const event of events) {
    const groups = file.hooks[event].map((group) => {
      assert.equal(group.hooks.length, 1, event);
      const [hook] = group.hooks;
      assert.equal(hook.type, "command");
      const m = hook.command.match(command);
      assert.ok(m, `${event}: unexpected command ${hook.command}`);
      const path = join(plugin, "scripts", `${m[1]}.sh`);
      assert.ok(statSync(path).mode & 0o111, `${m[1]}.sh is executable`);
      if (m[1] !== "index") {
        const lines = readFileSync(path, "utf8").trimEnd().split("\n");
        assert.equal(lines[0], "#!/bin/sh", `${m[1]}.sh: a plain sh script`);
        const code = lines.filter((l) => !l.startsWith("#"));
        assert.deepEqual(code.slice(0, 2), ["[ -x ~/.local/bin/cal ] || exit 0", probe], m[1]);
        const last = code.at(-1).match(run);
        assert.ok(last, `${m[1]}.sh: does not end by running cal ... --for codex: ${code.at(-1)}`);
        assert.equal(last[1], m[1], `${m[1]}.sh: the script is named after another command`);
        assert.ok(known.has(last[1]), `\`cal ${last[1]}\` is not in COMMANDS`);
      }
      return [group.matcher, m[1], hook.timeout];
    });
    assert.deepEqual(groups, expected[event], event);
  }
  // The same script as the Claude Code plugin's, whose test checks its probe and command.
  assert.equal(
    read(plugin, "scripts", "index.sh"),
    read(root, "plugins", "claude-code", "scripts", "index.sh"),
  );
  // SessionEnd hooks get at most 3 s in Codex (hooks/src/events/session_end.rs).
  assert.ok(file.hooks.SessionEnd[0].hooks[0].timeout <= 3);
  // The probe matches one line of `cal help`, the one that says a Codex hook acts once.
  const usage = read(root, "apps", "desktop", "src-tauri", "core", "src", "cli_core.rs")
    .split("pub(crate) const USAGE")[1]
    .split('";')[0];
  assert.equal(
    usage.split("\n").filter((l) => /codex or cursor, one run per event/.test(l)).length,
    1,
  );
});

test("the MCP server is the npm launcher pinned to the release, run in the session's folder", () => {
  // No `cwd`: Codex then starts it in the turn's working directory, where the server finds
  // the current project. A relative `cwd` is joined to the plugin root (codex-mcp's
  // plugin_config.rs), which would scope every project tool to the plugin's own folder.
  assert.deepEqual(readJson(plugin, ".mcp.json"), {
    mcpServers: {
      callimachus: { command: "npx", args: ["-y", `callimachus-mcp@${releaseVersion}`] },
    },
  });
  assert.equal(
    readJson(root, "packages", "callimachus-mcp", "package.json").version,
    releaseVersion,
  );
});

test("the skill is the bundled resource, byte for byte", () => {
  assert.equal(
    read(plugin, "skills", "recall", "SKILL.md"),
    read(root, "apps", "desktop", "src-tauri", "resources", "recall", "SKILL.md"),
  );
});
