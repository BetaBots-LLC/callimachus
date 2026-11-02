// `pnpm desktop:dev`: run the desktop app in dev against its OWN index, never the installed
// app's. The dev build carries newer migrations than the release; opening the real index
// upgrades it in place, after which the installed app can't open it any more (it showed up
// as a crash at launch of 0.11.0 after a dev run migrated the index to a newer schema).
//
// Set CALLIMACHUS_DB yourself to point the dev app somewhere else, including at the real
// index if you accept that upgrade (an APFS clone of it is the safer choice, see CLAUDE.md).
import { spawn } from "node:child_process";
import { homedir } from "node:os";
import { join } from "node:path";

function appDataDir() {
  // Mirrors dirs::data_local_dir() in db::default_index_path.
  if (process.platform === "darwin") return join(homedir(), "Library", "Application Support");
  if (process.platform === "win32")
    return process.env.LOCALAPPDATA ?? join(homedir(), "AppData", "Local");
  return process.env.XDG_DATA_HOME ?? join(homedir(), ".local", "share");
}

const env = { ...process.env };
if (!env.CALLIMACHUS_DB) {
  env.CALLIMACHUS_DB = join(appDataDir(), "dev.shaller.callimachus", "index.dev.db");
  console.log(`[desktop:dev] CALLIMACHUS_DB=${env.CALLIMACHUS_DB} (dev index; the installed app's index is untouched)`);
}

const child = spawn("pnpm", ["--filter", "callimachus", "tauri", "dev", ...process.argv.slice(2)], {
  stdio: "inherit",
  env,
  shell: process.platform === "win32",
});
child.on("exit", (code, signal) => {
  if (signal) process.kill(process.pid, signal);
  process.exit(code ?? 1);
});
