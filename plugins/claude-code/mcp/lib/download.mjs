// Resolves (and lazily downloads + caches) the prebuilt `callimachus-mcp`
// binary, or the `cal` CLI for `--install-cal`, that matches this npm package's
// version and the host platform.
//
// The npm package is a thin wrapper: it carries no binary itself, but its
// version is kept in lockstep with the desktop release (Changesets `fixed`
// group), so `v<version>` always names a GitHub Release that has the per-target
// `callimachus-mcp-<triple>` and `cal-<triple>` assets this downloads.
//
// Every download is checked against a SHA-256 manifest before it is made
// executable. The release workflow writes the manifest (`SHA256SUMS`) into
// `lib/` right before `npm publish`, so the expected hashes come from the npm
// tarball rather than from the same GitHub Release the binary is fetched from.
//
// IMPORTANT: nothing here may write to stdout. stdout is the MCP stdio
// transport; all diagnostics go to stderr.

import { createHash } from "node:crypto";
import {
  chmodSync,
  createWriteStream,
  existsSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  readlinkSync,
  realpathSync,
  renameSync,
  rmSync,
  statSync,
  unlinkSync,
  utimesSync,
} from "node:fs";
import { createRequire } from "node:module";
import { homedir } from "node:os";
import { basename, dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { Readable, Transform } from "node:stream";
import { pipeline } from "node:stream/promises";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const { version } = require("../package.json");

const REPO = "BetaBots-LLC/callimachus";
export const RELEASE_BASE = `https://github.com/${REPO}/releases/download/v${version}`;
export { version };

/** The release binaries this launcher fetches: the MCP server and the `cal` CLI. */
const TOOLS = ["callimachus-mcp", "cal"];
const BUNDLED_SUMS = fileURLToPath(new URL("./SHA256SUMS", import.meta.url));

// Host platform -> Rust target triple uploaded by .github/workflows/build.yml.
// Only these three targets are released today (see build.yml's matrix).
const TRIPLES = {
  "darwin-arm64": "aarch64-apple-darwin",
  "linux-x64": "x86_64-unknown-linux-gnu",
  "win32-x64": "x86_64-pc-windows-msvc",
};

// The Linux binary links the ONNX Runtime prebuilt, which needs glibc 2.38+
// (Ubuntu 24.04, Debian 13, Fedora 39 or newer). Older distros fail at load
// time with "GLIBC_2.38 not found", so check up front and say why.
const MIN_GLIBC = [2, 38];

// How to build a binary yourself, from the tag matching this launcher. It
// mirrors the repo's Dockerfile: both binaries live in the Tauri-free
// `callimachus-standalone` crate, so Linux needs only the OpenSSL and D-Bus dev
// packages (no GTK or WebKit, and no desktop frontend build). A source build
// links the same ONNX Runtime prebuilt as the release binaries, so it can't get
// past that prebuilt's platform limits (Intel macOS, glibc < 2.38): those errors
// say so instead of pointing here.
function buildFromSource(tool) {
  const use =
    tool === "cal"
      ? `then link the built apps/desktop/src-tauri/target/release/cal at ~/.local/bin/cal.`
      : `then set CALLIMACHUS_MCP_BIN to the built ` +
        `apps/desktop/src-tauri/target/release/callimachus-mcp. ` +
        `Or point CALLIMACHUS_MCP_BIN at a binary you already have.`;
  return (
    `To build it yourself you need Rust and, on Linux, the pkg-config, OpenSSL ` +
    `and D-Bus development packages (on Debian/Ubuntu: pkg-config libssl-dev ` +
    `libdbus-1-dev):\n` +
    `  git clone --depth 1 --branch v${version} https://github.com/${REPO}\n` +
    `  cd callimachus\n` +
    `  cargo build --release --locked --manifest-path apps/desktop/src-tauri/Cargo.toml -p callimachus-standalone --bin ${tool}\n` +
    use
  );
}

// Cached versions other than the running one are removed only when they are
// older and nothing has used them for this long, so clients pinned to different
// versions (a registry entry pinned to x.y.z next to a bare `npx -y`) don't keep
// deleting each other's ~45MB binary.
const PRUNE_UNUSED_MS = 30 * 24 * 60 * 60 * 1000;

/** Where downloaded binaries are cached, one directory per version. */
export function defaultCacheRoot() {
  return join(process.env.XDG_CACHE_HOME || join(homedir(), ".cache"), "callimachus-mcp");
}

/**
 * The release asset name of `tool` for this host (`cal-aarch64-apple-darwin`,
 * `callimachus-mcp-x86_64-pc-windows-msvc.exe`), or null on a platform with no
 * released binary.
 */
export function assetName(tool, platform = process.platform, arch = process.arch) {
  if (!TOOLS.includes(tool)) throw new Error(`unknown release binary ${tool}`);
  const triple = TRIPLES[`${platform}-${arch}`];
  if (!triple) return null;
  return `${tool}-${triple}${platform === "win32" ? ".exe" : ""}`;
}

/**
 * Returns an absolute path to a ready-to-exec release binary (`callimachus-mcp`
 * by default, or `cal`), downloading it on first run. Throws with an actionable
 * message on an unsupported platform, a failed download, or a checksum mismatch.
 *
 * `opts.signal` (an AbortSignal) stops a download in progress; its temp file is
 * removed and the call rejects. `fetch`, `releaseBase`, `cacheRoot` and `sums`
 * (a name -> hex Map standing in for the manifest) exist for tests: they
 * replace the network, the cache location and the bundled SHA256SUMS.
 */
export async function ensureBinary(tool = "callimachus-mcp", opts = {}) {
  // Escape hatch: point at an already-installed binary (e.g. one built with
  // `cargo build`, or the standalone binary from the Releases page).
  const override = tool === "callimachus-mcp" ? process.env.CALLIMACHUS_MCP_BIN : undefined;
  if (override) {
    if (!existsSync(override)) {
      throw new Error(`CALLIMACHUS_MCP_BIN is set but ${override} does not exist`);
    }
    return override;
  }

  const key = `${process.platform}-${process.arch}`;
  const name = assetName(tool);
  if (!name) throw new Error(unsupportedPlatformMessage(key, tool));

  const fetchImpl = opts.fetch ?? fetch;
  const releaseBase = opts.releaseBase ?? RELEASE_BASE;
  const cacheRoot = opts.cacheRoot ?? defaultCacheRoot();
  const cacheDir = join(cacheRoot, version);
  const binPath = join(cacheDir, name);
  if (existsSync(binPath)) {
    // Mark this version as in use, so another client's prune leaves it alone.
    touchQuietly(cacheDir);
    return binPath;
  }

  // Only checked before a download: a cached binary already passed this check,
  // and process.report is not free to generate on every MCP server start.
  if (process.platform === "linux") assertGlibc(linuxGlibcVersion());

  const sums = opts.sums ?? (await loadChecksums(fetchImpl, releaseBase, tool));
  const expected = sums.get(name);
  if (!expected) {
    throw new Error(
      `no SHA-256 checksum for ${name} in the v${version} manifest, refusing to run it.\n` +
        buildFromSource(tool),
    );
  }

  mkdirSync(cacheDir, { recursive: true });
  removeStaleDownloads(cacheDir, Date.now());
  // Download to a temp name, verify, then rename: a concurrent run never sees a
  // partial file at the final path, and a bad download never becomes executable.
  const tmp = join(cacheDir, `.${name}.${process.pid}.tmp`);
  try {
    const actual = await downloadTo(
      fetchImpl,
      `${releaseBase}/${name}`,
      tmp,
      `${tool} ${key}`,
      opts.signal,
    );
    if (actual !== expected) {
      throw new Error(
        `checksum mismatch for ${name}: expected ${expected}, got ${actual}. ` +
          `The download was discarded.`,
      );
    }
    if (process.platform !== "win32") chmodSync(tmp, 0o755);
    renameSync(tmp, binPath);
  } catch (err) {
    removeQuietly(tmp);
    throw err;
  }
  process.stderr.write(`callimachus-mcp: installed ${binPath}\n`);
  pruneOldVersions(cacheRoot, version, Date.now(), linkedVersions(cacheRoot));
  return binPath;
}

/**
 * Cached versions that `~/.local/bin/cal` (from `--install-cal`) runs from.
 * Exec'ing a binary never refreshes its directory's mtime, so without this the
 * pruning below would delete a linked `cal` 30 days after the MCP server moved
 * to a newer release, and the plugin's hooks would silently stop.
 */
export function linkedVersions(cacheRoot, home = homedir()) {
  const link = join(home, ".local", "bin", "cal");
  let target;
  try {
    target = readlinkSync(link);
  } catch {
    return [];
  }
  // Resolved against the link's own directory, as inspectCalSlot does: a
  // relative link into the cache counts as ours there, so it must be kept here.
  const where = cacheLinkVersion(resolve(dirname(link), target), cacheRoot);
  return where?.inRoot ? [where.version] : [];
}

/** `path` with every symlink in it resolved, as far as it exists. */
function realpathOrSelf(path) {
  try {
    return realpathSync(path);
  } catch {
    // Dangling: resolve the directory it would be in, so a link into a
    // symlinked cache (~/.cache -> /data/cache) still compares equal.
    try {
      return join(realpathSync(dirname(path)), basename(path));
    } catch {
      return path;
    }
  }
}

/** The first path segment of `path` under `root`, or null when it is outside. */
function versionUnder(root, path) {
  const rel = relative(root, path);
  if (rel === "" || rel.startsWith("..") || isAbsolute(rel)) return null;
  return rel.split(sep)[0];
}

/**
 * Whether the absolute path `target` is a binary this launcher cached:
 * `{ version, inRoot }` or null. `inRoot` is true for a file under `cacheRoot`
 * (compared with symlinks resolved on both sides, so a symlinked ~/.cache
 * still matches). A `.../callimachus-mcp/<version>/cal-<triple>` path under
 * another root also counts, with `inRoot` false: the shell that ran
 * `--install-cal` may have had another XDG_CACHE_HOME than this process.
 */
export function cacheLinkVersion(target, cacheRoot) {
  const version =
    versionUnder(cacheRoot, target) ??
    versionUnder(realpathOrSelf(cacheRoot), realpathOrSelf(target));
  if (version !== null) return { version, inRoot: true };
  const versionDir = dirname(target);
  if (
    basename(target).startsWith("cal-") &&
    VERSION_DIR.test(basename(versionDir)) &&
    basename(dirname(versionDir)) === "callimachus-mcp"
  ) {
    return { version: basename(versionDir), inRoot: false };
  }
  return null;
}

/** The error for a platform with no released binary. */
export function unsupportedPlatformMessage(key, tool = "callimachus-mcp") {
  const released = `Released targets: ${Object.keys(TRIPLES).join(", ")}.`;
  if (key === "darwin-x64") {
    // ort (fastembed's ONNX backend) publishes no x86_64-apple-darwin prebuilt,
    // and fastembed always downloads one, so there is nothing to build against.
    return (
      `Intel Macs are not supported. ${released} ` +
      `A source build fails there too: the ONNX Runtime that ${tool} links ` +
      `has no Intel macOS build. (On an Apple Silicon Mac this means Node itself is an ` +
      `x64 build running under Rosetta: use an arm64 Node instead.)`
    );
  }
  // A Linux source build inherits the ONNX Runtime prebuilt's glibc floor too.
  const floor = key.startsWith("linux-")
    ? ` A Linux source build needs glibc ${MIN_GLIBC.join(".")} or newer, like the released binary.`
    : "";
  return `unsupported platform ${key}. ${released}\n${buildFromSource(tool)}${floor}`;
}

/**
 * Streams `url` into `dest`, hashing as it goes, and returns the hex SHA-256.
 * Prints coarse progress to stderr so a slow first run is visibly alive.
 */
async function downloadTo(fetchImpl, url, dest, label, signal) {
  process.stderr.write(`callimachus-mcp: downloading v${version} ${label}...\n`);
  const res = await fetchImpl(url, { redirect: "follow", signal });
  if (!res.ok || !res.body) {
    throw new Error(
      `download failed (${res.status} ${res.statusText}) from ${url}\n` +
        `The release asset may not exist for this version/platform yet.`,
    );
  }

  const total = Number(res.headers.get("content-length")) || 0;
  const hash = createHash("sha256");
  let received = 0;
  let nextReport = 0.25;
  const meter = new Transform({
    transform(chunk, _encoding, callback) {
      hash.update(chunk);
      received += chunk.length;
      if (total > 0 && received / total >= nextReport) {
        const pct = Math.floor((received / total) * 100);
        process.stderr.write(`callimachus-mcp: ${pct}% of ${formatMB(total)}\n`);
        while (received / total >= nextReport) nextReport += 0.25;
      }
      callback(null, chunk);
    },
  });

  await pipeline(Readable.fromWeb(res.body), meter, createWriteStream(dest), { signal });
  return hash.digest("hex");
}

/**
 * The expected hashes for this version: the manifest bundled into the npm
 * package at publish time, or (for an unpublished checkout) the SHA256SUMS
 * asset of the matching GitHub Release.
 */
async function loadChecksums(fetchImpl, releaseBase, tool) {
  if (existsSync(BUNDLED_SUMS)) return parseChecksums(readFileSync(BUNDLED_SUMS, "utf8"));

  const url = `${releaseBase}/SHA256SUMS`;
  const res = await fetchImpl(url, { redirect: "follow" });
  if (!res.ok) {
    throw new Error(
      `could not fetch the checksum manifest (${res.status} ${res.statusText}) from ${url}, ` +
        `so the binary cannot be verified.\n${buildFromSource(tool)}`,
    );
  }
  return parseChecksums(await res.text());
}

/**
 * Parses `sha256sum` output (`<hex>  <name>`, or `<hex> *<name>` in binary
 * mode) into a Map of file name -> lowercase hex digest.
 */
export function parseChecksums(text) {
  const sums = new Map();
  for (const line of text.split(/\r?\n/)) {
    const match = line.trim().match(/^([0-9a-fA-F]{64})\s+\*?(.+)$/);
    if (match) sums.set(match[2].trim(), match[1].toLowerCase());
  }
  return sums;
}

/** The runtime glibc version as [major, minor], or null on non-glibc (musl) systems. */
function linuxGlibcVersion() {
  const raw = process.report?.getReport?.().header?.glibcVersionRuntime;
  if (!raw) return null;
  const [major, minor] = String(raw).split(".").map(Number);
  return Number.isFinite(major) && Number.isFinite(minor) ? [major, minor] : null;
}

/** True when a glibc [major, minor] meets the Linux binary's floor. */
export function glibcSupported(glibc) {
  if (!glibc) return false;
  const [major, minor] = glibc;
  return major > MIN_GLIBC[0] || (major === MIN_GLIBC[0] && minor >= MIN_GLIBC[1]);
}

function assertGlibc(glibc) {
  if (glibcSupported(glibc)) return;
  throw new Error(glibcMessage(glibc));
}

/** The error for a Linux system below the glibc floor (or without glibc). */
export function glibcMessage(glibc) {
  const floor = MIN_GLIBC.join(".");
  const found = glibc
    ? `this system has glibc ${glibc.join(".")}`
    : "no glibc was detected (musl-based distros such as Alpine are not supported)";
  // No source-build hint: a source build links the same ONNX Runtime prebuilt and
  // fails with undefined __isoc23_* symbols below glibc 2.38.
  return (
    `the prebuilt Linux binary needs glibc ${floor} or newer ` +
    `(Ubuntu 24.04+, Debian 13+, Fedora 39+), but ${found}.\n` +
    `Building from source doesn't get around this: the ONNX Runtime it links has the same ` +
    `glibc ${floor} floor. Use a newer distro, or run callimachus-mcp in a container based ` +
    `on one (the repo's Dockerfile builds it on Debian 13), with CALLIMACHUS_DB pointing at ` +
    `your mounted index.db.`
  );
}

const VERSION_DIR = /^\d+\.\d+\.\d+([-+][0-9A-Za-z.-]+)?$/;

/**
 * Semver precedence of two `x.y.z[-pre][+build]` strings: negative, zero or
 * positive, like a sort comparator. Build metadata is ignored.
 */
export function compareVersions(a, b) {
  const parse = (v) => {
    const noBuild = v.split("+")[0];
    const dash = noBuild.indexOf("-");
    const core = (dash < 0 ? noBuild : noBuild.slice(0, dash)).split(".").map(Number);
    const pre = dash < 0 ? [] : noBuild.slice(dash + 1).split(".");
    return { core, pre };
  };
  const x = parse(a);
  const y = parse(b);
  for (let i = 0; i < 3; i++) {
    if (x.core[i] !== y.core[i]) return x.core[i] - y.core[i];
  }
  // A release outranks its own prereleases (1.0.0 > 1.0.0-rc.1).
  if (x.pre.length === 0 || y.pre.length === 0) return y.pre.length - x.pre.length;
  for (let i = 0; i < Math.max(x.pre.length, y.pre.length); i++) {
    const p = x.pre[i];
    const q = y.pre[i];
    if (p === undefined) return -1;
    if (q === undefined) return 1;
    if (p === q) continue;
    const pNum = /^\d+$/.test(p);
    const qNum = /^\d+$/.test(q);
    if (pNum && qNum) return Number(p) - Number(q);
    // Numeric identifiers sort before alphanumeric ones.
    if (pNum !== qNum) return pNum ? -1 : 1;
    return p < q ? -1 : 1;
  }
  return 0;
}

/**
 * Whether a cached version directory may be deleted while `keep` runs: it must
 * look like a version, be OLDER than `keep` (a newer one belongs to a client on
 * a newer release), and have gone unused for PRUNE_UNUSED_MS. `lastUsedMs` is
 * the newest mtime of the directory and its files, which a cache hit refreshes
 * and an in-progress download keeps current.
 */
export function shouldPrune(name, keep, lastUsedMs, nowMs) {
  if (name === keep || !VERSION_DIR.test(name)) return false;
  if (compareVersions(name, keep) >= 0) return false;
  return nowMs - lastUsedMs >= PRUNE_UNUSED_MS;
}

/** Newest mtime of `dir` and its direct entries (one binary, maybe a .tmp). */
function lastUsedMs(dir) {
  let newest = statSync(dir).mtimeMs;
  for (const entry of readdirSync(dir)) {
    try {
      newest = Math.max(newest, statSync(join(dir, entry)).mtimeMs);
    } catch {
      // Renamed or removed under us: another run is using this directory.
      return Number.POSITIVE_INFINITY;
    }
  }
  return newest;
}

/**
 * Best-effort cleanup of binaries cached for older, long-unused versions (each
 * is ~40-50MB). A directory holding another run's in-progress `.tmp` download
 * has a fresh mtime, so it is never pulled out from under that run. A binary
 * still running from a deleted directory keeps working on Unix, and on Windows
 * the delete just fails quietly and is retried next time.
 */
export function pruneOldVersions(cacheRoot, keep, nowMs, linked = []) {
  let entries;
  try {
    entries = readdirSync(cacheRoot, { withFileTypes: true });
  } catch {
    return;
  }
  for (const entry of entries) {
    // Cheap name checks first, so nothing that isn't a version dir is even stat'ed.
    if (!entry.isDirectory() || entry.name === keep || !VERSION_DIR.test(entry.name)) continue;
    if (linked.includes(entry.name)) continue;
    const dir = join(cacheRoot, entry.name);
    try {
      if (!shouldPrune(entry.name, keep, lastUsedMs(dir), nowMs)) continue;
      rmSync(dir, { recursive: true, force: true });
    } catch {
      // In use or permission denied: leave it for next time.
    }
  }
}

// A download that has written nothing for this long belongs to a process that
// died mid-download (killed with its MCP client, say). Live downloads keep their
// temp file's mtime current with every chunk.
const STALE_DOWNLOAD_MS = 60 * 60 * 1000;

/**
 * Removes `.<asset>.<pid>.tmp` leftovers in `dir`. Pruning never visits the
 * running version's directory, so without this each killed download stayed
 * there (up to about 40 MB) for good.
 */
export function removeStaleDownloads(dir, nowMs) {
  let entries;
  try {
    entries = readdirSync(dir);
  } catch {
    return;
  }
  for (const entry of entries) {
    if (!entry.startsWith(".") || !entry.endsWith(".tmp")) continue;
    const path = join(dir, entry);
    try {
      if (nowMs - statSync(path).mtimeMs >= STALE_DOWNLOAD_MS) unlinkSync(path);
    } catch {
      // Finished or removed under us.
    }
  }
}

function touchQuietly(path) {
  try {
    const now = new Date();
    utimesSync(path, now, now);
  } catch {
    // Read-only cache: nothing in it can be pruned either.
  }
}

function removeQuietly(path) {
  try {
    unlinkSync(path);
  } catch {
    // Already gone.
  }
}

function formatMB(bytes) {
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}
