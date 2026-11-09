# Callimachus for VS Code & Cursor

Search your AI coding-agent conversation history across **Claude Code, Codex, Cursor, Gemini, Qwen, Goose, OpenCode, Continue, Cline, Roo Code, Kilo Code, Copilot Chat, Copilot CLI** and in-app chats, without leaving the editor. Pick a past thread and open its full transcript as a markdown doc.

This extension is a thin client over the **`cal`** CLI, so it shares the exact same local index as the Callimachus desktop app and MCP server. No separate database, no separate indexing.

## Install

- **VS Code** — [VS Code Marketplace](https://marketplace.visualstudio.com/) (search "Callimachus").
- **Cursor / VSCodium** — [Open VSX](https://open-vsx.org/) (these editors install from Open VSX, not the VS Code Marketplace).
- **Manual** — download the `.vsix` from [Releases](../../../releases) and run *Extensions: Install from VSIX…*.

## Requirements

- The **Callimachus desktop app** installed and run at least once: it builds the index, and its **`cal`** CLI is what this extension shells out to. The extension finds it without `PATH`: it checks the `~/.local/bin/cal` link that **Settings → Agents → Enable for Claude Code** adds and the installed app itself (on macOS, Windows, and the deb or rpm `/usr/bin/callimachus` on Linux), which answers `cal` commands too (this also covers a per-machine Windows install that can't write `cal.exe`). With the Linux AppImage, enable that integration or set `callimachus.calPath`. (Building `cal` yourself from a checkout: `cargo install --locked --path apps/desktop/src-tauri/standalone --bin cal`, adding `--force` over a source install from an earlier version, then set `callimachus.calPath` if it isn't on your `PATH`.)

## Sidebar

Click the **Callimachus** icon in the Activity Bar to open the **History** panel — a live search box over your whole AI-history index, with source/project scope, a recent-threads list, and a corpus-stats footer. It themes itself to your editor (light/dark/high-contrast).

Pick any result to open its full transcript as a rich tab, with one-click **Insert**, **Copy**, **Export**, and **Open in CLI** actions. Hover a row in the sidebar to insert or copy without opening it. **Export** writes a markdown note to your `callimachus.vaultPath` (or, if that's unset, opens it as an untitled document), and **Open in CLI** seeds the `callimachus.openCommand` agent with the thread's context.

## Ambient Recall

The sidebar can also work *without* you searching. With **Ambient Recall** on, it watches what you're looking at — the current selection, the symbol under the cursor, or the nearest error — and surfaces past threads related to it (via `cal related`), so prior context finds you instead of the other way around. Toggle it with the **Callimachus: Toggle Ambient Recall** command or the `callimachus.ambientRecall` setting; tune its timing and volume with the `ambientRecall*` settings below.

## Provenance in the gutter

Open any file in a git repository and the lines a past AI session wrote get a thin mark in the gutter: the lines whose commit (by `git blame`) Callimachus linked to a session, or that an [Agent Trace](https://agent-trace.dev) record in the repo's `.agent-trace/` attributes to an agent. Hover one to see the session's title, agent and date, how many files it shares with the commit, and up to three of its decisions or gotchas, with **Open session** (the transcript tab) and **Why this line** (the sidebar's full answer). A line another tool traced shows its agent, model and conversation too, even when Callimachus has no transcript of it.

It costs one background `cal why` per file version: asked when the file is shown, again 500 ms after you stop typing (an unsaved buffer is blamed as it is, so the marks follow your edits), one process per file and two in all at a time, cancelled when a newer version supersedes it or the file is closed or hidden, and stopped after 5 seconds together with the `git blame` it runs. Files over 20,000 lines and files outside a git repository are skipped. An unsaved buffer is blamed only when the file is UTF-8 (with or without a byte order mark); other encodings keep the saved file's marks until you save. On VS Code before 1.100, which does not report a file's encoding, a file in a legacy 8-bit encoding is taken for UTF-8, and its non-ASCII lines lose their marks while it is unsaved. Commits are linked to sessions by the desktop app as it indexes (or by `cal commits`). Turn the marks or the hover off with `callimachus.whyGutter` and `callimachus.whyHover`.

## Commands

| Command | What it does |
|---|---|
| **Callimachus: Search History** | Search every indexed source, pick a thread, open the transcript. |
| **Callimachus: Search History (current project)** | Same, scoped to the open workspace folder's path. |
| **Callimachus: Search History for Selection** | Use the current editor selection as the query. |
| **Callimachus: Recent Threads** | Browse the most recently updated threads. |
| **Callimachus: Insert Thread into Editor** | Insert a thread's transcript at the cursor (seed a chat / notes). |
| **Callimachus: Copy Thread Context** | Copy a thread's packed transcript to the clipboard. |
| **Callimachus: Why This Line** | Show, in the sidebar, the commit that last changed the line under the cursor (`git blame`), the sessions that produced it, and their decisions and gotchas. Also in the editor's right-click menu. Uses `cal why`, so it needs a Callimachus that has it. |
| **Callimachus: Toggle Ambient Recall** | Turn the sidebar's ambient-recall section on or off. |

There's also a **status-bar** button (`$(history) Callimachus`) that opens search.

## Settings

- `callimachus.calPath` — path to the `cal` binary (default `cal`). Leave as `cal` if it's on your `PATH`.
- `callimachus.resultLimit` — max results to fetch per search (default `40`).
- `callimachus.vaultPath` — Obsidian vault folder for the **Export** action (default empty). When empty, Export opens the note as an untitled document instead of writing to a vault.
- `callimachus.openCommand` — CLI agent the **Open in CLI** action seeds with a thread's context (default `claude`; e.g. `codex`, `gemini`). Must be on your `PATH`.
- `callimachus.ambientRecall` — surface past threads related to your selection / symbol / nearest error in the sidebar, with no searching (default `true`).
- `callimachus.ambientRecallThrottle` — milliseconds to wait after the cursor/selection settles before querying for related threads (default `1500`).
- `callimachus.ambientRecallMinContext` — minimum length of the editor context (selection or symbol) before ambient recall runs (default `10`).
- `callimachus.ambientRecallLimit` — max related threads to show in the ambient-recall section (default `5`).
- `callimachus.whyGutter`: mark the lines a past session produced (or an Agent Trace record attributes) in the gutter (default `true`).
- `callimachus.whyHover`: on hover over such a line, show the session and its decisions, with Open session and Why this line (default `true`).

## Develop

```bash
pnpm install          # from the monorepo root (pulls @types/vscode etc.)
pnpm --filter callimachus-vscode build
# then press F5 in VS Code to launch an Extension Development Host
pnpm --filter callimachus-vscode package   # build a .vsix with @vscode/vsce
```
