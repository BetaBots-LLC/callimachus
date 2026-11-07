// `npx callimachus-mcp --install-cal`: the `cal` CLI without the desktop app.
//
// Downloads `cal-<triple>` from the same GitHub Release as the MCP server (same
// cache, same SHA256SUMS check, see download.mjs) and links it at
// `~/.local/bin/cal`, the path the Claude Code plugin's hooks run. That is also
// where the desktop app's "Install the cal CLI" puts its own link, so the slot
// is shared, and the rules here follow the app's (integration.rs
// `inspect_cal_slot`): a regular file or a link to anything else belongs to
// another program and is never replaced, and a live link to a Callimachus build
// (the app, or one built from source) is left alone.

import { lstatSync, mkdirSync, readlinkSync, renameSync, rmSync, symlinkSync } from "node:fs";
import { homedir } from "node:os";
import { delimiter, dirname, join, resolve } from "node:path";
import {
  cacheLinkVersion,
  compareVersions,
  defaultCacheRoot,
  ensureBinary,
  version,
} from "./download.mjs";

/** `~/.local/bin/cal`: what the plugin's hooks and the VS Code extension run. */
export function calLinkPath(home = homedir()) {
  return join(home, ".local", "bin", "cal");
}

/**
 * What occupies the `cal` link location, without following it:
 * - `{ kind: "missing" }`
 * - `{ kind: "ours", target, version }`: a link into a launcher cache
 *   (`<cache>/<version>/cal-<triple>`, see download.mjs `cacheLinkVersion`),
 *   whether or not it still resolves.
 * - `{ kind: "app", target, live }`: a link to another Callimachus build (the
 *   app binary, which keeps the link up to date itself, or a source build).
 * - `{ kind: "foreign", what }`: anything else, described for the user.
 */
export function inspectCalSlot(link, cacheRoot) {
  let stat;
  try {
    stat = lstatSync(link);
  } catch (err) {
    if (err.code === "ENOENT") return { kind: "missing" };
    return { kind: "foreign", what: `it couldn't be inspected: ${err.message}` };
  }
  if (!stat.isSymbolicLink()) return { kind: "foreign", what: "it's a regular file" };
  let target;
  try {
    target = readlinkSync(link);
  } catch (err) {
    return { kind: "foreign", what: `its link couldn't be read: ${err.message}` };
  }
  const absolute = resolve(dirname(link), target);
  const cached = cacheLinkVersion(absolute, cacheRoot);
  if (cached) return { kind: "ours", target: absolute, version: cached.version };
  // Checked only after the cache: the cache path itself says "callimachus-mcp".
  if (absolute.toLowerCase().includes("callimachus")) {
    return { kind: "app", target: absolute, live: exists(absolute) };
  }
  return { kind: "foreign", what: `it links to ${target}` };
}

function exists(path) {
  try {
    lstatSync(path);
    return true;
  } catch {
    return false;
  }
}

/** Point `link` at `bin` in one step: a reader never sees the link missing. */
function linkAt(link, bin) {
  mkdirSync(dirname(link), { recursive: true });
  const tmp = `${link}.${process.pid}.tmp`;
  rmSync(tmp, { force: true });
  symlinkSync(bin, tmp);
  try {
    renameSync(tmp, link);
  } catch (err) {
    rmSync(tmp, { force: true });
    throw err;
  }
}

/** The error for a `cal` that belongs to another program. */
export function foreignCalMessage(link, what) {
  return (
    `${link} belongs to another program (${what}), so it was left alone. ` +
    `Remove or rename it, then run \`npx callimachus-mcp --install-cal\` again.`
  );
}

/**
 * Downloads `cal` for this release and links it at `~/.local/bin/cal`. Returns
 * a summary for the user (the caller prints it); throws when the slot belongs
 * to another program, on Windows, or when the download fails.
 *
 * `opts` exists for tests: `home`, `cacheRoot`, `ensure` (the downloader) and
 * `platform`.
 */
