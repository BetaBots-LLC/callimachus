#!/usr/bin/env node
// Launcher for the Callimachus MCP server.
//
// Resolves the prebuilt `callimachus-mcp` binary for this platform (downloading
// it from the matching GitHub Release on first run, then caching it under
// ~/.cache), and execs it, passing stdio straight through so the host MCP client
// talks to the real server.
//
// IMPORTANT: this wrapper must never write to stdout. stdout is the MCP stdio
// transport; any stray bytes corrupt the protocol. All diagnostics go to stderr.

import { spawn } from "node:child_process";
import { ensureBinary } from "../lib/download.mjs";

try {
  const bin = await ensureBinary();

  const child = spawn(bin, process.argv.slice(2), { stdio: "inherit" });

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
    if (signal) {
      // Re-raise the same signal so our exit status mirrors the child's.
      process.kill(process.pid, signal);
      return;
    }
    process.exit(code ?? 0);
  });
} catch (err) {
  process.stderr.write(`callimachus-mcp: ${err.message}\n`);
  process.exit(1);
}
