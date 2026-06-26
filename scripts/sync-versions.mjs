#!/usr/bin/env node
// Single source of truth for the desktop version is apps/desktop/package.json
// (bumped by `changeset version`). This script propagates that version into the
// two Rust/Tauri files that Changesets can't touch, so installers, the updater
// manifest, and the git tag never drift apart.
//
// Run automatically by the `version-packages` script. Safe to run by hand.

import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const desktop = join(root, "apps", "desktop");

const pkgPath = join(desktop, "package.json");
const confPath = join(desktop, "src-tauri", "tauri.conf.json");
const cargoPath = join(desktop, "src-tauri", "Cargo.toml");

const { version } = JSON.parse(readFileSync(pkgPath, "utf8"));
if (!version) {
  console.error("sync-versions: no version in apps/desktop/package.json");
  process.exit(1);
}

// tauri.conf.json — plain JSON, top-level "version" key.
const conf = JSON.parse(readFileSync(confPath, "utf8"));
const confBefore = conf.version;
conf.version = version;
writeFileSync(confPath, JSON.stringify(conf, null, 2) + "\n");

// Cargo.toml — replace ONLY the `version` inside the [package] table, never a
// dependency's version. Scope the replace to the first table block.
const cargo = readFileSync(cargoPath, "utf8");
const pkgHeader = cargo.indexOf("[package]");
if (pkgHeader === -1) {
  console.error("sync-versions: no [package] table in Cargo.toml");
  process.exit(1);
}
const nextHeader = cargo.indexOf("\n[", pkgHeader + 1);
const sectionEnd = nextHeader === -1 ? cargo.length : nextHeader;
const head = cargo.slice(0, pkgHeader);
const section = cargo.slice(pkgHeader, sectionEnd);
const tail = cargo.slice(sectionEnd);

let cargoBefore;
const newSection = section.replace(/^version\s*=\s*"[^"]*"/m, (m) => {
  cargoBefore = m.match(/"([^"]*)"/)[1];
  return `version = "${version}"`;
});
if (cargoBefore === undefined) {
  console.error('sync-versions: no `version = "..."` line in [package]');
  process.exit(1);
}
writeFileSync(cargoPath, head + newSection + tail);

// server.json — the official MCP registry manifest. Changesets bumps the
// `callimachus-mcp` package.json (it's in the `fixed` group), but server.json is
// not a package, so sync its top-level `version` and each pinned package version
// here. They must equal the release version: the npm wrapper downloads the
// `v<version>` GitHub Release binaries, and the registry entry points at it.
const serverPath = join(root, "server.json");
const server = JSON.parse(readFileSync(serverPath, "utf8"));
const serverBefore = server.version;
server.version = version;
for (const pkg of server.packages ?? []) pkg.version = version;
writeFileSync(serverPath, JSON.stringify(server, null, 2) + "\n");

console.log(`sync-versions: ${version}`);
console.log(`  tauri.conf.json  ${confBefore} -> ${version}`);
console.log(`  Cargo.toml       ${cargoBefore} -> ${version}`);
console.log(`  server.json      ${serverBefore} -> ${version}`);
