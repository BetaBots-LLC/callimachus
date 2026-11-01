# Security & Privacy

## Privacy model

Callimachus is **local-first**. Your index, transcripts and memory never leave
your machine, except conversation content you explicitly send to an LLM provider
you choose. Specifically:

- Your conversation index (`index.db`) lives only on your machine and is never
  uploaded anywhere.
- Embeddings are computed **on-device**; no text is sent to a cloud embedding API.
- API keys are stored in the **OS keychain**, never in plaintext on disk or in the
  repo.

### What the app sends over the network

- **The LLM provider you explicitly pick.** Conversation content leaves your
  machine **only** when you use a cloud provider for one of three features:
  (1) the **in-app chat**, (2) **Knowledge distillation** (`agent::distill`) of
  decisions, gotchas, and summaries, and (3) **Ask your history / RAG**
  (`agent::answer`, also reachable as `cal ask` and the `ask_history` MCP tool).
  The request goes directly from your machine to that provider with your own key.
  The keyless **Claude Code / Codex CLI** engines send it to Anthropic / OpenAI
  through those CLIs, under your own subscription. Choosing **local Ollama** at its
  default local address keeps distillation and Ask fully on-device. Pointing its
  custom server URL at a **remote** host (even one you control, like a VPS or a
  homelab box) transmits conversation content off the local machine; it stays
  within your own infrastructure, but it is no longer on-device. **Ollama Cloud**
  (ollama.com) is a cloud provider like any other: picking it sends conversation
  content to Ollama's hosted service, keyed by your Ollama API key. Indexing,
  search, and embeddings are **always local** regardless of which engine you pick.
- **The update check.** At launch and periodically while it runs, the desktop app
  fetches the release manifest (`latest.json`) from GitHub Releases to see whether
  a newer version exists, and downloads the update only when you accept it.
  GitHub sees an ordinary download request (your IP address and the app's user
  agent), nothing about your data.
- **A one-time model download.** The first time you build the semantic index, the
  small embedding model (bge-small-en-v1.5) is downloaded from its public host.
- **Opt-in usage analytics (desktop app only).** The desktop app can send
  anonymous usage analytics to PostHog (US): event names, app version, OS, CPU
  architecture and language, and a few properties picked from fixed lists (such
  as which agent source, AI provider or setting an event concerns, an error
  category, or a count rounded into a range). It never sends file paths, thread
  titles, prompts, transcripts, project names or API keys. It is **off until you
  choose Allow**, and you can turn it off any time in **Settings → General** or
  with `CALLIMACHUS_TELEMETRY=0` / `DO_NOT_TRACK=1`. Events are tied to a random install ID that is created when you
  opt in and deleted when you opt out. The analytics token is compiled into
  official release builds only, so builds from source never send anything. The
  **CLI and MCP server never send analytics.** The exact list of events and
  properties is in the [privacy policy](https://callimachus.app/privacy).

The website (callimachus.app) uses PostHog in cookieless mode: no cookies, no
browser storage, no cross-site tracking.

### Safeguards

- Before a transcript is packed for an LLM provider, an agent CLI or another
  agent over MCP, common secret formats (AI provider API keys, Stripe keys and
  webhook secrets, GitHub and Slack tokens, AWS access keys, bearer tokens, JWTs,
  private-key blocks, and `api_key=`/`password:`-style assignments) are replaced
  with `[REDACTED]`. This is best-effort pattern matching, not a guarantee, so treat
  any secret that appeared in a transcript as exposed to the providers you use.
- The in-app agent's `run_shell` tool requires **explicit per-command approval**
  before anything executes.
- Agent-recorded and LLM-distilled facts that Callimachus injects into new
  sessions (hooks, `AGENTS.md` / `CLAUDE.md`) are labelled as notes from past
  sessions, not instructions. Review a repo's memory (`cal memory`, or the
  Projects tab) before committing a managed block other people will load.
- Release artifacts are signed: the updater with minisign, macOS builds and the
  standalone macOS binaries with a notarized Developer ID, Windows builds and
  binaries with Authenticode. The `callimachus-mcp` npm launcher checks every
  binary it downloads against a SHA-256 manifest shipped inside the npm package.

## Reporting a vulnerability

Please **do not** open a public issue for security vulnerabilities.

Email **ari@shaller.dev** with:

- a description of the issue and its impact,
- steps to reproduce (a proof-of-concept if possible),
- any suggested remediation.

You'll get an acknowledgement within a few days. Once a fix is released, we're
happy to credit you (or keep you anonymous, your call).

## Supported versions

The latest released version receives security fixes. As a pre-1.0 project, fixes
ship in the next patch release rather than as backports.
