// Unit tests for the npm launcher's pure helpers. Run with:
//   node --test packages/callimachus-mcp/test/*.test.mjs
import assert from "node:assert/strict";
import { existsSync, mkdirSync, mkdtempSync, rmSync, utimesSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import {
  compareVersions,
  glibcMessage,
  glibcSupported,
  parseChecksums,
  pruneOldVersions,
  shouldPrune,
  unsupportedPlatformMessage,
} from "../lib/download.mjs";

const HASH_A = "a".repeat(64);
const HASH_B = "B".repeat(64);

test("parseChecksums reads sha256sum text and binary mode lines", () => {
  const sums = parseChecksums(
    [
      `${HASH_A}  callimachus-mcp-aarch64-apple-darwin`,
      `${HASH_B} *callimachus-mcp-x86_64-pc-windows-msvc.exe`,
      "",
      "not a checksum line",
    ].join("\r\n"),
  );
  assert.equal(sums.size, 2);
  assert.equal(sums.get("callimachus-mcp-aarch64-apple-darwin"), HASH_A);
  // Digests are normalized to lowercase so they compare equal to Node's hex output.
  assert.equal(sums.get("callimachus-mcp-x86_64-pc-windows-msvc.exe"), "b".repeat(64));
});

test("parseChecksums ignores short or malformed digests", () => {
  const sums = parseChecksums(`${"a".repeat(63)}  short\nzz${"a".repeat(62)}  bad-hex\n`);
  assert.equal(sums.size, 0);
});

test("glibcSupported enforces the 2.38 floor", () => {
  assert.equal(glibcSupported([2, 38]), true);
  assert.equal(glibcSupported([2, 39]), true);
  assert.equal(glibcSupported([3, 0]), true);
  assert.equal(glibcSupported([2, 35]), false); // Ubuntu 22.04
  assert.equal(glibcSupported([2, 36]), false); // Debian 12
  assert.equal(glibcSupported(null), false); // musl (Alpine)
});

test("compareVersions follows semver precedence", () => {
  assert.ok(compareVersions("0.11.0", "0.11.1") < 0);
  assert.ok(compareVersions("0.12.0", "0.11.9") > 0);
  assert.ok(compareVersions("0.10.0", "0.9.0") > 0); // numeric, not lexical
  assert.ok(compareVersions("1.0.0-rc.1", "1.0.0") < 0); // a release outranks its RCs
  assert.ok(compareVersions("1.0.0-rc.2", "1.0.0-rc.10") < 0);
  assert.ok(compareVersions("1.0.0-alpha", "1.0.0-alpha.1") < 0);
  assert.ok(compareVersions("1.0.0-1", "1.0.0-beta") < 0); // numeric before alphanumeric
  assert.equal(compareVersions("1.2.3+build.5", "1.2.3"), 0);
});

test("shouldPrune only removes older versions nobody used for 30 days", () => {
  const DAY = 24 * 60 * 60 * 1000;
  const now = 1_000 * DAY;
  // An older version idle for a month is removed.
  assert.equal(shouldPrune("0.10.0", "0.11.1", now - 31 * DAY, now), true);
  // Another MCP client still runs the older version (its cache hit refreshed the mtime).
  assert.equal(shouldPrune("0.10.0", "0.11.1", now - 2 * DAY, now), false);
  // A newer version belongs to a client on a newer release (e.g. an unpinned `npx -y`).
  assert.equal(shouldPrune("0.12.0", "0.11.1", now - 365 * DAY, now), false);
  // The running version and anything that isn't a version dir are never touched.
  assert.equal(shouldPrune("0.11.1", "0.11.1", 0, now), false);
  assert.equal(shouldPrune("tmp", "0.11.1", 0, now), false);
  // An RC of the running release is older than it.
  assert.equal(shouldPrune("0.11.1-rc.1", "0.11.1", now - 40 * DAY, now), true);
});

test("the glibc and Intel Mac errors don't send people to a source build that can't work", () => {
  assert.doesNotMatch(glibcMessage([2, 35]), /cargo/);
  assert.match(glibcMessage([2, 35]), /glibc 2\.35/);
  assert.match(glibcMessage(null), /musl/);
  assert.doesNotMatch(unsupportedPlatformMessage("darwin-x64"), /cargo/);
  assert.match(unsupportedPlatformMessage("darwin-x64"), /Intel Macs are not supported/);
  // Elsewhere the source-build recipe is offered, including the empty dist dir it needs.
  const other = unsupportedPlatformMessage("linux-arm64");
  assert.match(other, /cargo build --release --locked/);
  assert.match(other, /mkdir dist/);
});

test("pruneOldVersions keeps in-progress downloads and newer versions", (t) => {
  const root = mkdtempSync(join(tmpdir(), "callimachus-mcp-prune-"));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  const DAY = 24 * 60 * 60 * 1000;
  const now = Date.now();
  const old = new Date(now - 60 * DAY);
  const make = (name, files) => {
    const dir = join(root, name);
    mkdirSync(dir);
    for (const [file, mtime] of files) {
      writeFileSync(join(dir, file), "x");
      utimesSync(join(dir, file), mtime, mtime);
    }
    utimesSync(dir, old, old);
    return dir;
  };
  const idle = make("0.9.0", [["callimachus-mcp-x", old]]);
  // Another run is still writing its download here (fresh .tmp, stale dir mtime).
  const downloading = make("0.10.0", [[".callimachus-mcp-x.123.tmp", new Date(now)]]);
  const newer = make("0.12.0", [["callimachus-mcp-x", old]]);
  const notAVersion = make("scratch", [["file", old]]);

  pruneOldVersions(root, "0.11.0", now);

  assert.equal(existsSync(idle), false);
  assert.equal(existsSync(downloading), true);
  assert.equal(existsSync(newer), true);
  assert.equal(existsSync(notAVersion), true);
});
