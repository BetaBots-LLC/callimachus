#!/usr/bin/env node
// Multi-artifact release tagger. Wired as the Changesets Action `publish` step,
// so it runs right after a "Version Packages" PR merges (no changesets left).
//
// For each publishable package whose new version isn't tagged yet, it creates +
// pushes that tag — which triggers the matching workflow:
//
//   v<version>         -> build.yml             (desktop: .dmg + signed updater)
//   vscode-v<version>  -> publish-extension.yml (VS Code Marketplace + Open VSX + .vsix)
//
// Independent versions: a release that only bumps the extension pushes only the
// vscode tag (and vice versa). Tags are pushed via the checkout token (use a PAT
// as RELEASE_TOKEN so the pushed tag is allowed to trigger the artifact workflows).

import { execSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const versionOf = (pkg) =>
  JSON.parse(readFileSync(join(root, pkg), "utf8")).version;

// package.json -> git tag prefix
const ARTIFACTS = [
  { pkg: "apps/desktop/package.json", prefix: "v" },
  { pkg: "apps/vscode/package.json", prefix: "vscode-v" },
];

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
