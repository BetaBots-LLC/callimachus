// Shared wrapper over the `cal` CLI. All `cal` invocations and the JSON shapes it
// returns live here so the webview RPC, the panels, and the palette commands stay
// in sync. Types are shared with the webview via ./protocol.

import * as vscode from "vscode";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import type { SearchHit, Stats, ThreadSummary } from "./protocol";

const exec = promisify(execFile);

export function config<T>(key: string, fallback: T): T {
  return vscode.workspace.getConfiguration("callimachus").get<T>(key) ?? fallback;
}

/** Run `cal` with args, returning stdout. cal exits 1 with a friendly stderr. */
export async function runCal(args: string[]): Promise<string> {
  const bin = config<string>("calPath", "cal");
  try {
    const { stdout } = await exec(bin, args, { maxBuffer: 64 * 1024 * 1024 });
    return stdout;
  } catch (err: unknown) {
    const e = err as { stderr?: Buffer | string; code?: string; message?: string };
    if (e.code === "ENOENT") {
      throw new Error(
        `Could not run \`${bin}\`. Install the Callimachus CLI or set "callimachus.calPath".`,
      );
    }
    const stderr = e.stderr?.toString().trim();
    throw new Error(stderr || e.message || String(err));
  }
}

/** Search snippets wrap matches in char(1)/char(2) sentinels; strip for display. */
const MARK_RE = new RegExp(`[${String.fromCharCode(1)}${String.fromCharCode(2)}]`, "g");
export const stripMarks = (s: string): string => s.replace(MARK_RE, "");

export async function searchHits(query: string, project?: string): Promise<SearchHit[]> {
  const limit = String(config<number>("resultLimit", 40));
  const args = ["search", query, "--json", "-n", limit];
  if (project) args.push("-p", project);
  return JSON.parse(await runCal(args)) as SearchHit[];
}

export async function recentThreads(project?: string): Promise<ThreadSummary[]> {
  const limit = String(config<number>("resultLimit", 40));
  const args = ["recent", "--json", "-n", limit];
  if (project) args.push("-p", project);
  return JSON.parse(await runCal(args)) as ThreadSummary[];
}

export async function stats(): Promise<Stats> {
  return JSON.parse(await runCal(["stats", "--json"])) as Stats;
}

/** The packed markdown transcript for a thread. */
export function catThread(id: number): Promise<string> {
  return runCal(["cat", String(id)]);
}
