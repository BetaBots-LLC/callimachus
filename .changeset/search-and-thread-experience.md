---
"callimachus": patch
---

Search results and threads are easier to scan and to move through.

- **Results:** one row per thread, two lines: the title with its match count and time, then where the match came from ("You:", "Assistant:", "Bash output:") and the project. Arrow keys move through the list from the search box, Enter opens, Esc returns to the box. No-match states name the active filters and offer "Clear filters" and "Try semantic search"; an empty index offers Reindex inline.
- **Threads:** runs of tool calls fold into one line ("12 tool calls · Bash 5 · Read 4 · Edit 3") that expands in place, with results attached to their calls; a Match stepper walks the search hits (Enter / Shift+Enter, Cmd+G) and a Prompts stepper walks your messages ([ and ]); the header is two lines (title and actions, then source, project, branch, time and inline tags), and the commits row only appears when there are commits.
- **Elsewhere:** Coach's recurring errors open a search for that error; the Projects toolbar keeps Build memory and Remember and moves the rest under More, with facts as a plain list; Stats opens with one summary row; the command palette gains "Search for …" and recent threads and uses the same labels as the tabs; Chat has one New chat button and focuses its box on open; drawn icons replace the few text glyphs.
