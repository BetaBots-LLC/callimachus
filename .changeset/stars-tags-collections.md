---
"callimachus": minor
---

Stars, tags & collections — organize your archive, not just search it.

- **Star** any thread and attach free-form **tags**, then filter the list by a ⭐ Starred toggle and tag chips in the search bar.
- Stars and tags survive re-indexing (stars live on the thread row but are never overwritten by the indexer; tags are keyed separately).
- Reaches every surface: desktop UI, the `cal` CLI (`cal star <id> [--off]`, `cal tag <id> <tag…>`, `cal tags`, plus `--starred` / `-t <tag>` on `recent`/`related`/`search`), and the MCP server (`recent_threads` gains `starred`/`tags`, new `list_tags` tool) so agents can ask for "my starred auth threads".
- Added a `busy_timeout` on the SQLite connection so `cal` writes (star/tag) wait for the app's lock instead of failing with "database is locked".
