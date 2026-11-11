#!/usr/bin/env node
// Single source of truth for the desktop version is apps/desktop/package.json
// (bumped by `changeset version`). This script propagates that version into the
// files Changesets can't touch, so installers, the updater manifest, the MCP
// registry entry and the git tag never drift apart:
//
//   apps/desktop/src-tauri/tauri.conf.json   top-level "version"
//   apps/desktop/src-tauri/Cargo.toml        [package] version
//   apps/desktop/src-tauri/Cargo.lock        the root `callimachus` package entry
//   server.json                              registry manifest + pinned npm version
//   plugins/claude-code/.claude-plugin/plugin.json  Claude Code plugin "version"
//   plugins/claude-code/mcp/package.json     the vendored MCP launcher's "version"
//   .claude-plugin/marketplace.json          plugin source `ref` = the `v<version>` tag
//   plugins/cursor/.cursor-plugin/plugin.json  Cursor plugin "version"
//   plugins/cursor/mcp.json                  the `npx -y callimachus-mcp@<version>` pin
//   .cursor-plugin/marketplace.json          "version" and "metadata.version"
//   plugins/codex/.codex-plugin/plugin.json  Codex plugin "version"
//   plugins/codex/.mcp.json                  the `npx -y callimachus-mcp@<version>` pin
//   .agents/plugins/marketplace.json         Codex plugin source `ref` = the `v<version>` tag
//
// Run automatically by the `version-packages` script. Safe to run by hand, and
// idempotent: running it twice with the same version changes nothing.

import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const desktop = join(root, "apps", "desktop");

const pkgPath = join(desktop, "package.json");
const confPath = join(desktop, "src-tauri", "tauri.conf.json");
const cargoPath = join(desktop, "src-tauri", "Cargo.toml");
const lockPath = join(desktop, "src-tauri", "Cargo.lock");
const serverPath = join(root, "server.json");
const pluginPath = join(root, "plugins", "claude-code", ".claude-plugin", "plugin.json");
const pluginLauncherPath = join(root, "plugins", "claude-code", "mcp", "package.json");
const marketPath = join(root, ".claude-plugin", "marketplace.json");
const cursorPluginPath = join(root, "plugins", "cursor", ".cursor-plugin", "plugin.json");
const cursorMcpPath = join(root, "plugins", "cursor", "mcp.json");
const cursorMarketPath = join(root, ".cursor-plugin", "marketplace.json");
const codexPluginPath = join(root, "plugins", "codex", ".codex-plugin", "plugin.json");
const codexMcpPath = join(root, "plugins", "codex", ".mcp.json");
const codexMarketPath = join(root, ".agents", "plugins", "marketplace.json");

function fail(message) {
  console.error(`sync-versions: ${message}`);
  process.exit(1);
}

const { version } = JSON.parse(readFileSync(pkgPath, "utf8"));
if (!version) fail("no version in apps/desktop/package.json");

// tauri.conf.json: swap only the top-level "version" value in place.
// Re-serializing with JSON.stringify expands the one-line bundle.linux
// `depends` arrays, which Biome then reports as a format error.
const confText = readFileSync(confPath, "utf8");
const confBefore = JSON.parse(confText).version;
const topLevelVersion = /^ {2}"version": "[^"]*"/m;
if (!topLevelVersion.test(confText)) fail('no top-level "version" in tauri.conf.json');
writeFileSync(confPath, confText.replace(topLevelVersion, `  "version": "${version}"`));

// Cargo.toml: replace ONLY the `version` inside the [package] table, never a
// dependency's version. Scope the replace to the [package] block.
const cargo = readFileSync(cargoPath, "utf8");
const pkgHeader = cargo.indexOf("[package]");
if (pkgHeader === -1) fail("no [package] table in Cargo.toml");
const nextHeader = cargo.indexOf("\n[", pkgHeader + 1);
const sectionEnd = nextHeader === -1 ? cargo.length : nextHeader;
const section = cargo.slice(pkgHeader, sectionEnd);

