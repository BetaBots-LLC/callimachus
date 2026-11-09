// Shared wrapper over the `cal` CLI. All `cal` invocations and the JSON shapes it
// returns live here so the webview RPC, the panels, and the palette commands stay
// in sync. Types are shared with the webview via ./protocol.
//
// The extension is a thin client over `cal` (which ships with the Callimachus
// desktop app and reads the same local index). If `cal` isn't installed, or the
// index hasn't been built yet, we surface a friendly "download the app" prompt
// instead of a raw error — see CalSetupError + showCalSetupPrompt.

import * as vscode from "vscode";
import { execFile, spawn } from "node:child_process";
import { existsSync, realpathSync } from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { promisify } from "node:util";
import type { SearchHit, Stats, ThreadSummary, WhyResult } from "./protocol";

const exec = promisify(execFile);

const DOWNLOAD_URL = "https://callimachus.app/download";

export function config<T>(key: string, fallback: T): T {
  return vscode.workspace.getConfiguration("callimachus").get<T>(key) ?? fallback;
}

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

const MISSING_MESSAGE =
  "The `cal` CLI wasn't found. Install the Callimachus desktop app (it provides `cal`), or set callimachus.calPath.";

/**
 * Known locations of the Callimachus CLI, checked before PATH. The desktop app's
 * "Install integration" step symlinks `~/.local/bin/cal` to the app binary; the app
 * binary itself also serves `cal` subcommands, so the bundle path works without it.
 *
 * Bare `/usr/bin/cal` is deliberately absent: on macOS and most Linux distros it is the
 * system *calendar*, which answers every call with a usage message.
 */
function calCandidates(): string[] {
  const home = os.homedir();
  const exe = process.platform === "win32" ? "cal.exe" : "cal";
  const c: string[] = [path.join(home, ".local", "bin", exe)];
  if (process.platform === "darwin") {
    c.push(
      "/usr/local/bin/cal",
      "/opt/homebrew/bin/cal",
      "/Applications/Callimachus.app/Contents/MacOS/callimachus",
      path.join(home, "Applications", "Callimachus.app", "Contents", "MacOS", "callimachus"),
    );
  } else if (process.platform === "win32") {
    // Each install dir: the `cal.exe` the app drops next to itself, then the app binary.
    // A per-machine (MSI / Program Files) install can't write `cal.exe`, but the app runs
    // `search`, `recent` and every other cal subcommand itself, so it works as `cal` too.
    // Its path contains "callimachus", so isCallimachusCal accepts it without launching it.
    // NTFS is case-insensitive, so one spelling covers callimachus.exe / Callimachus.exe.
    const dirs: string[] = [];
    const la = process.env.LOCALAPPDATA;
    const pf = process.env.ProgramFiles;
    if (la) dirs.push(path.join(la, "Programs", "Callimachus"), path.join(la, "Callimachus"));
    if (pf) dirs.push(path.join(pf, "Callimachus"));
    for (const dir of dirs) c.push(path.join(dir, exe), path.join(dir, "callimachus.exe"));
  } else {
    // The deb and rpm app binary, which runs every cal subcommand itself. Those packages no
    // longer install /usr/bin/cal (it collided with the system calendar), so a user who never
    // enabled the integration would otherwise find nothing. isCallimachusCal accepts it by path.
    c.push("/usr/local/bin/cal", "/usr/bin/callimachus");
  }
  return c;
}

/** Every `cal` on PATH, in PATH order (the system calendar is filtered out later). */
function pathCandidates(): string[] {
  const exe = process.platform === "win32" ? "cal.exe" : "cal";
  return (process.env.PATH ?? "")
    .split(path.delimiter)
    .filter(Boolean)
    .map((dir) => path.join(dir, exe));
}

// `cal help` from Callimachus prints its usage banner ("cal — search your indexed AI
// coding-agent history"); the BSD / util-linux calendar rejects `help` and exits non-zero.
const CAL_SIGNATURE = /coding-agent history|callimachus/i;
const PROBE_TIMEOUT_MS = 5_000;

