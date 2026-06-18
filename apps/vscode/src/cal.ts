// Shared wrapper over the `cal` CLI. All `cal` invocations and the JSON shapes it
// returns live here so the commands and the tree view stay in sync.

import * as vscode from "vscode";
import { execFile } from "node:child_process";
import { promisify } from "node:util";

const exec = promisify(execFile);

/** Message-level search hit (mirrors the Rust `SearchHit`, camelCase JSON). */
export interface SearchHit {
  threadId: number;
  source: string;
  title: string | null;
  snippet: string;
  projectPath: string | null;
}

/** Thread summary row (mirrors the Rust `ThreadSummary`). */
export interface ThreadSummary {
  id: number;
  source: string;
  title: string | null;
  projectPath: string | null;
  messageCount: number;
  updatedAt: number | null;
}

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

/** Search snippets wrap matches in \u0001…\u0002 sentinels; strip for display. */
export const stripMarks = (s: string): string => s.replace(/[\u0001\u0002]/g, "");

export async function searchHits(query: string, project?: string): Promise<SearchHit[]> {
  const limit = String(config<number>("resultLimit", 40));
  const args = ["search", query, "--json", "-n", limit];
  if (project) args.push("-p", project);
  return JSON.parse(await runCal(args)) as SearchHit[];
}

export async function recentThreads(): Promise<ThreadSummary[]> {
  const limit = String(config<number>("resultLimit", 40));
  return JSON.parse(await runCal(["recent", "--json", "-n", limit])) as ThreadSummary[];
}

/** The packed markdown transcript for a thread. */
export function catThread(id: number): Promise<string> {
  return runCal(["cat", String(id)]);
}

/** Open a thread's packed transcript as a markdown document. */
export async function openThread(id: number): Promise<void> {
  const md = await catThread(id);
  const doc = await vscode.workspace.openTextDocument({ content: md, language: "markdown" });
  await vscode.window.showTextDocument(doc, { preview: true });
}
