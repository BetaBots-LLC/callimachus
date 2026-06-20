---
"callimachus": minor
---

**Two-way agent memory.** Agents (and you) can now WRITE to memory, not just read it, and the richest capabilities reach the MCP surface agents actually use. The MCP server is now a read+write tool (15 tools).

- **Closeable TODOs.** Mark an open TODO done so it drops out of every open-TODO list: a check button on each TODO in the Knowledge tab, `cal done <id>`, and a `complete_todo` MCP tool. Completion (and pin/dismiss) now survives re-indexing, so the task list stops re-filling with noise you already handled.
- **Agent write-back.** Record a decision or gotcha mid-session that persists in the project's memory and immediately surfaces in recall: `record_decision` / `record_gotcha` MCP tools and `cal remember <decision|gotcha> <text>`. Recorded facts are pinned, embedded for cross-thread recall, and flow through Project Memory and the memory file like distilled facts.
- **MCP parity.** The already-built RAG and code-aware search are now MCP tools too: `ask_history` (a synthesized, cited answer over the user's history) and `threads_for_file` (which past sessions touched a path). The ask retrieval is factored into one shared path used by the app, `cal ask`, and MCP.
- The MCP server and the standalone `callimachus-mcp` binary now open the index read-write (WAL + busy_timeout let them coexist with the desktop app's writer).