export async function installCal(opts = {}) {
  const platform = opts.platform ?? process.platform;
  if (platform === "win32") {
    // The plugin's hooks run `~/.local/bin/cal` through sh, which a Windows
    // install doesn't have (see the plugin README), and the app's own `cal.exe`
    // sits next to the app, where the VS Code extension looks for it.
    throw new Error(
      "--install-cal is for macOS and Linux. On Windows, install the Callimachus app and " +
        "click Install the cal CLI in Settings > Agents, or download " +
        `cal-x86_64-pc-windows-msvc.exe from the v${version} release.`,
    );
  }
  const link = calLinkPath(opts.home);
  const cacheRoot = opts.cacheRoot ?? defaultCacheRoot();
  const ensure = opts.ensure ?? (() => ensureBinary("cal"));

  // Look before downloading: a slot we must not touch should not cost a 40 MB download.
  const slot = inspectCalSlot(link, cacheRoot);
  if (slot.kind === "foreign") throw new Error(foreignCalMessage(link, slot.what));
  if (slot.kind === "app" && slot.live) {
    return {
      changed: false,
      message:
        `${link} already runs a Callimachus build (${slot.target}), so it was left alone. ` +
        `To use the launcher's cal instead, remove that link and run ` +
        `\`npx callimachus-mcp --install-cal\` again.`,
    };
  }
  const bin = await ensure();
  if (slot.kind === "ours" && slot.target === bin) {
    return { changed: false, message: `${link} already runs cal v${version}.` };
  }
  linkAt(link, bin);
  return { changed: true, message: `Linked ${link} -> ${bin} (cal v${version}).` };
}

/**
 * Called on every MCP server start: when `~/.local/bin/cal` is a link from an
 * older `--install-cal`, move it to this release, so the hooks' `cal` and the
 * MCP server stay on one version (and one index schema) as the plugin updates.
 * Never downgrades (another MCP client may be pinned to an older release), and
 * never touches a link it did not make. Returns the new target, or null.
 * `opts.signal` aborts a download in progress (the launcher's exit path).
 */
export async function followLinkedCal(opts = {}) {
  if ((opts.platform ?? process.platform) === "win32") return null;
  const link = calLinkPath(opts.home);
  const cacheRoot = opts.cacheRoot ?? defaultCacheRoot();
  const slot = inspectCalSlot(link, cacheRoot);
  if (slot.kind !== "ours" || !/^\d+\.\d+\.\d+/.test(slot.version)) return null;
  const cmp = compareVersions(slot.version, version);
  if (cmp > 0) return null;
  // This release's own link is left alone unless its file is gone: the hooks skip a
  // missing `cal`, so a dangling link would silently turn them off.
  if (cmp === 0 && exists(slot.target)) return null;
  const bin = await (opts.ensure ?? (() => ensureBinary("cal", { signal: opts.signal })))();
  linkAt(link, bin);
  return bin;
}

/** Whether `dir` is on the PATH `env` names. */
export function onPath(dir, env = process.env) {
  return (env.PATH || "").split(delimiter).some((p) => p && resolve(p) === resolve(dir));
}

/** What to run next, after a successful install. */
export function nextSteps(link, env = process.env) {
  const dir = dirname(link);
  const lines = [];
  if (!onPath(dir, env)) {
    lines.push(
      `${dir} is not on your PATH yet. Add it (for example: export PATH="$HOME/.local/bin:$PATH" ` +
        `in your shell profile), or run ${link} by its full path. The plugin's hooks use the ` +
        `full path, so they work either way.`,
    );
  }
  lines.push(
    "Next:",
    "  cal index    import your agents' history into the index (once; it can take a few minutes)",
    "  cal embed    build the semantic index (downloads the 128 MB embedding model once)",
    "  cal watch    keep the index fresh as agents write (in a terminal, or as a login item)",
    "Without cal watch, the Claude Code plugin's SessionStart hook runs a background " +
      "cal index when no pass finished in the last hour.",
  );
  return lines.join("\n");
}
