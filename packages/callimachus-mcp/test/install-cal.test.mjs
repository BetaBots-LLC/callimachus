// Tests for the launcher's `cal` download and `--install-cal` link, with a fake
// fetch and a throwaway HOME and cache. Run with:
//   node --test packages/callimachus-mcp/test/*.test.mjs
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import {
  existsSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  readlinkSync,
  rmSync,
  statSync,
  symlinkSync,
  utimesSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import {
  assetName,
  cacheLinkVersion,
  ensureBinary,
  linkedVersions,
  pruneOldVersions,
  removeStaleDownloads,
  version,
} from "../lib/download.mjs";
import {
  calLinkPath,
  followLinkedCal,
  inspectCalSlot,
  installCal,
  nextSteps,
} from "../lib/install-cal.mjs";

const BASE = "https://example.invalid/releases/download/v0";
const BODY = Buffer.from("#!/bin/sh\necho cal\n");
const sha = (buf) => createHash("sha256").update(buf).digest("hex");
const supported = assetName("cal") !== null;
const unix = process.platform !== "win32";

/** A fetch that serves `files` (name -> Buffer) under BASE and records each URL. */
function fakeFetch(files) {
  const calls = [];
  const impl = async (url) => {
    calls.push(url);
    const name = url.slice(BASE.length + 1);
    const body = files[name];
    if (!body) return new Response("missing", { status: 404, statusText: "Not Found" });
    return new Response(body, { headers: { "content-length": String(body.length) } });
  };
  return { impl, calls };
}

function scratch(t) {
  // A neutral prefix: one containing "callimachus" would put that word in every path
  // below, and the app-link checks would pass even with the detection broken.
  const dir = mkdtempSync(join(tmpdir(), "launcher-cal-test-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const home = join(dir, "home");
  const cacheRoot = join(dir, "cache");
  mkdirSync(home);
  return { dir, home, cacheRoot };
}

test("assetName names the cal asset next to the MCP server's", () => {
  assert.equal(assetName("cal", "darwin", "arm64"), "cal-aarch64-apple-darwin");
  assert.equal(assetName("cal", "win32", "x64"), "cal-x86_64-pc-windows-msvc.exe");
  assert.equal(
    assetName("callimachus-mcp", "linux", "x64"),
    "callimachus-mcp-x86_64-unknown-linux-gnu",
  );
  assert.equal(assetName("cal", "darwin", "x64"), null);
  assert.throws(() => assetName("rm", "darwin", "arm64"), /unknown release binary/);
});

test("cal downloads from the release, is checked against SHA256SUMS and cached", {
  skip: !supported,
}, async (t) => {
  const { cacheRoot } = scratch(t);
  const name = assetName("cal");
  const { impl, calls } = fakeFetch({ [name]: BODY });
  const opts = { fetch: impl, releaseBase: BASE, cacheRoot, sums: new Map([[name, sha(BODY)]]) };

  const bin = await ensureBinary("cal", opts);
  assert.equal(bin, join(cacheRoot, version, name));
  assert.deepEqual(calls, [`${BASE}/${name}`]);
  assert.deepEqual(readFileSync(bin), BODY);
  if (unix) assert.equal(statSync(bin).mode & 0o111, 0o111, "executable");

  // Cached: no second download.
  assert.equal(await ensureBinary("cal", opts), bin);
  assert.equal(calls.length, 1);
});

test("a cal download that fails the checksum is discarded", { skip: !supported }, async (t) => {
  const { cacheRoot } = scratch(t);
  const name = assetName("cal");
  const { impl } = fakeFetch({ [name]: BODY });
  await assert.rejects(
    ensureBinary("cal", {
      fetch: impl,
      releaseBase: BASE,
      cacheRoot,
      sums: new Map([[name, "0".repeat(64)]]),
    }),
    /checksum mismatch for cal-/,
  );
  assert.equal(existsSync(join(cacheRoot, version, name)), false);
  // No manifest entry: refused before anything is fetched, with the cal build recipe.
  const { impl: never, calls } = fakeFetch({});
  await assert.rejects(
    ensureBinary("cal", { fetch: never, releaseBase: BASE, cacheRoot, sums: new Map() }),
    /no SHA-256 checksum for cal-[\s\S]*--bin cal/,
  );
  assert.equal(calls.length, 0);
});

test("CALLIMACHUS_MCP_BIN overrides only the MCP server, never cal", {
  skip: !supported,
}, async (t) => {
  const { cacheRoot } = scratch(t);
  const name = assetName("cal");
  const { impl } = fakeFetch({ [name]: BODY });
  const before = process.env.CALLIMACHUS_MCP_BIN;
  process.env.CALLIMACHUS_MCP_BIN = "/does/not/exist";
  t.after(() => {
    if (before === undefined) delete process.env.CALLIMACHUS_MCP_BIN;
    else process.env.CALLIMACHUS_MCP_BIN = before;
  });
  const sums = new Map([[name, sha(BODY)]]);
  const bin = await ensureBinary("cal", { fetch: impl, releaseBase: BASE, cacheRoot, sums });
  assert.equal(bin, join(cacheRoot, version, name));
  await assert.rejects(ensureBinary(), /CALLIMACHUS_MCP_BIN is set/);
});

/** A fake cached `cal` for `ver`, as ensureBinary would leave it. */
function cachedCal(cacheRoot, ver) {
  const dir = join(cacheRoot, ver);
  mkdirSync(dir, { recursive: true });
  const bin = join(dir, "cal-test");
  writeFileSync(bin, BODY, { mode: 0o755 });
  return bin;
}

test("--install-cal links ~/.local/bin/cal and replaces only its own links", {
  skip: !unix,
}, async (t) => {
  const { dir, home, cacheRoot } = scratch(t);
  const link = calLinkPath(home);
  const current = cachedCal(cacheRoot, version);
  let downloads = 0;
  const ensure = async () => {
    downloads++;
    return current;
  };
  const opts = { home, cacheRoot, ensure };

  // Missing: linked, ~/.local/bin created.
  let out = await installCal(opts);
  assert.equal(out.changed, true);
  assert.equal(readlinkSync(link), current);
  // Already ours on this version: nothing to do.
  out = await installCal(opts);
  assert.equal(out.changed, false);
  assert.match(out.message, /already runs cal v/);

  // An older link of ours moves to this release.
  rmSync(link);
  symlinkSync(cachedCal(cacheRoot, "0.0.1"), link);
  out = await installCal(opts);
  assert.equal(out.changed, true);
  assert.equal(readlinkSync(link), current);

  // A live link to the Callimachus app is the app's: left alone, nothing downloaded.
  const app = join(dir, "Callimachus.app", "callimachus");
  mkdirSync(join(dir, "Callimachus.app"));
  writeFileSync(app, "app");
  rmSync(link);
  symlinkSync(app, link);
  downloads = 0;
  out = await installCal(opts);
  assert.equal(out.changed, false);
  assert.match(out.message, /already runs a Callimachus build/);
  assert.equal(downloads, 0);
  assert.equal(readlinkSync(link), app);
  // A dangling app link (the app was deleted) is replaced.
  rmSync(app);
  out = await installCal(opts);
  assert.equal(out.changed, true);
  assert.equal(readlinkSync(link), current);

  // Another program's cal (the calendar, a script) is never replaced.
  rmSync(link);
  writeFileSync(link, "#!/bin/sh\necho calendar\n");
  await assert.rejects(installCal(opts), /belongs to another program \(it's a regular file\)/);
  assert.equal(lstatSync(link).isSymbolicLink(), false);
  assert.match(readFileSync(link, "utf8"), /calendar/);
  rmSync(link);
  symlinkSync("/usr/bin/cal", link);
  downloads = 0;
  await assert.rejects(installCal(opts), /links to \/usr\/bin\/cal/);
  assert.equal(readlinkSync(link), "/usr/bin/cal");
  assert.equal(downloads, 0, "a foreign slot costs no download");
});

test("--install-cal sends Windows users to the app instead", async (t) => {
  const { home, cacheRoot } = scratch(t);
  await assert.rejects(
    installCal({ home, cacheRoot, platform: "win32", ensure: async () => "x" }),
    /Install the cal CLI in Settings > Agents/,
  );
  assert.equal(existsSync(calLinkPath(home)), false);
});

test("an MCP start moves an older linked cal to its release, never a newer or foreign one", {
  skip: !unix,
}, async (t) => {
  const { dir, home, cacheRoot } = scratch(t);
  const link = calLinkPath(home);
  const current = cachedCal(cacheRoot, version);
  const ensure = async () => current;
  const opts = { home, cacheRoot, ensure };
  mkdirSync(join(home, ".local", "bin"), { recursive: true });

  assert.equal(await followLinkedCal(opts), null, "no link: nothing to follow");

  symlinkSync(cachedCal(cacheRoot, "0.0.1"), link);
  assert.equal(await followLinkedCal(opts), current);
  assert.equal(readlinkSync(link), current);
  assert.equal(await followLinkedCal(opts), null, "already on this release");

  // A newer release's link (another client is unpinned) is not downgraded.
  rmSync(link);
  const newer = cachedCal(cacheRoot, "999.0.0");
  symlinkSync(newer, link);
  assert.equal(await followLinkedCal(opts), null);
  assert.equal(readlinkSync(link), newer);

  // This release's link whose file is gone is restored.
  rmSync(link);
  symlinkSync(join(cacheRoot, version, "cal-gone"), link);
  assert.equal(await followLinkedCal(opts), current);

  // The app's link and another program's are not ours to move.
  rmSync(link);
  symlinkSync(join(dir, "Callimachus.app", "callimachus"), link);
  assert.equal(await followLinkedCal(opts), null);
  rmSync(link);
  symlinkSync("/usr/bin/cal", link);
  assert.equal(await followLinkedCal(opts), null);
});

test("inspectCalSlot reads the version of a link into the cache", { skip: !unix }, (t) => {
  const { home, cacheRoot } = scratch(t);
  const link = calLinkPath(home);
  mkdirSync(join(home, ".local", "bin"), { recursive: true });
  assert.deepEqual(inspectCalSlot(link, cacheRoot), { kind: "missing" });
  const bin = cachedCal(cacheRoot, "0.11.4");
  symlinkSync(bin, link);
  assert.deepEqual(inspectCalSlot(link, cacheRoot), {
    kind: "ours",
    target: bin,
    version: "0.11.4",
  });
});

test("a link into the cache is ours however it is spelled", { skip: !unix }, (t) => {
  const { dir, home, cacheRoot } = scratch(t);
  const link = calLinkPath(home);
  mkdirSync(join(home, ".local", "bin"), { recursive: true });
  const bin = cachedCal(cacheRoot, "0.11.4");

  // Relative: resolved against the link's directory, for the follow and for pruning alike.
  symlinkSync(join("..", "..", "..", "cache", "0.11.4", "cal-test"), link);
  assert.equal(inspectCalSlot(link, cacheRoot).kind, "ours");
  assert.deepEqual(linkedVersions(cacheRoot, home), ["0.11.4"]);

  // Through a symlinked cache root (~/.cache pointing at another disk).
  const alias = join(dir, "cache-alias");
  symlinkSync(cacheRoot, alias);
  rmSync(link);
  symlinkSync(bin, link);
  assert.equal(inspectCalSlot(link, alias).version, "0.11.4");
  assert.deepEqual(linkedVersions(alias, home), ["0.11.4"]);

  // Another XDG_CACHE_HOME's launcher cache: ours to follow, not this root's to keep.
  const other = join(dir, "xdg", "callimachus-mcp", "0.11.2", "cal-aarch64-apple-darwin");
  assert.deepEqual(cacheLinkVersion(other, cacheRoot), { version: "0.11.2", inRoot: false });
  rmSync(link);
  symlinkSync(other, link);
  assert.equal(inspectCalSlot(link, cacheRoot).kind, "ours");
  assert.deepEqual(linkedVersions(cacheRoot, home), []);

  // A source build is not the launcher's: it is left alone like the app's link.
  rmSync(link);
  const built = join(dir, "callimachus", "src-tauri", "target", "release", "cal");
  symlinkSync(built, link);
  assert.equal(inspectCalSlot(link, cacheRoot).kind, "app");
  assert.equal(cacheLinkVersion(built, cacheRoot), null);
});

test("a download left behind by a killed launcher is removed after an hour", (t) => {
  const { cacheRoot } = scratch(t);
  const dir = join(cacheRoot, version);
  mkdirSync(dir, { recursive: true });
  const stale = join(dir, ".cal-test.123.tmp");
  const fresh = join(dir, ".cal-test.456.tmp");
  const bin = join(dir, "cal-test");
  for (const f of [stale, fresh, bin]) writeFileSync(f, "x");
  const now = Date.now();
  const twoHoursAgo = new Date(now - 2 * 60 * 60 * 1000);
  utimesSync(stale, twoHoursAgo, twoHoursAgo);
  utimesSync(bin, twoHoursAgo, twoHoursAgo);
  removeStaleDownloads(dir, now);
  assert.equal(existsSync(stale), false);
  assert.equal(existsSync(fresh), true, "a download in progress is kept");
  assert.equal(existsSync(bin), true, "only temp files go");
});

test("an aborted cal download rejects and leaves no temp file", {
  skip: !supported,
}, async (t) => {
  const { cacheRoot } = scratch(t);
  const name = assetName("cal");
  const abort = new AbortController();
  // Sends one chunk, then stalls until the abort, like a slow download at exit.
  const stalled = async (_url, init) =>
    new Response(
      new ReadableStream({
        start(controller) {
          controller.enqueue(new Uint8Array(BODY));
          setTimeout(() => abort.abort(), 20);
          init.signal.addEventListener("abort", () => controller.error(init.signal.reason));
        },
      }),
      { headers: { "content-length": String(BODY.length * 4) } },
    );
  await assert.rejects(
    ensureBinary("cal", {
      fetch: stalled,
      releaseBase: BASE,
      cacheRoot,
      sums: new Map([[name, sha(BODY)]]),
      signal: abort.signal,
    }),
  );
  const dir = join(cacheRoot, version);
  assert.deepEqual(
    readdirSync(dir).filter((f) => f.endsWith(".tmp")),
    [],
    "the partial download is gone",
  );
  assert.equal(existsSync(join(dir, name)), false);
});

test("pruning keeps the version directory a linked cal runs from", (t) => {
  const { cacheRoot } = scratch(t);
  const DAY = 24 * 60 * 60 * 1000;
  const now = Date.now() + 365 * DAY; // everything below is idle for a year by then
  cachedCal(cacheRoot, "0.9.0");
  cachedCal(cacheRoot, "0.10.0");
  pruneOldVersions(cacheRoot, "0.11.0", now, ["0.10.0"]);
  assert.equal(existsSync(join(cacheRoot, "0.9.0")), false);
  assert.equal(existsSync(join(cacheRoot, "0.10.0")), true);
});

test("the next steps say how to get ~/.local/bin on PATH only when it is missing", () => {
  const link = "/home/me/.local/bin/cal";
  const off = nextSteps(link, { PATH: "/usr/bin:/bin" });
  assert.match(off, /is not on your PATH/);
  assert.match(off, /cal index/);
  assert.match(off, /cal watch/);
  const on = nextSteps(link, { PATH: "/home/me/.local/bin:/usr/bin" });
  assert.doesNotMatch(on, /not on your PATH/);
});
