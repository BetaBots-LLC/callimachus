#!/usr/bin/env node
// Release tagger. Wired as the Changesets Action `publish` step, so it runs
// right after a "Version Packages" PR merges (no changesets left).
//
// Every shippable package (desktop app, VS Code extension, `callimachus-mcp`
// npm wrapper) is in one Changesets `fixed` group, so they always share a
// version and one tag drives every release workflow:
//
//   v<version>  -> build.yml             (desktop installers, standalone cal +
//                                         callimachus-mcp binaries, updater
//                                         manifest, npm wrapper, MCP registry)
//               -> publish-extension.yml (VS Code Marketplace + Open VSX, and
//                                         the .vsix attached to the same Release)
//
// If the tag already exists it does nothing. Tags are pushed via the checkout
// token: use a PAT as RELEASE_TOKEN so the pushed tag is allowed to trigger the
// release workflows (the default GITHUB_TOKEN cannot trigger other workflows).

import { execSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const versionOf = (pkg) => JSON.parse(readFileSync(join(root, pkg), "utf8")).version;

// The desktop package.json carries the shared version (see the header).
const ARTIFACTS = [{ pkg: "apps/desktop/package.json", prefix: "v" }];

const existing = new Set(
  execSync("git tag --list", { encoding: "utf8" })
    .split("\n")
    .map((t) => t.trim()),
);

let pushed = 0;
for (const { pkg, prefix } of ARTIFACTS) {
  const tag = `${prefix}${versionOf(pkg)}`;
  if (existing.has(tag)) {
    console.log(`release-tag: ${tag} already exists, skipping`);
    continue;
  }
  execSync(`git tag ${tag}`, { stdio: "inherit" });
  execSync(`git push origin ${tag}`, { stdio: "inherit" });
  console.log(`release-tag: pushed ${tag}`);
  pushed++;
}

if (pushed === 0) console.log("release-tag: nothing new to tag");
