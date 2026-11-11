// A stand-in for `cal` in the process tests, run through a `cal` shell wrapper the test
// writes (so the binary's name is `cal`, as the extension finds it). Controlled by the
// environment the extension passes through:
//   FAKE_CAL_ARGV    file to write {argv, cwd, jsonVersion, pid} to
//   FAKE_CAL_STDOUT  file whose bytes go to stdout
//   FAKE_CAL_STDERR  text for stderr
//   FAKE_CAL_EXIT    exit code (default 0)
//   FAKE_CAL_SLEEP_MS  wait this long first (the timeout test)

import { readFileSync, writeFileSync } from "node:fs";

const env = process.env;
if (env.FAKE_CAL_ARGV) {
  writeFileSync(
    env.FAKE_CAL_ARGV,
    JSON.stringify({
      argv: process.argv.slice(2),
      cwd: process.cwd(),
      jsonVersion: env.CAL_JSON_VERSION ?? null,
      // The wrapper `exec`s node, so this is the pid the extension spawned (the abort test).
      pid: process.pid,
    }),
  );
}

function answer() {
  if (env.FAKE_CAL_STDOUT) process.stdout.write(readFileSync(env.FAKE_CAL_STDOUT));
  if (env.FAKE_CAL_STDERR) process.stderr.write(`${env.FAKE_CAL_STDERR}\n`);
  process.exitCode = Number(env.FAKE_CAL_EXIT ?? 0);
}

const sleep = Number(env.FAKE_CAL_SLEEP_MS ?? 0);
if (sleep > 0) setTimeout(answer, sleep);
else answer();
