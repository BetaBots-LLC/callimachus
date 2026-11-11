// The parts of the `cal` client that need no editor: running a resolved `cal` binary, telling
// its setup failures apart, and reading its versioned `--json` output. No `vscode` import, so
// `node --test` (apps/vscode/test) runs them against a fake `cal`. cal.ts adds binary
// discovery and the editor prompts on top.

import { spawn } from "node:child_process";
import { existsSync } from "node:fs";

/** The newest `cal --json` major version this extension reads. Each payload opens with
 *  `schema_version` (schemas: https://callimachus.app/schemas/cal/); a field added within
 *  a version is ignored here, a newer version means the shape changed incompatibly. */
export const CAL_SCHEMA_VERSION = 1;

/** Why a `cal` call couldn't run, so the UI can show the right call-to-action. */
export class CalSetupError extends Error {
  constructor(
    public readonly reason: "missing" | "no-index",
    message: string,
  ) {
    super(message);
    this.name = "CalSetupError";
  }
}

/** A `cal` that ran but could not read the index for a reason the user fixes outside the
 *  editor. The message stays `cal`'s own stderr, so the existing error toasts read as before. */
export class CalStateError extends Error {
  constructor(
    public readonly problem: "busy" | "schema-behind" | "restore-interrupted",
    message: string,
  ) {
    super(message);
    this.name = "CalStateError";
  }
}

/** A `cal` newer than this extension: its output uses a shape this build can't read. */
export class CalVersionError extends Error {
  constructor(
    public readonly command: string,
    public readonly version: number,
  ) {
    super(
      `\`cal ${command}\` uses output format version ${version}; this extension reads up to version ${CAL_SCHEMA_VERSION}. Update the Callimachus extension.`,
    );
    this.name = "CalVersionError";
  }
}

export const MISSING_MESSAGE =
  "The `cal` CLI wasn't found. Install the Callimachus desktop app (it provides `cal`), or set callimachus.calPath.";

export const NO_INDEX_MESSAGE =
  "No index yet: open the Callimachus app once to index your AI agent history.";

// The system calendar's reply to arguments it doesn't understand (BSD exits 64). macOS 26's
// /usr/bin/cal prints "Usage: cal [general options] ..." for `cal memory --json -n 40`.
const SYSTEM_CAL_USAGE = /^usage: cal\b/im;

export type CalProblem =
  | "system-cal"
  | "no-index"
  | "busy"
  | "schema-behind"
  | "restore-interrupted";

/** What a failed `cal` run means, from its stderr (and stdout and exit code), or null for an
 *  ordinary error. Each pattern is what core prints through `run_and_exit` (`cal: {e}`):
 *  - `db::open_readonly`: "no index at <path> - open the Callimachus app once to build it
 *    (or set CALLIMACHUS_DB)"; `open_with` says "no index at <path> - open the app to build
 *    it first". The extension used to look for "no index found", which core never prints.
 *  - `run_and_exit` on `db::is_busy`: "the index is busy (an upgrade or a large cleanup may be
 *    running); try again shortly".
 *  - SQLite on an index older than this `cal` (read paths never migrate or check the schema;
 *    the hooks classify the same two strings as `SkipClass::SchemaBehind`): "no such table:
 *    <t>" / "no such column: <c>".
 *  - `backup::interrupted_restore_error` (Batch K): "a restore was interrupted before it
 *    finished: ...".
 *  All four were reproduced with the Batch L debug `cal` (2026-11-10): a missing
 *  CALLIMACHUS_DB, an APFS clone with a table renamed, the clone under an exclusive lock, and
 *  a folder in the half-swapped state. */
export function calProblem(stderr: string, stdout = "", code?: number | string): CalProblem | null {
  if (code === 64 || SYSTEM_CAL_USAGE.test(`${stderr}\n${stdout}`)) return "system-cal";
  if (/\bno index at\b/i.test(stderr)) return "no-index";
  if (/\bthe index is busy\b/i.test(stderr)) return "busy";
  if (/\ba restore was interrupted before it finished\b/i.test(stderr)) {
    return "restore-interrupted";
  }
  if (/\bno such (table|column)\b/i.test(stderr)) return "schema-behind";
  return null;
}

