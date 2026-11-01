# Security & Privacy

## Privacy model

Callimachus is **local-first**. Your index, transcripts and memory never leave
your machine, except conversation content you explicitly send to an LLM provider
you choose and the excerpts that agents you connect pull into their own sessions.
Specifically:

- Your conversation index (`index.db`) lives only on your machine and is never
  uploaded anywhere.
- Embeddings are computed **on-device**; no text is sent to a cloud embedding API.
- API keys are stored in the **OS keychain**, never in plaintext on disk or in the
  repo.
- The desktop app keeps a diagnostics log, `callimachus.log`, next to `index.db`
  (rotated at 1 MB, keeping one previous copy as `callimachus.log.1`; readable
  only by you on macOS and Linux). It records background errors, which can
  include file and project paths and error text from AI providers. Nothing
  uploads it; attach it to a bug report only if you choose to. The `cal` CLI and
  the MCP server never write to it.

### What the app sends over the network

- **The LLM provider you explicitly pick.** Conversation content leaves your
  machine **only** when you use a cloud provider for one of these features:
  (1) the **in-app chat**, including the history results and threads its tools
  fetch; (2) **Knowledge distillation** (`agent::distill`) of decisions, gotchas,
  and summaries, including background auto-distill when you turn it on (threads
  active in the last 30 days, up to the daily limit you set, 50 by default);
  (3) **Ask your history / RAG** (`agent::answer`, also reachable as `cal ask`
  and the `ask_history` MCP tool); (4) the Projects tab's **Synthesize brief**,
  **Write memory file** and **Review conflicts** (`agent::project_brief`,
  `agent::find_conflicts`), which send the project's path and the notes already
  distilled from it; and (5) the **AI-summarized Obsidian export**
  (`agent::synthesize`, also `cal export --synthesize`), which sends the thread
  to the engine picked under **Settings → Obsidian export** (for the CLI, the
  first provider with a key).
  The request goes directly from your machine to that provider with your own key,
  after common secret formats in it are replaced with `[REDACTED]` (see
  [Safeguards](#safeguards)).
  The keyless **Claude Code / Codex CLI** engines send it to Anthropic / OpenAI
  through those CLIs, under your own subscription. Choosing **local Ollama** at its
  default local address keeps distillation and Ask fully on-device. Pointing its
  custom server URL at a **remote** host (even one you control, like a VPS or a
  homelab box) transmits conversation content off the local machine; it stays
  within your own infrastructure, but it is no longer on-device. **Ollama Cloud**
  (ollama.com) is a cloud provider like any other: picking it sends conversation
  content to Ollama's hosted service, keyed by your Ollama API key. Indexing,
  search, and embeddings are **always local** regardless of which engine you pick.
- **Agents you connect.** The MCP server, the SessionStart and optional
  proactive-recall hooks, and the managed `AGENTS.md` / `CLAUDE.md` block hand
  excerpts of your history (search results, threads an agent asks for, project
  memory), redacted the same way, to that agent, which sends them to its own
  provider along with the rest of its context. Callimachus makes no network request for this; it happens
  inside the agent you installed.
- **The update check.** At launch and periodically while it runs, the desktop app
  fetches the release manifest (`latest.json`) from GitHub Releases to see whether
  a newer version exists, and downloads the update only when you accept it.
  GitHub sees an ordinary download request (your IP address and the app's user
  agent), nothing about your data.
- **The embedding-model download.** The desktop app downloads the small
  embedding model (bge-small-en-v1.5, about 128 MB) from Hugging Face the first
  time you build the semantic index, and again at launch only if you have a
  semantic index but the model is missing from its cache
  (`<app data>/dev.shaller.callimachus/models`, or `$HF_HOME` /
  `$FASTEMBED_CACHE_DIR` when set). The `cal` CLI, the hooks and the MCP server
  never download it; without it they fall back to keyword search.
- **Opt-in usage analytics (desktop app only).** The desktop app can send
  anonymous usage analytics to PostHog (US): event names, app version, OS, CPU
  architecture and language, and a few properties picked from fixed lists (such
  as which agent source, AI provider or setting an event concerns, an error
  category, or a count rounded into a range). It never sends file paths, thread
  titles, prompts, transcripts, project names or API keys. It is **off until you
  choose Allow**, and you can turn it off any time in **Settings → General** or
  with `CALLIMACHUS_TELEMETRY=0` / `DO_NOT_TRACK=1`. Events are tied to a random
  install ID that is created when you opt in and deleted when you opt out. Every
  event tells PostHog not to build a person profile or look up a location, and
  the PostHog project is set to discard IP addresses instead of storing them.
  The analytics token is compiled into
  official release builds only, so builds from source never send anything. The
  **CLI and MCP server never send analytics.** The exact list of events and
  properties is in the [privacy policy](https://callimachus.app/privacy).

The website (callimachus.app) uses PostHog in cookieless mode: no cookies, no
browser storage, no cross-site tracking.

### Safeguards

- Every path that sends conversation content to an LLM provider or hands it to
  another agent redacts it on the way out: the in-app chat (including the
  history results and threads its tools fetch), distillation, Ask, project
  briefs and conflict review, every MCP tool result, the hooks and the managed
  memory block, the context Open in CLI passes to a new agent session, and
  Obsidian exports. Common secret formats (OpenAI, Anthropic and OpenRouter
  `sk-` keys, Stripe keys and webhook secrets, GitHub and Slack tokens, AWS
  access key IDs, bearer tokens, JWTs, private-key blocks, and
  `api_key=`/`password:`-style assignments; other keys, such as a bare Google
  `AIza...` key, are caught only in that assignment form) are replaced with
  `[REDACTED]` in transcripts, thread titles, search snippets, distilled facts
  and error signatures alike. The index itself keeps the original text. This is
  a best-effort safety net, not a guarantee: it matches known patterns, so a key
  in an unusual format can slip through. Treat any secret that appeared in a
  transcript as exposed to the providers you use. The `cal` query commands
  (`cal search`, `cal memory` and the like) redact their output the same way,
  human and `--json` alike, since agents run them too; pass `--raw` to print
  your history as stored in your own terminal (`cal cat` and the hooks ignore
  it). What you type into the chat yourself is not redacted.
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
