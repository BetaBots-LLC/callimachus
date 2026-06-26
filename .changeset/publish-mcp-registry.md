---
"callimachus": patch
---

**Publish the bundled MCP server to the official MCP registry.** Adds a
`callimachus-mcp` npm package (`packages/callimachus-mcp`): an `npx`-friendly
launcher that downloads the prebuilt `callimachus-mcp` binary for your platform on
first run and execs it over stdio, so any MCP client can run it with
`claude mcp add callimachus -- npx -y callimachus-mcp`. Adds a root `server.json`
(listed as `io.github.betabots-llc/callimachus`), keeps it on the shared version
via `sync-versions.mjs`, and adds a `publish-mcp` CI job that publishes to npm +
the registry after each release.