let cargoBefore;
const newSection = section.replace(/^version\s*=\s*"[^"]*"/m, (line) => {
  cargoBefore = line.match(/"([^"]*)"/)[1];
  return `version = "${version}"`;
});
if (cargoBefore === undefined) fail('no `version = "..."` line in [package]');
writeFileSync(cargoPath, cargo.slice(0, pkgHeader) + newSection + cargo.slice(sectionEnd));

// Cargo.lock: the root crate records its own version too. Cargo rewrites it on
// the next build, which used to leave the committed lock several releases
// behind and dirty the tree after every local build (and made `--locked`
// builds impossible). Edit the entry directly instead of shelling out to
// `cargo update -p callimachus`, so this script needs no Rust toolchain and
// never touches any dependency's resolved version.
const lockBefore = syncCargoLock(lockPath, "callimachus", version);

// server.json: the official MCP registry manifest. Changesets bumps the
// `callimachus-mcp` package.json (it's in the `fixed` group), but server.json is
// not a package, so sync its top-level `version` and each pinned package version
// here. They must equal the release version: the npm wrapper downloads the
// `v<version>` GitHub Release binaries, and the registry entry points at it.
const server = JSON.parse(readFileSync(serverPath, "utf8"));
const serverBefore = server.version;
server.version = version;
for (const pkg of server.packages ?? []) pkg.version = version;
writeFileSync(serverPath, `${JSON.stringify(server, null, 2)}\n`);

// plugin.json: Claude Code pins a plugin with a `version` to it until the field changes, so a
// stale value would keep plugin users on the previous release's hooks and skill. Swap the
// top-level value in place, like tauri.conf.json, so the file keeps its formatting.
const pluginText = readFileSync(pluginPath, "utf8");
const pluginBefore = JSON.parse(pluginText).version;
if (!topLevelVersion.test(pluginText)) fail('no top-level "version" in plugin.json');
writeFileSync(pluginPath, pluginText.replace(topLevelVersion, `  "version": "${version}"`));

// The vendored launcher and marketplace.json: the plugin must match the app release the user
// installed, not the tip of main. A plugin from main runs `cal` subcommands an older app does
// not have yet, and a launcher from a newer release would download a server ahead of the
// index's schema. So the marketplace fetches the plugin from the release tag, and the launcher
// vendored into the plugin (plugins/claude-code/mcp, a copy of packages/callimachus-mcp) reads
// the release it downloads from its own package.json "version". Both swaps are in place,
// keeping the files' formatting.
const launcherText = readFileSync(pluginLauncherPath, "utf8");
const launcherBefore = JSON.parse(launcherText).version;
if (!topLevelVersion.test(launcherText))
  fail('no top-level "version" in plugins/claude-code/mcp/package.json');
writeFileSync(
  pluginLauncherPath,
  launcherText.replace(topLevelVersion, `  "version": "${version}"`),
);

const marketText = readFileSync(marketPath, "utf8");
const tagRef = /"ref": "v[^"]*"/;
const marketBefore = marketText.match(tagRef)?.[0].slice(9, -1);
if (marketBefore === undefined) fail('no "ref": "v<version>" in .claude-plugin/marketplace.json');
writeFileSync(marketPath, marketText.replace(tagRef, `"ref": "v${version}"`));

// The Cursor plugin: its manifest and the root marketplace (whose top-level "version" is
// what cursor.directory lists, since its parser reads only the root manifest) carry the
// release, and its MCP server is the npm launcher pinned to it, so the server matches the
// `cal` the plugin's hooks run. All in place, keeping the files' formatting.
const cursorPluginText = readFileSync(cursorPluginPath, "utf8");
const cursorPluginBefore = JSON.parse(cursorPluginText).version;
if (!topLevelVersion.test(cursorPluginText))
  fail('no top-level "version" in plugins/cursor/.cursor-plugin/plugin.json');
writeFileSync(
  cursorPluginPath,
  cursorPluginText.replace(topLevelVersion, `  "version": "${version}"`),
);

const cursorMcpText = readFileSync(cursorMcpPath, "utf8");
const npxPin = /"callimachus-mcp@[^"]*"/;
const cursorMcpBefore = cursorMcpText.match(npxPin)?.[0].slice(17, -1);
if (cursorMcpBefore === undefined)
  fail('no "callimachus-mcp@<version>" in plugins/cursor/mcp.json');
