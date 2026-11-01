// Thread actions shared by the webview RPC and the palette commands. Each takes a
// plain thread id so both callers hit the same core.

import * as path from "node:path";
import * as vscode from "vscode";
import { calBinPath, catThread, config, runCal } from "./cal";

/** Insert a thread's transcript at the cursor of the active editor. */
export async function insertThreadById(id: number): Promise<void> {
  const editor = vscode.window.activeTextEditor;
  if (!editor) {
    vscode.window.showInformationMessage("Callimachus: open an editor to insert into.");
    return;
  }
  const md = await catThread(id);
  await editor.edit((b) => b.insert(editor.selection.active, md));
}

/** Copy a thread's packed context to the clipboard. */
export async function copyThreadById(id: number): Promise<void> {
  await vscode.env.clipboard.writeText(await catThread(id));
  vscode.window.showInformationMessage("Callimachus: thread context copied.");
}

/**
 * Export a thread as an Obsidian note. With `callimachus.vaultPath` set, write it
 * into the vault; otherwise open the note as an untitled markdown document.
 */
export async function exportThreadById(id: number): Promise<void> {
  const vault = config<string>("vaultPath", "").trim();
  if (vault) {
    await runCal(["export", String(id), "--vault", vault]);
    vscode.window.showInformationMessage(`Callimachus: exported thread ${id} to ${vault}.`);
    return;
  }
  const md = await runCal(["export", String(id)]);
  const doc = await vscode.workspace.openTextDocument({ content: md, language: "markdown" });
  await vscode.window.showTextDocument(doc, { preview: false });
}

type ShellSyntax = "posix" | "powershell";

/**
 * Which command-line syntax the new terminal will speak, and the shell to force when the
 * default can't do command substitution. On Windows, cmd.exe has no `$()` at all and
 * wsl.exe can't run the Windows `cal.exe` path, so those (and anything unrecognized) get
 * a Windows PowerShell terminal, which every Windows install has.
 */
function terminalShell(): { syntax: ShellSyntax; shellPath?: string } {
  const name = path
    .basename(vscode.env.shell || "")
    .toLowerCase()
    .replace(/\.exe$/, "");
  if (name === "pwsh" || name === "powershell") return { syntax: "powershell" };
  if (process.platform !== "win32") return { syntax: "posix" };
  // Git Bash, MSYS2, Cygwin.
  if (["bash", "zsh", "sh", "fish"].includes(name)) return { syntax: "posix" };
  return { syntax: "powershell", shellPath: "powershell.exe" };
}

/** The line that runs `program` with `cal cat <id>` spliced in as its one argument. The
 *  subshell keeps the (large) context off the visible command line. The binary path is
 *  quoted because install dirs can contain spaces ("Program Files"). */
function cliCommandLine(program: string, bin: string, id: number, syntax: ShellSyntax): string {
  if (syntax === "powershell") {
    // `&` is required: PowerShell parses a quoted path followed by arguments as an
    // expression ("Unexpected token 'cat'"). Out-String keeps the transcript's line
    // breaks; a bare subexpression would join its output lines with spaces.
    const quoted = `'${bin.replace(/'/g, "''")}'`;
    return `${program} "$(& ${quoted} cat ${id} | Out-String)"`;
  }
  return `${program} "$("${bin}" cat ${id})"`;
}

/** Seed a CLI agent (default `claude`) with the thread's context in a terminal. */
export async function openInCli(id: number): Promise<void> {
  const program = config<string>("openCommand", "claude").trim() || "claude";
  const bin = await calBinPath(); // throws CalSetupError (setup prompt) when cal is missing
  const shell = terminalShell();
  const term = vscode.window.createTerminal({
    name: `Callimachus ▸ thread ${id}`,
    shellPath: shell.shellPath,
  });
  term.show();
  term.sendText(cliCommandLine(program, bin, id, shell.syntax));
}
