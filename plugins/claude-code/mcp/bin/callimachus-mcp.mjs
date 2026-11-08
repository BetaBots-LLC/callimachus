#!/usr/bin/env node
// Launcher for the Callimachus MCP server.
//
// Resolves the prebuilt `callimachus-mcp` binary for this platform (downloading
// it from the matching GitHub Release on first run, then caching it under
// ~/.cache), and execs it, passing stdio straight through so the host MCP client
// talks to the real server.
//
// `npx callimachus-mcp --install-cal` instead downloads the `cal` CLI from the
// same release and links it at ~/.local/bin/cal (lib/install-cal.mjs), for a
// machine without the desktop app.
//
// IMPORTANT: when serving, this wrapper must never write to stdout. stdout is the
// MCP stdio transport; any stray bytes corrupt the protocol. All diagnostics go
// to stderr.

import { spawn } from "node:child_process";
import { ensureBinary } from "../lib/download.mjs";
import { calLinkPath, followLinkedCal, installCal, nextSteps } from "../lib/install-cal.mjs";

if (process.argv[2] === "--install-cal") {
  try {
    const { changed, message } = await installCal();
    process.stdout.write(`${message}\n`);
    if (changed) process.stdout.write(`${nextSteps(calLinkPath())}\n`);
    process.exit(0);
  } catch (err) {
    process.stderr.write(`callimachus-mcp: ${err.message}\n`);
    process.exit(1);
  }
}

// How long the launcher outlives its server to let a `cal` download that
// followLinkedCal started finish. Past it the download is aborted, which removes
// its temp file; a later start tries again.
const FOLLOW_GRACE_MS = 10_000;

try {
  const bin = await ensureBinary();

  const child = spawn(bin, process.argv.slice(2), { stdio: "inherit" });
  const follow = new AbortController();
  let following = Promise.resolve();

  /**
   * Exits once a follow in progress has settled: exiting at once killed its
   * download midway, which left a partial `.cal-<triple>.<pid>.tmp` behind.
   */
  const exitAfterFollow = (exit) => {
    const grace = setTimeout(() => follow.abort(), FOLLOW_GRACE_MS);
    grace.unref();
    following.finally(() => {
      clearTimeout(grace);
      exit();
    });
  };

  const forward = (sig) => {
    try {
      child.kill(sig);
    } catch {
      // child already gone
    }
  };
  process.on("SIGINT", () => forward("SIGINT"));
  process.on("SIGTERM", () => forward("SIGTERM"));

  child.on("error", (err) => {
    process.stderr.write(`callimachus-mcp: failed to start binary: ${err.message}\n`);
    process.exit(127);
  });
  child.on("exit", (code, signal) => {
    exitAfterFollow(() => {
      if (signal) {
        // Re-raise the same signal so our exit status mirrors the child's.
        process.kill(process.pid, signal);
        return;
      }
      process.exit(code ?? 0);
    });
  });

  // After the server is up, so the client's startup never waits on it: keep a
  // `cal` from an earlier --install-cal on this release. Best effort, stderr only.
  following = followLinkedCal({ signal: follow.signal }).then(
    (moved) => {
      if (moved) process.stderr.write(`callimachus-mcp: ~/.local/bin/cal now runs ${moved}\n`);
    },
    (err) => {
      if (follow.signal.aborted) return;
      process.stderr.write(`callimachus-mcp: could not update ~/.local/bin/cal: ${err.message}\n`);
    },
  );
} catch (err) {
  process.stderr.write(`callimachus-mcp: ${err.message}\n`);
  process.exit(1);
}
