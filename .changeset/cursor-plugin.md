---
"callimachus": patch
---

A Cursor plugin (`plugins/cursor`, listed by `.cursor-plugin/marketplace.json` at the repo root so Cursor's marketplace import and cursor.directory find it): the `recall` skill, the MCP server (`npx -y callimachus-mcp`, pinned to the release) and `sessionStart` / `sessionEnd` hooks that run `cal hook --for cursor` and `cal session-end --for cursor` through `~/.local/bin/cal`, staying silent when `cal` is missing or too old for Cursor's protocol. With both the plugin and the app's own Cursor hooks (Settings > Agents), `cal hook --for cursor` now injects the memory once per conversation: the first hook claims it with a marker file under `~/.callimachus/recall`, which `cal session-end --for cursor` removes and a week-old one is pruned.
