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

function fail(message) {
  console.error(`sync-versions: ${message}`);
  process.exit(1);
}

const { version } = JSON.parse(readFileSync(pkgPath, "utf8"));
if (!version) fail("no version in apps/desktop/package.json");

// tauri.conf.json: plain JSON, top-level "version" key.
const conf = JSON.parse(readFileSync(confPath, "utf8"));
const confBefore = conf.version;
conf.version = version;
writeFileSync(confPath, `${JSON.stringify(conf, null, 2)}\n`);

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

console.log(`sync-versions: ${version}`);
console.log(`  tauri.conf.json  ${confBefore} -> ${version}`);
console.log(`  Cargo.toml       ${cargoBefore} -> ${version}`);
console.log(`  Cargo.lock       ${lockBefore} -> ${version}`);
console.log(`  server.json      ${serverBefore} -> ${version}`);

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
