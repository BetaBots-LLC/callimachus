---
"callimachus-vscode": minor
---

Add a rich webview UI for VS Code & Cursor.

- **Callimachus sidebar** in a new Activity Bar container: live search over your whole AI-history index with All / This-project scope, a recent-threads list, hover insert/copy, and a corpus-stats footer.
- **Transcript tabs:** pick a result to open its conversation in an editor tab, styled to match the desktop app — right-aligned user bubbles, full-markdown assistant turns, and collapsible tool calls.
- **Themed to the editor:** the UI follows your active VS Code / Cursor theme (light / dark / high-contrast) via the editor's own theme variables.
- Built with Vite, reusing the desktop app's shadcn components and Markdown renderer; data flows over a typed message bridge to the `cal` CLI (no Tauri in the editor).
- Adds `callimachus.vaultPath` (Export destination) and `callimachus.openCommand` (Open-in-CLI agent) settings.

Replaces the old Explorer "Callimachus History" tree view.