/** Whether `bin` is the Callimachus CLI rather than the system calendar. */
async function isCallimachusCal(bin: string): Promise<boolean> {
  // 1. By install path: the installer's symlink resolves into the app bundle / install
  //    dir, and the app binary itself is named callimachus. No process needed.
  try {
    if (/callimachus/i.test(realpathSync(bin))) return true;
  } catch {
    return false; // dangling symlink or unreadable
  }
  // 2. By output. Only files named `cal` are probed: the app runs in CLI mode when
  //    invoked as `cal`, whereas the app binary under another name with an unknown
  //    argument would launch the GUI.
  const stem = path.basename(bin, path.extname(bin)).toLowerCase();
  if (stem !== "cal") return false;
  try {
    const { stdout, stderr } = await exec(bin, ["help"], {
      timeout: PROBE_TIMEOUT_MS,
      windowsHide: true,
    });
    return CAL_SIGNATURE.test(`${stdout}\n${stderr}`);
  } catch {
    return false;
  }
}

async function findCallimachusCal(): Promise<string | undefined> {
  const seen = new Set<string>();
  for (const cand of [...calCandidates(), ...pathCandidates()]) {
    if (seen.has(cand)) continue;
    seen.add(cand);
    if (existsSync(cand) && (await isCallimachusCal(cand))) return cand;
  }
  return undefined;
}

let cachedBin: string | undefined;
let resolving: Promise<string | undefined> | undefined;
// After a failed discovery, don't re-probe on every call (the sidebar fires several
// RPCs at once), but do retry soon so a fresh desktop-app install is picked up.
let lastMiss = 0;
const REDISCOVER_AFTER_MS = 30_000;

/** Resolve the `cal` binary: an explicit `calPath` setting wins; otherwise the first
 *  known install location or PATH entry that is really the Callimachus CLI. Throws
 *  CalSetupError("missing") when there is none. */
async function resolveCalBin(): Promise<string> {
  const configured = config<string>("calPath", "cal").trim();
  if (configured && configured !== "cal") return configured; // user override
  if (cachedBin) return cachedBin;
  if (Date.now() - lastMiss < REDISCOVER_AFTER_MS) {
    throw new CalSetupError("missing", MISSING_MESSAGE);
  }
  resolving ??= findCallimachusCal().finally(() => {
    resolving = undefined;
  });
  const found = await resolving;
  if (!found) {
    lastMiss = Date.now();
    throw new CalSetupError("missing", MISSING_MESSAGE);
  }
  cachedBin = found;
  return found;
}

/** The resolved `cal` binary path — for callers that build their own command line
 *  (e.g. the "Open in CLI" terminal action). */
export function calBinPath(): Promise<string> {
  return resolveCalBin();
}

// The system calendar's reply to arguments it doesn't understand (BSD exits 64).
const SYSTEM_CAL_USAGE = /^usage: cal\b/im;

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
function execCal(
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

/** Run `cal` with args, returning stdout. Throws CalSetupError when `cal` is
 *  missing or the index hasn't been built yet. */
export async function runCal(args: string[], opts: RunCalOptions = {}): Promise<string> {
  const bin = await resolveCalBin();
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
      cachedBin = undefined; // uninstalled or moved: rediscover next time
      throw new CalSetupError("missing", MISSING_MESSAGE);
    }
    const stderr = e.stderr?.toString().trim();
    // A user-set calPath (or a stale PATH entry) pointing at the system calendar.
    if (e.code === 64 || SYSTEM_CAL_USAGE.test(`${stderr ?? ""}\n${e.stdout?.toString() ?? ""}`)) {
      cachedBin = undefined;
      throw new CalSetupError(
        "missing",
        `\`${bin}\` is the system calendar, not the Callimachus CLI. Install the desktop app or fix callimachus.calPath.`,
      );
    }
    if (stderr && /no index found/i.test(stderr)) {
      throw new CalSetupError(
        "no-index",
        "No index yet — open the Callimachus app once to index your AI agent history.",
      );
    }
    throw new Error(stderr || e.message || String(err));
  }
}

// Only one setup prompt at a time: the sidebar fires several RPCs on open, which
// would otherwise stack three identical toasts.
let lastPromptAt = 0;

/** If `err` is a CalSetupError, show a download/setup prompt and return true
 *  (handled). Otherwise return false so the caller shows a generic error. */
