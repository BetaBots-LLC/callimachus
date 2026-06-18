// Shared wrapper over the `cal` CLI. All `cal` invocations and the JSON shapes it
// returns live here so the webview RPC, the panels, and the palette commands stay
// in sync. Types are shared with the webview via ./protocol.
//
// The extension is a thin client over `cal` (which ships with the Callimachus
// desktop app and reads the same local index). If `cal` isn't installed, or the
// index hasn't been built yet, we surface a friendly "download the app" prompt
// instead of a raw error — see CalSetupError + showCalSetupPrompt.

import * as vscode from "vscode";
import { execFile } from "node:child_process";
import { existsSync } from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { promisify } from "node:util";
import type { SearchHit, Stats, ThreadSummary } from "./protocol";

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

/** Known locations the desktop app drops `cal`, checked before falling back to PATH. */
function calCandidates(): string[] {
  const home = os.homedir();
  const exe = process.platform === "win32" ? "cal.exe" : "cal";
  const c: string[] = [path.join(home, ".local", "bin", exe)];
  if (process.platform === "darwin") {
    c.push(
      "/usr/local/bin/cal",
      "/opt/homebrew/bin/cal",
      "/Applications/Callimachus.app/Contents/Resources/cal",
      "/Applications/Callimachus.app/Contents/MacOS/cal",
    );
  } else if (process.platform === "win32") {
    const la = process.env.LOCALAPPDATA;
    const pf = process.env.ProgramFiles;
    if (la) c.push(path.join(la, "Programs", "Callimachus", exe));
    if (pf) c.push(path.join(pf, "Callimachus", exe));
  } else {
    c.push("/usr/local/bin/cal", "/usr/bin/cal");
  }
  return c;
}

let cachedBin: string | undefined;

/** Resolve the `cal` binary: an explicit `calPath` setting wins; otherwise probe
 *  known install locations; otherwise fall back to PATH (`cal`). */
function resolveCalBin(): string {
  const configured = config<string>("calPath", "cal");
  if (configured && configured !== "cal") return configured; // user override
  if (cachedBin) return cachedBin;
  for (const cand of calCandidates()) {
    if (existsSync(cand)) {
      cachedBin = cand;
      return cand;
    }
  }
  return "cal"; // not cached — a later app install should be discovered on next call
}

/** The resolved `cal` binary path/name — for callers that build their own command
 *  line (e.g. the "Open in CLI" terminal action). */
export function calBinPath(): string {
  return resolveCalBin();
}

/** Run `cal` with args, returning stdout. Throws CalSetupError when `cal` is
 *  missing or the index hasn't been built yet. */
export async function runCal(args: string[]): Promise<string> {
  const bin = resolveCalBin();
  try {
    const { stdout } = await exec(bin, args, { maxBuffer: 64 * 1024 * 1024 });
    return stdout;
  } catch (err: unknown) {
    const e = err as { stderr?: Buffer | string; code?: string; message?: string };
    if (e.code === "ENOENT") {
      throw new CalSetupError(
        "missing",
        "Callimachus needs the desktop app, which provides the `cal` CLI and builds your searchable history.",
      );
    }
    const stderr = e.stderr?.toString().trim();
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
  void vscode.window.showWarningMessage(`Callimachus: ${err.message}`, ...buttons).then((choice) => {
    if (choice === "Download Callimachus") {
      void vscode.env.openExternal(vscode.Uri.parse(DOWNLOAD_URL));
    } else if (choice === "Set cal path…") {
      void vscode.commands.executeCommand(
        "workbench.action.openSettings",
        "callimachus.calPath",
      );
    }
  });
  return true;
}

/** Search snippets wrap matches in char(1)/char(2) sentinels; strip for display. */
const MARK_RE = new RegExp(`[${String.fromCharCode(1)}${String.fromCharCode(2)}]`, "g");
export const stripMarks = (s: string): string => s.replace(MARK_RE, "");

/** Parse `cal --json` stdout, turning malformed output into a clear error rather
 *  than a raw SyntaxError (e.g. an old `cal` that doesn't support `--json`). */
function parseJson<T>(raw: string, command: string): T {
  try {
    return JSON.parse(raw) as T;
  } catch {
    throw new Error(`\`cal ${command}\` returned output that wasn't valid JSON. Update the cal CLI?`);
  }
}

export async function searchHits(query: string, project?: string): Promise<SearchHit[]> {
  const limit = String(config<number>("resultLimit", 40));
  const args = ["search", query, "--json", "-n", limit];
  if (project) args.push("-p", project);
  return parseJson<SearchHit[]>(await runCal(args), "search");
}

export async function recentThreads(project?: string): Promise<ThreadSummary[]> {
  const limit = String(config<number>("resultLimit", 40));
  const args = ["recent", "--json", "-n", limit];
  if (project) args.push("-p", project);
  return parseJson<ThreadSummary[]>(await runCal(args), "recent");
}

export async function stats(): Promise<Stats> {
  return parseJson<Stats>(await runCal(["stats", "--json"]), "stats");
}

/** The packed markdown transcript for a thread. */
export function catThread(id: number): Promise<string> {
  return runCal(["cat", String(id)]);
}
