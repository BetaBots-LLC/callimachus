---
"callimachus": minor
---

Three small, high-leverage improvements across the chat agent, CLI, and MCP server:

- **In-app chat now has a system prompt.** The chat agent is told its edge is your own indexed coding history and when to reach for each tool (`search_history` / `get_thread` / `run_shell`), with a `[thread N]` citation convention. Previously it had tools but no instructions, so it rarely searched your history — especially on weaker or local models.
- **`cal audit-pr` resolves abbreviated commit SHAs.** Commit provenance now prefix-matches the short hashes that `git log --oneline` and PR tooling produce against the stored full SHAs, instead of silently returning empty provenance for them.
- **New `recurring_issues` MCP tool.** Any connected agent can now ask which errors you keep hitting in a repo (scoped to the current project by default, `"*"` for all) before retrying a known-broken approach — the same analysis behind `cal issues`, now reachable over MCP.