export function showCalSetupPrompt(err: unknown): boolean {
  if (!(err instanceof CalSetupError)) return false;
  const now = Date.now();
  if (now - lastPromptAt < 4000) return true; // debounce duplicates
  lastPromptAt = now;

  const buttons =
    err.reason === "missing" ? ["Download Callimachus", "Set cal path…"] : ["Download Callimachus"];
  void vscode.window
    .showWarningMessage(`Callimachus: ${err.message}`, ...buttons)
    .then((choice) => {
      if (choice === "Download Callimachus") {
        void vscode.env.openExternal(vscode.Uri.parse(DOWNLOAD_URL));
      } else if (choice === "Set cal path…") {
        void vscode.commands.executeCommand("workbench.action.openSettings", "callimachus.calPath");
      }
    });
  return true;
}

/** Search snippets wrap matches in char(1)/char(2) sentinels; strip for display. */
const MARK_RE = new RegExp(`[${String.fromCharCode(1)}${String.fromCharCode(2)}]`, "g");
export const stripMarks = (s: string): string => s.replace(MARK_RE, "");

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

/** The newest `cal --json` major version this extension reads. Each payload opens with
 *  `schema_version` (schemas: https://callimachus.app/schemas/cal/); a field added within
 *  a version is ignored here, a newer version means the shape changed incompatibly. */
export const CAL_SCHEMA_VERSION = 1;

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
function parseList<T>(raw: string, command: string): T[] {
  const payload = parseJson(raw, command);
  if (Array.isArray(payload)) return payload as T[];
  checkVersion(payload, command);
  const results = (payload as { results?: unknown } | null)?.results;
  if (!Array.isArray(results)) {
    throw new Error(`\`cal ${command}\` returned JSON without a results list. Update the cal CLI?`);
  }
  return results as T[];
}

/** An object output (`stats`): its fields follow `schema_version`, or come alone from an
 *  older `cal`. */
function parseObject<T>(raw: string, command: string): T {
  const payload = parseJson(raw, command);
  if (typeof payload !== "object" || payload === null || Array.isArray(payload)) {
    throw new Error(`\`cal ${command}\` returned JSON that isn't an object. Update the cal CLI?`);
  }
  checkVersion(payload, command);
  return payload as T;
}

export async function searchHits(query: string, project?: string): Promise<SearchHit[]> {
  const limit = String(config<number>("resultLimit", 40));
  const args = ["search", query, "--json", "-n", limit];
  if (project) args.push("-p", project);
  return parseList<SearchHit>(await runCal(args), "search");
}

export async function recentThreads(project?: string): Promise<ThreadSummary[]> {
  const limit = String(config<number>("resultLimit", 40));
  const args = ["recent", "--json", "-n", limit];
  if (project) args.push("-p", project);
  return parseList<ThreadSummary>(await runCal(args), "recent");
}

export async function stats(): Promise<Stats> {
  return parseObject<Stats>(await runCal(["stats", "--json"]), "stats");
}

// Ambient recall re-asks for the same few contexts as the cursor moves back and forth
// between symbols, and each `cal related` is a fresh process that loads the embedding
// model. Keep the last few answers in memory (briefly, since the index keeps growing).
const RELATED_CACHE_MAX = 20;
const RELATED_CACHE_TTL_MS = 10 * 60_000;
const relatedCache = new Map<string, { at: number; results: ThreadSummary[] }>();

/** Threads semantically related to arbitrary editor context — powers ambient
 *  recall. Matches across all projects (no `-p`); returns thread summaries. */
export async function relatedThreads(contextText: string): Promise<ThreadSummary[]> {
  const limit = String(config<number>("ambientRecallLimit", 5));
  const key = `${limit}\u0000${contextText}`;
  const hit = relatedCache.get(key);
  if (hit && Date.now() - hit.at < RELATED_CACHE_TTL_MS) {
    relatedCache.delete(key); // refresh its LRU position
    relatedCache.set(key, hit);
    return hit.results;
  }
  const results = parseList<ThreadSummary>(
    await runCal(["related", contextText, "--json", "-n", limit]),
    "related",
  );
  relatedCache.delete(key);
  relatedCache.set(key, { at: Date.now(), results });
  while (relatedCache.size > RELATED_CACHE_MAX) {
    const oldest = relatedCache.keys().next().value;
    if (oldest === undefined) break;
    relatedCache.delete(oldest);
  }
  return results;
}

