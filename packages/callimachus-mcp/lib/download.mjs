// Resolves (and lazily downloads + caches) the prebuilt `callimachus-mcp`
// binary that matches this npm package's version and the host platform.
//
// The npm package is a thin wrapper: it carries no binary itself, but its
// version is kept in lockstep with the desktop release (Changesets `fixed`
// group), so `v<version>` always names a GitHub Release that has the per-target
// `callimachus-mcp-<triple>` asset this downloads.

import { createRequire } from "node:module";
import { chmodSync, existsSync, mkdirSync, renameSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

const require = createRequire(import.meta.url);
const { version } = require("../package.json");

const REPO = "BetaBots-LLC/callimachus";

// Host platform -> Rust target triple uploaded by .github/workflows/build.yml.
// Only these three targets are released today (see build.yml's matrix).
const TRIPLES = {
  "darwin-arm64": "aarch64-apple-darwin",
  "linux-x64": "x86_64-unknown-linux-gnu",
  "win32-x64": "x86_64-pc-windows-msvc",
};

/**
 * Returns an absolute path to a ready-to-exec `callimachus-mcp` binary,
 * downloading it on first run. Throws with an actionable message on an
 * unsupported platform or a failed download.
 */
export async function ensureBinary() {
  // Escape hatch: point at an already-installed binary (e.g. the one the
  // desktop app puts on your PATH, or a `cargo build` output).
  const override = process.env.CALLIMACHUS_MCP_BIN;
  if (override) {
    if (!existsSync(override)) {
      throw new Error(`CALLIMACHUS_MCP_BIN is set but ${override} does not exist`);
    }
    return override;
  }

  const key = `${process.platform}-${process.arch}`;
  const triple = TRIPLES[key];
  if (!triple) {
    throw new Error(
      `unsupported platform ${key}. Released targets: ${Object.keys(TRIPLES).join(", ")}.\n` +
        `Build from source instead:\n` +
        `  cargo install --git https://github.com/${REPO} --bin callimachus-mcp\n` +
        `or set CALLIMACHUS_MCP_BIN to an existing binary.`,
    );
  }
  const ext = process.platform === "win32" ? ".exe" : "";
  const name = `callimachus-mcp-${triple}${ext}`;

  const cacheBase = process.env.XDG_CACHE_HOME || join(homedir(), ".cache");
  const cacheDir = join(cacheBase, "callimachus-mcp", version);
  const binPath = join(cacheDir, name);
  if (existsSync(binPath)) return binPath;

  const url = `https://github.com/${REPO}/releases/download/v${version}/${name}`;
  // Diagnostics MUST go to stderr: stdout is the MCP stdio transport.
  process.stderr.write(`callimachus-mcp: downloading v${version} for ${key}...\n`);

  const res = await fetch(url, { redirect: "follow" });
  if (!res.ok) {
    throw new Error(
      `download failed (${res.status} ${res.statusText}) from ${url}\n` +
        `The release asset may not exist for this version/platform yet.`,
    );
  }
  const buf = Buffer.from(await res.arrayBuffer());

  mkdirSync(cacheDir, { recursive: true });
  // Write to a temp name then rename, so a concurrent run never sees a partial
  // file at the final path.
  const tmp = join(cacheDir, `.${name}.${process.pid}.tmp`);
  writeFileSync(tmp, buf);
  if (process.platform !== "win32") chmodSync(tmp, 0o755);
  renameSync(tmp, binPath);
  process.stderr.write(`callimachus-mcp: installed ${binPath}\n`);
  return binPath;
}