export interface RunCalOptions {
  cwd?: string;
  timeoutMs?: number;
  /** Aborting kills the `cal` process (the editor gutter cancels a superseded run). */
  signal?: AbortSignal;
  /** Written to `cal`'s stdin (an unsaved buffer for `cal why --stdin`). */
  input?: string;
}

/** Most `cal` output kept, as execFile's maxBuffer was. */
const MAX_OUTPUT_BYTES = 64 * 1024 * 1024;

/** End `pid` and everything it started. `cal why` runs `git blame`, and killing `cal` alone
 *  orphaned it: each run the gutter stopped (a timeout, a newer version) left a full-file
 *  blame running to the end. On Unix `cal` leads its own process group (spawned detached),
 *  so signalling the group reaches git; on Windows `taskkill /T` walks the tree. */
function killTree(pid: number | undefined): void {
  if (pid === undefined) return;
  if (process.platform === "win32") {
    const killer = spawn("taskkill", ["/pid", String(pid), "/T", "/F"], {
      windowsHide: true,
      stdio: "ignore",
    });
    killer.on("error", () => {});
    return;
  }
  try {
    process.kill(-pid, "SIGTERM");
  } catch {
    // Already gone.
  }
}

/** Run `cal`, with stdin input, and reject with an error carrying `code`, `stdout` and
 *  `stderr` the way execFile's does. A timeout or an abort kills the whole process tree. */
export function execCal(
  bin: string,
  args: string[],
  opts: RunCalOptions,
): Promise<{ stdout: string; stderr: string }> {
  return new Promise((resolve, reject) => {
    if (opts.signal?.aborted) {
      reject(Object.assign(new Error("aborted"), { name: "AbortError", stdout: "", stderr: "" }));
      return;
    }
    const child = spawn(bin, args, {
      cwd: opts.cwd,
      windowsHide: true,
      // Its own process group on Unix, so killTree reaches the git it runs.
      detached: process.platform !== "win32",
      stdio: ["pipe", "pipe", "pipe"],
      // Ask for the versioned `--json` envelopes through the environment, not the
      // `--json-version` flag: an older `cal` refuses an unknown flag but ignores the
      // variable and prints its bare arrays, which parseList / parseObject still read.
      env: { ...process.env, CAL_JSON_VERSION: String(CAL_SCHEMA_VERSION) },
    });
    const out: Buffer[] = [];
    const err: Buffer[] = [];
    let size = 0;
    let settled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const text = (chunks: Buffer[]) => Buffer.concat(chunks).toString("utf8");
    const finish = (error: Error | null) => {
      if (settled) return;
      settled = true;
      if (timer) clearTimeout(timer);
      opts.signal?.removeEventListener("abort", onAbort);
      const stdout = text(out);
      const stderr = text(err);
      if (error) reject(Object.assign(error, { stdout, stderr }));
      else resolve({ stdout, stderr });
    };
    const stop = (error: Error) => {
      killTree(child.pid);
      finish(error);
    };
    const onAbort = () =>
      stop(Object.assign(new Error("aborted"), { name: "AbortError", killed: true }));
    const collect = (into: Buffer[]) => (chunk: Buffer) => {
      size += chunk.length;
      if (size > MAX_OUTPUT_BYTES) {
        stop(new Error("cal printed more than the extension reads"));
        return;
      }
      into.push(chunk);
    };
    child.stdout?.on("data", collect(out));
    child.stderr?.on("data", collect(err));
    child.on("error", (e) => finish(e));
    child.on("close", (code, signal) => {
      if (code === 0) {
        finish(null);
        return;
      }
      const e = new Error(`Command failed: ${bin} ${args.join(" ")}`);
      finish(Object.assign(e, { code: code ?? undefined, signal: signal ?? undefined }));
    });
    opts.signal?.addEventListener("abort", onAbort, { once: true });
    if (opts.timeoutMs !== undefined) {
      timer = setTimeout(
        () =>
          stop(
            Object.assign(new Error(`cal timed out after ${opts.timeoutMs} ms`), {
              killed: true,
            }),
          ),
        opts.timeoutMs,
      );
    }
    // A `cal` that exits before reading everything (an error, an abort) closes the pipe.
    child.stdin?.on("error", () => {});
    // Closed even with no input: a command that reads stdin gets EOF instead of waiting.
    child.stdin?.end(opts.input);
  });
}