/** Forget cached ambient-recall answers (e.g. on an explicit refresh). */
export function clearRelatedCache(): void {
  relatedCache.clear();
}

/** The packed markdown transcript for a thread. */
export function catThread(id: number): Promise<string> {
  return runCal(["cat", String(id)]);
}

/** "Why this line": the commits behind `line` of `file` (git blame), the sessions that
 *  produced them and their decisions. Run from the file's folder, so `cal` finds the repo
 *  the way it would in a terminal there. */
export async function whyLine(file: string, line: number): Promise<WhyResult> {
  if (!(await supportsWhy(await resolveCalBin()))) throw new Error(NO_WHY_MESSAGE);
  let raw: string;
  try {
    raw = await runCal(["why", `${file}:${line}`, "--json"], {
      cwd: path.dirname(file),
      timeoutMs: WHY_TIMEOUT_MS,
    });
  } catch (err) {
    // A `cal` from before `cal why` (the extension updates apart from the app).
    if (/unknown command 'why'/.test((err as Error).message)) {
      throw new Error(NO_WHY_MESSAGE);
    }
    throw err;
  }
  return parseObject<WhyResult>(raw, "why");
}

const NO_WHY_MESSAGE =
  "This version of Callimachus has no `cal why`. Update the app, then try again.";

/** Whether the resolved `cal` has `cal why`. Throws CalSetupError when there is no `cal`. */
export async function calHasWhy(): Promise<boolean> {
  return supportsWhy(await resolveCalBin());
}

/** Commits `whyFile` asks for: every linked one of a long-lived file (linked commits are
 *  picked first, so the unlinked ones past this are the ones dropped). */
const WHY_FILE_COMMITS = 100;

/** `cal why <file>` for the whole file: each blamed commit with the lines it owns
 *  (`ranges`), its sessions and its Agent Trace attributions. With `input`, blames that
 *  buffer instead of the saved file (`--stdin`), so line numbers match an unsaved editor. */
export async function whyFile(
  file: string,
  opts: { input?: string; signal?: AbortSignal; timeoutMs: number },
): Promise<WhyResult> {
  const args = ["why", file, "--json", "-n", String(WHY_FILE_COMMITS)];
  if (opts.input !== undefined) args.push("--stdin");
  const raw = await runCal(args, {
    cwd: path.dirname(file),
    timeoutMs: opts.timeoutMs,
    signal: opts.signal,
    input: opts.input,
  });
  return parseObject<WhyResult>(raw, "why");
}

// `cal why` gives git 30 s (WHY_GIT_BUDGET in cli_core.rs); past this something is stuck,
// and the sidebar would otherwise wait forever.
const WHY_TIMEOUT_MS = 60_000;

const whySupport = new Map<string, Promise<boolean>>();

/** Whether `bin` has `cal why`. A binary named `cal` answers an unknown command with an
 *  error, which whyLine reports. The app binary under its own name (a bundle-path
 *  candidate) enters CLI mode only for a command its version knows, so an app from before
 *  `cal why` would start a second GUI window instead. Ask its `help` (a command in every
 *  release) once per binary and look for `cal why` in the usage. */
function supportsWhy(bin: string): Promise<boolean> {
  const stem = path.basename(bin, path.extname(bin)).toLowerCase();
  if (stem === "cal") return Promise.resolve(true);
  let known = whySupport.get(bin);
  if (!known) {
    known = exec(bin, ["help"], { timeout: PROBE_TIMEOUT_MS, windowsHide: true }).then(
      ({ stdout, stderr }) => /\bcal why\b/.test(`${stdout}\n${stderr}`),
      () => false,
    );
    // Only a yes is kept: the app may be updated (or finish installing) meanwhile.
    void known.then((ok) => {
      if (!ok) whySupport.delete(bin);
    });
    whySupport.set(bin, known);
  }
  return known;
}
