# Security & Privacy

## Privacy model

Callimachus is **local-first**. Your index, transcripts and memory never leave
your machine, except conversation content you explicitly send to an LLM provider
you choose and the excerpts that agents you connect pull into their own sessions.
Specifically:

- Your conversation index (`index.db`) lives only on your machine and is never
  uploaded anywhere. It keeps transcripts as they were written, secrets included,
  so on macOS and Linux its folder is created readable only by you (0700),
  `index.db` and its `-wal` and `-shm` files are made owner-only (0600) by the app
  and `cal` whenever they open it for writing, and `index.db.index.log` is created
  owner-only (an existing one keeps its mode). The lock files next to the index
  hold a process id and nothing else, and keep the default mode so that a lock
  another user left behind still reads as free; in the app's own data folder,
  which is tightened to 0700, nobody else can reach any of these. A folder you
  chose with `CALLIMACHUS_DB` keeps its own mode; read-only tools and hooks change
  nothing.
- Snapshots of the index (`snapshots/` next to it, taken on macOS before each
  schema upgrade and weekly, and by `cal backup`) hold the same unredacted text:
  their folders are 0700 and their files 0600. They stay on your machine; a copy
  you make with `cal backup --to DIR` is as private as the folder you put it in.
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
  active in the last 30 days, up to the daily limit you set, 50 by default),
  which beside the thread also sends the project's current decisions and gotchas
  (up to 30 and 20, each cut to 200 characters: the ones distilled from other
  threads, the ones you or an agent recorded and the ones imported from the
  repo's memory files, never Claude Code auto memory) so the engine can tell
  what the session changed (`knowledge::distill_memory_context`);
  (3) **Ask your history / RAG** (Chat's Ask mode, `agent::answer`, also reachable
  as `cal ask` and the `ask_history` MCP tool); (4) the Project page's **Synthesize brief**,
  **Write memory file** and **Review conflicts** (`agent::project_brief`,
  `agent::find_conflicts`), which send the project's path and the notes already
  distilled from it; and (5) the **AI-summarized Obsidian export**
  (`agent::synthesize`, also `cal export --synthesize`), which sends the thread
  to the engine picked under **Settings → Export** (for the CLI, the
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
- **Agents you connect.** The MCP server, the SessionStart hooks (Claude Code,
  and Codex, Gemini CLI, GitHub Copilot and Cursor when you turn theirs on), the optional
  proactive-recall and error-playbook hooks, and the managed `AGENTS.md` /
  `CLAUDE.md` block hand excerpts of your history (search results, threads an
  agent asks for, project memory, the recorded fix of an error), redacted the
  same way, to that agent, which sends them to its own provider along with the
  rest of its context. The error-playbook hook (Claude Code PostToolUse and
  PostToolUseFailure on Bash, off by default) reads each shell command's output
  on your machine to match it against your recurring errors. Callimachus keeps
  none of that output and sends it nowhere; it writes only the normalized
  signatures of the errors it already answered, to a per-session file under
  `~/.callimachus/recall/` that `cal session-end` deletes when the session ends
  (and the hook prunes after 7 days), and it prints only a fix that a commit in
  the same repo made. Callimachus makes no network request for this; it happens
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
  `$FASTEMBED_CACHE_DIR` when set). The hooks, the MCP server and every `cal`
  command but one never download it; without it they fall back to keyword search.
  The exception is `cal embed`, which builds the semantic index on a machine
  without the app and fetches the same model the same way the first time
  (`cal embed --no-download` never does).
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

This list is enforced by tests, not only by review: a network client anywhere
except the LLM engines (`agent`), the model download (`embed`), the analytics
client and the updater fails the build (`egress_is_confined`, and
`app_egress_is_confined` for the app).

The website (callimachus.app) uses PostHog in cookieless mode: no cookies, no
browser storage, no cross-site tracking.

### Safeguards

- Every path that sends conversation content to an LLM provider or hands it to
  another agent redacts it on the way out: the in-app chat (including the
  history results and threads its tools fetch), distillation, Ask, project
  briefs and conflict review, every MCP tool result, the hooks and the managed
  memory block, the context Open in CLI passes to a new agent session, and
  Obsidian exports. Common secret formats (OpenAI, Anthropic and OpenRouter
  `sk-` keys, Stripe keys and webhook secrets, GitHub, GitLab and Slack tokens,
  AWS access key IDs, Google `AIza...` API keys, OAuth access tokens and client
  secrets, npm, Hugging Face, PostHog personal, Groq, xAI, Perplexity,
  Replicate, Supabase secret, DigitalOcean, Shopify, Linear and SendGrid keys,
  age secret keys, bearer tokens and `Basic` credentials, JWTs, private-key
  blocks including one cut off before its END line, the password in a URL such
  as `postgres://user:password@host` (even one holding an `@` or `#`),
  `api_key=`/`password:`/`AccountKey=`-style assignments and `--token <value>`-style
  command-line flags) are replaced with `[REDACTED]` in transcripts, thread titles,
  search snippets, distilled facts and error signatures alike. A key with
  zero-width or other invisible characters inside it, or partly spelled as JSON
  `\uXXXX` escapes, is still matched, and the whole original span is removed.
  The index itself keeps the original text. This is a best-effort safety net,
  not a guarantee: it matches known patterns, so a key in an unusual format can
  slip through. Treat any secret that appeared in a
  transcript as exposed to the providers you use. The `cal` query commands
  (`cal search`, `cal memory` and the like) redact their output the same way,
  human and `--json` alike, since agents run them too; pass `--raw` to print
  your history as stored in your own terminal (`cal cat` and the hooks ignore
  it). What you type into the chat yourself is not redacted.
- Text Callimachus injects into an agent's context or writes where a terminal
  shows it (project memory lines, the SessionStart session digest, the
  compaction prompt anchor, the managed AGENTS.md / CLAUDE.md block, packed
  transcripts and snapshots, MCP tool results, and the human output of the
  `cal` query commands such as `cal search` and `cal memory` unless you pass
  `--raw`) is stripped of terminal escape sequences, control characters,
  bidirectional overrides and invisible Unicode tag characters, so a stored
  transcript cannot restyle your terminal or hide instructions from you. JSON
  output (`--json`, MCP structured results) drops the bidirectional overrides
  and tag characters and escapes the control characters. The index keeps the
  text as recorded.
- The in-app agent's `run_shell` tool requires **explicit per-command approval**
  before anything executes.
- The Claude Code hooks Callimachus installs (SessionStart `cal hook`, PreCompact
  `cal snapshot-hook`, SessionEnd `cal session-end`, and the opt-in
  UserPromptSubmit `cal recall-now`) only read and write the local index. They
  never create the index, never upgrade its schema (that waits for the app or a
  `cal` command you run), and exit silently on any error. The PreCompact hook
  stores Claude Code's compaction summary, redacted, and prints nothing; the
  SessionEnd hook only queues the session for auto-distillation, which then
  sends it to the provider you chose, if you turned that on.
- The Codex and Gemini CLI memory hooks (Settings > Agents > Other agents, off
  until you turn them on per agent) are the same commands with `--for codex` /
  `--for gemini`, written into `~/.codex/hooks.json` and `~/.gemini/settings.json`
  next to your own hooks, and hold to the same rules: read the local index, never
  create or upgrade it, exit silently on any error. The Codex PreCompact hook only
  takes a local snapshot. The Codex hooks also read `~/.codex/config.toml` and
  Codex's plugin folder to see whether the Callimachus plugin for Codex is enabled
  and installed; while it is, each leaves a per-event claim file under
  `~/.callimachus/recall`, so that the app's hook and the plugin's act once, and
  those files are removed after 7 days. These agents also receive imported Claude
  Code memories, which they send to their own providers with the rest of their
  context.
- While the Callimachus plugin for Codex is enabled in `~/.codex/config.toml` and
  installed in Codex, the app removes its own `[mcp_servers.callimachus]` entry
  from that file without asking (at launch, and within a minute while it runs), so
  that one Callimachus server starts, and keeps a copy of the entry next to the
  index. Once the plugin is off, it puts that entry back, with any keys you had
  added to it, pointed at the current app. It edits nothing else in that file, and
  an entry you add back while the plugin is on stays.
- The GitHub Copilot and Cursor memory hooks (also off until you turn them on) are
  `--for copilot` / `--for cursor`, in their own file
  `~/.copilot/hooks/callimachus.json` (read by Copilot CLI and VS Code's Copilot
  sessions) and in `~/.cursor/hooks.json` next to your own hooks, under the same
  rules. From the payload they read only these fields: at session start `source`
  (new or resumed; Cursor sends none, which counts as a new session) and the
  working directory (Copilot's `cwd`, Cursor's first `workspace_roots` entry); at
  session end the session id (Copilot's `sessionId`, Cursor's `session_id`), to
  queue that session for auto-distill. Cursor's payload also carries your account
  email, the model and the transcript path, which they ignore. The Claude Code hooks
  that Cursor also runs from `~/.claude/settings.json` only check whether the
  payload has a `cursor_version` field, and exit without reading anything else when
  it does.
- The value ledger (`ledger.jsonl` next to the index, or `<name>.ledger.jsonl`
  for another index file) counts what the hooks and the MCP server did: one line
  per injection, recall hit or MCP tool call, with a timestamp and a short detail
  (the project path or the tool name). It is written by appending to a local file
  (owner-only permissions on macOS and Linux; on Windows it inherits the index
  folder's permissions), folded into the index by the desktop app for the Stats
  tab once each day is over, never read by telemetry and never sent anywhere.
- "Keep transcripts for a year" in the Stats tab edits one key,
  `cleanupPeriodDays`, in Claude Code's user `settings.json`, only after you
  confirm, through the same atomic merge as the hooks (every other setting is
  kept; an unparseable file is left alone), and offers to restore the value it
  replaced.
- Claude Code auto memory (`~/.claude/projects/*/memory/*.md`) is read by the
  desktop app only and redacted before it is stored in the local index, and the
  PreCompact hook's compaction summaries are stored the same way. Both then
  become project memory and leave the machine only along its paths: MCP results
  and recall to the agents you connected (which send them to their own
  providers), and the LLM features (a project brief, a conflict review, Ask) to
  the provider you picked. Imported Claude Code memory is left out of
  `.callimachus/memory.md` and the managed `AGENTS.md` / `CLAUDE.md` block.
  Turning the import off in Settings > Agents removes every imported fact.
- Repo memory (`.callimachus/memory/*.jsonl` in a checkout) is written only by
  your own publish action (Publish to repo in the app, after a preview, or the
  `cal memory publish` command you run, which lists what it wrote); Callimachus
  never writes, commits or pushes it on its own. Published
  text is redacted, and Claude Code auto memory is never published. The files
  leave the machine only when you commit and push them. Imported repo facts come
  from whoever can push to the repo, so they are never pinned, are marked
  "From the repo" in the app and `repo` in each injected line's provenance
  suffix, e.g. `(2026-10-03, repo)`, under a line that names the files, and
  `cal memory import --review` shows what an import would change without
  writing it. A committed line's `supersedes` list can take a local fact out of
  injected memory on import, but only a visible fact of the same kind in the
  same repo; one you pinned or edited is not replaced but marked disputed for
  you to settle, and a pair you undid is not linked again.
- Writes that take a fact out of injected memory are bounded, and each one
  shows in the Projects tab, where you can undo it. Replaced facts stay as
  history; nothing is deleted.
  - `flag_fact` (an agent marks a fact stale or wrong): at most 10 unreviewed
    agent flags hide facts in a project at a time, a fact you pinned or edited
    stays in memory (labelled flagged), and the MCP server reaches only facts of
    the repo it runs in (or another clone of it), whatever project the agent
    names.
  - `record_decision` / `record_gotcha` with `replaces`, and
    `cal remember --replaces`: a target you pinned or edited is not replaced but
    marked disputed; an agent gets 5 replacements per project in 24 hours, after
    which further ones are marked disputed too. Over MCP the target must be in
    the repo the server runs in.
  - The distiller's links: a session can only point at the existing items it
    was shown for that project (a closed list of references, never a free id),
    only for a reversal or disagreement the session plainly states, at most 3
    replacements, 3 disputes, 3 retirements and 5 confirmations per session. It
    never replaces or hides a fact you pinned or edited (it is marked disputed
    or stays in, labelled), and a link you undo is remembered, so re-distilling
    the session does not bring it back.
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
