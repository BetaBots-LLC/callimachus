---
"callimachus": minor
---

Knowledge layer: an opt-in **Knowledge** feature that surfaces what matters from your history.

Off by default — enable it in Settings. The free, on-device tier scans your threads for action items (markdown task checkboxes `- [ ]` and word-boundaried `TODO`/`FIXME` markers, with a noise filter for code/table/JSON blobs) and stores them in a new `facts` table. Enabling backfills from already-indexed text (no re-index); disabling clears it.

- **Desktop:** a **Todos** tab (shown only when the feature is on) lists every open TODO with its source/thread; click to jump to the conversation.
- **`cal todos`** `[-p PROJECT] [-s SOURCE] [-n LIMIT] [--json]` — list open TODOs from the CLI.
- **MCP:** a `list_open_todos` tool so agents can ask "what did I leave unfinished?".

TODOs re-derive on every index, so they never go stale. The LLM-distilled tier (decisions, gotchas, summaries — lazy, per-thread, with consent) reuses the same `facts` table.