/** Run the resolved `bin` with `args`, returning stdout. Throws CalSetupError when `bin` is
 *  missing, is the system calendar or finds no index, CalStateError when the index is busy,
 *  older than `bin` or halfway through a restore, else an Error with `cal`'s stderr (or
 *  naming `opts.cwd` when that folder does not exist). */
export async function runCalAt(
  bin: string,
  args: string[],
  opts: RunCalOptions = {},
): Promise<string> {
  try {
    const { stdout } = await execCal(bin, args, opts);
    return stdout;
  } catch (err: unknown) {
    const e = err as {
      stderr?: Buffer | string;
      stdout?: Buffer | string;
      code?: string | number;
      message?: string;
    };
    if (e.code === "ENOENT") {
      // spawn reports a `cwd` that does not exist with the same ENOENT as a missing binary
      // (Node's child_process docs, `options.cwd`), and the error names only the binary. A
      // workspace folder deleted or unmounted under an open window read as "cal not found",
      // and cal.ts dropped its cached binary for it.
      if (opts.cwd !== undefined && !existsSync(opts.cwd)) {
        throw new Error(`The folder ${opts.cwd} does not exist, so cal could not run in it.`);
      }
      throw new CalSetupError("missing", MISSING_MESSAGE);
    }
    const stderr = e.stderr?.toString().trim() ?? "";
    switch (calProblem(stderr, e.stdout?.toString() ?? "", e.code)) {
      case "system-cal":
        // A user-set calPath (or a stale PATH entry) pointing at the system calendar.
        throw new CalSetupError(
          "missing",
          `\`${bin}\` is the system calendar, not the Callimachus CLI. Install the desktop app or fix callimachus.calPath.`,
        );
      case "no-index":
        throw new CalSetupError("no-index", NO_INDEX_MESSAGE);
      case "busy":
        throw new CalStateError("busy", stderr);
      case "schema-behind":
        throw new CalStateError("schema-behind", stderr);
      case "restore-interrupted":
        throw new CalStateError("restore-interrupted", stderr);
      default:
        throw new Error(stderr || e.message || String(err));
    }
  }
}

/** Parse `cal --json` stdout, turning malformed output into a clear error rather
 *  than a raw SyntaxError (e.g. an old `cal` that doesn't support `--json`). */
function parseJson(raw: string, command: string): unknown {
  try {
    return JSON.parse(raw);
  } catch {
    throw new Error(
      `\`cal ${command}\` returned output that wasn't valid JSON. Update the cal CLI?`,
    );
  }
}

/** Refuse a payload from a newer major version before reading any field of it, so a
 *  newer `cal` fails with a clear message instead of a missing-field error downstream.
 *  A payload without `schema_version` comes from a `cal` that predates versioning. */
function checkVersion(payload: unknown, command: string): void {
  if (typeof payload !== "object" || payload === null || Array.isArray(payload)) return;
  const version = (payload as { schema_version?: unknown }).schema_version;
  if (typeof version === "number" && version > CAL_SCHEMA_VERSION) {
    throw new CalVersionError(command, version);
  }
}

/** A list output: `{ schema_version, results }`, or a bare array from a `cal` that predates
 *  versioning (it ignores CAL_JSON_VERSION), told apart by shape. */
export function parseList<T>(raw: string, command: string): T[] {
  const payload = parseJson(raw, command);
  if (Array.isArray(payload)) return payload as T[];
  checkVersion(payload, command);
  const results = (payload as { results?: unknown } | null)?.results;
  if (!Array.isArray(results)) {
    throw new Error(`\`cal ${command}\` returned JSON without a results list. Update the cal CLI?`);
  }
  return results as T[];
}

/** An object output (`stats`, `memory`): its fields follow `schema_version`, or come alone
 *  from an older `cal`. */
export function parseObject<T>(raw: string, command: string): T {
  const payload = parseJson(raw, command);
  if (typeof payload !== "object" || payload === null || Array.isArray(payload)) {
    throw new Error(`\`cal ${command}\` returned JSON that isn't an object. Update the cal CLI?`);
  }
  checkVersion(payload, command);
  return payload as T;
}
