---
"callimachus": patch
---

Faster, lighter, and better search results, measured on a 5 GB index of 1.4 million messages.

- **Memory:** the embedding model unloads after it has been idle (10 minutes in the app, 1 minute in the MCP server), and bulk builds use their own model instance that is dropped when the build ends. The app no longer sits at 1.5 GB after a day, and an MCP server no longer keeps 400 MB after one semantic query.
- **Semantic search:** results are filtered by a calibrated similarity floor, so a query no longer pads out with unrelated hits; Starred, tag and date filters now apply to semantic results too; the keyword and semantic passes run in parallel and the semantic pass is capped, which takes about 200 ms off each hybrid query. Turning the semantic switch on warms the model so the first query is not keyword-only.
- **Keyword search:** quoted phrases are honored as phrases (about 14x faster and precise), tool output ranks below prose, identical snippets collapse, recent activity gets a nudge, and a page no longer comes back short when many threads match. A new search cancels the previous one.
- **Threads:** opening a long thread sends tool output as a preview and fetches the full text only when a row is expanded (the largest thread went from a 6 MB payload to a fraction of it); switching tabs keeps the Search view, its scroll positions and the open thread; expanded rows stay expanded when scrolled away and back; scroll height no longer drifts.
- **Background work:** new messages are embedded automatically about a minute after agents go quiet, so semantic search, Ask and recall stay current without pressing Build; a message appended to a distilled thread no longer wipes its facts; the launch check for new sessions no longer flashes "Indexing…" or disables Reindex; live refresh pauses while the window is hidden and never re-runs a typed search behind your back; every background job logs its duration.
- **Reindex toasts** report the messages that were actually new.
- **Storage:** large tool calls from Claude Code (a Write of a whole file, for example) are now stored like tool output, head and tail with the middle elided, which keeps the index smaller; Shrink index applies the same cap to calls stored by older versions.