writeFileSync(cursorMcpPath, cursorMcpText.replace(npxPin, `"callimachus-mcp@${version}"`));

const cursorMarketText = readFileSync(cursorMarketPath, "utf8");
const anyVersion = /"version": "[^"]*"/g;
const cursorMarketBefore = JSON.parse(cursorMarketText).version;
if ((cursorMarketText.match(anyVersion) ?? []).length !== 2)
  fail('expected "version" and "metadata.version" in .cursor-plugin/marketplace.json');
writeFileSync(cursorMarketPath, cursorMarketText.replace(anyVersion, `"version": "${version}"`));

// The Codex plugin, for the same reasons as the Claude Code one: Codex caches it under its
// version, its marketplace fetches it from the release tag, and its MCP server is the npm
// launcher pinned to that release. In place, keeping the files' formatting.
const codexPluginText = readFileSync(codexPluginPath, "utf8");
const codexPluginBefore = JSON.parse(codexPluginText).version;
if (!topLevelVersion.test(codexPluginText))
  fail('no top-level "version" in plugins/codex/.codex-plugin/plugin.json');
writeFileSync(
  codexPluginPath,
  codexPluginText.replace(topLevelVersion, `  "version": "${version}"`),
);

const codexMcpText = readFileSync(codexMcpPath, "utf8");
const codexMcpBefore = codexMcpText.match(npxPin)?.[0].slice(17, -1);
if (codexMcpBefore === undefined) fail('no "callimachus-mcp@<version>" in plugins/codex/.mcp.json');
writeFileSync(codexMcpPath, codexMcpText.replace(npxPin, `"callimachus-mcp@${version}"`));

const codexMarketText = readFileSync(codexMarketPath, "utf8");
const codexMarketBefore = codexMarketText.match(tagRef)?.[0].slice(9, -1);
if (codexMarketBefore === undefined)
  fail('no "ref": "v<version>" in .agents/plugins/marketplace.json');
writeFileSync(codexMarketPath, codexMarketText.replace(tagRef, `"ref": "v${version}"`));

console.log(`sync-versions: ${version}`);
console.log(`  tauri.conf.json  ${confBefore} -> ${version}`);
console.log(`  Cargo.toml       ${cargoBefore} -> ${version}`);
console.log(`  Cargo.lock       ${lockBefore} -> ${version}`);
console.log(`  server.json      ${serverBefore} -> ${version}`);
console.log(`  plugin.json      ${pluginBefore} -> ${version}`);
console.log(`  mcp/package.json ${launcherBefore} -> ${version}`);
console.log(`  marketplace.json v${marketBefore} -> v${version}`);
console.log(`  cursor plugin.json ${cursorPluginBefore} -> ${version}`);
console.log(`  cursor mcp.json  ${cursorMcpBefore} -> ${version}`);
console.log(`  cursor marketplace.json ${cursorMarketBefore} -> ${version}`);
console.log(`  codex plugin.json ${codexPluginBefore} -> ${version}`);
console.log(`  codex .mcp.json  ${codexMcpBefore} -> ${version}`);
console.log(`  codex marketplace.json v${codexMarketBefore} -> v${version}`);

/**
 * Sets the version of the local (path) package `name` in a Cargo.lock and
 * returns the previous version. Registry/git dependencies that happen to share
 * the name are left alone: only the entry without a `source = ...` line is the
 * workspace crate.
 */
function syncCargoLock(path, name, nextVersion) {
  const lock = readFileSync(path, "utf8");
  const marker = "[[package]]\n";
  const blocks = lock.split(marker);
  let before;
  for (let i = 1; i < blocks.length; i++) {
    const block = blocks[i];
    const isTarget = block.startsWith(`name = "${name}"\n`) && !/^source = /m.test(block);
    if (!isTarget) continue;
    blocks[i] = block.replace(/^version = "([^"]*)"$/m, (_line, current) => {
      before = current;
      return `version = "${nextVersion}"`;
    });
    break;
  }
  if (before === undefined) fail(`no local "${name}" package entry in Cargo.lock`);
  writeFileSync(path, blocks.join(marker));
  return before;
}
