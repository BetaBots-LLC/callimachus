# Contributing to Callimachus

Thanks for your interest. Issues, feature ideas, and PRs are all welcome.

## Prerequisites

- **macOS, Windows or Linux.** Everything builds and runs on all three. "Open in
  CLI" / "Resume" open Terminal.app on macOS and a new console window on Windows.
  On Linux they use `$TERMINAL` or the first terminal emulator found on `PATH`
  (x-terminal-emulator, GNOME Terminal, Konsole, xfce4-terminal, kitty,
  Alacritty, WezTerm, xterm). To build on Linux, install the WebKitGTK build deps
  first ([Tauri's prerequisites](https://v2.tauri.app/start/prerequisites/#linux)),
  e.g. on Ubuntu 24.04+ / Debian 13+:
  ```bash
  sudo apt-get install -y libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev \
    build-essential pkg-config libssl-dev
  ```
  The ONNX Runtime prebuilt needs glibc 2.38 or newer, so older distros (Ubuntu
  22.04, Debian 12) can't build or run it.
- **Node 22+** and **pnpm** (`corepack enable` or `npm i -g pnpm`)
- **Rust** via [rustup](https://rustup.rs). CI pins a specific stable release
  (`RUST_TOOLCHAIN` in `.github/workflows/ci.yml`); use that or newer.

## Setup

```bash
git clone <repo-url> callimachus && cd callimachus
pnpm install
pnpm desktop:dev        # launches the desktop window (tauri dev)
```

`pnpm desktop:dev` uses the same index as an installed Callimachus
(`dev.shaller.callimachus/index.db` in your app-data folder) and runs any new
migrations on it. Once a dev build has upgraded that index, an older installed
release, including its MCP server and hooks, refuses to open it ("This index is
from a newer Callimachus") until you update the app. To keep your real index out
of it, give the dev build its own file:

```bash
CALLIMACHUS_DB=/tmp/callimachus-dev/index.db pnpm desktop:dev
```

This is also how to try a clean first run. The embedding model cache stays
shared, so the model isn't downloaded again. On an empty index the app opens on
onboarding: choose **Index my history**, then **Build semantic index** for hybrid
search.

## Repo layout

This is a [Turborepo](https://turborepo.com) + pnpm workspace.

```
apps/desktop/              Tauri 2 app + the `cal` CLI and `callimachus-mcp` server (src-tauri)
apps/vscode/               VS Code extension (shells out to `cal`)
apps/web/                  marketing + download site (TanStack Start)
packages/callimachus-mcp/  npm launcher for the MCP server (`npx -y callimachus-mcp`)
scripts/                   version-sync, release tagging
.changeset/                versioning + changelog
```

## Checks before a PR

```bash
pnpm check                             # Biome lint + format check
pnpm typecheck                         # all apps (tsc)
pnpm build                             # all apps; also creates apps/desktop/dist, which the Rust crate needs to compile
node --test packages/callimachus-mcp/test/*.test.mjs   # npm launcher tests
node --test plugins/test/*.test.mjs                    # Claude Code plugin manifests
cd apps/desktop/src-tauri
cargo fmt --check
cargo clippy --workspace --locked --lib --bins --tests -- -D warnings
cargo test --workspace --locked        # Rust unit tests, plus the drift guards below
```

CI runs the same checks (on macOS; the repo-wide Biome job runs on its own, see
below), plus `pnpm lint`, `vsce ls` on the extension manifest,
`cargo check --locked --workspace --all-targets` on Linux and Windows (Windows also links
the standalone `cal.exe` and checks it needs no VC runtime DLL), and a debug
`tauri build --no-bundle`. That build fails when a tauri crate and its
`@tauri-apps/*` npm package drift apart by a minor version, so after bumping
either side run `pnpm --filter callimachus tauri build --debug --no-bundle`
locally. Rust builds use `--locked`, so commit `Cargo.lock` with any dependency
change.

The repo-wide Biome job is advisory for now (there are existing errors to
clear), but please don't add new ones. `apps/web` is already clean, and its
`lint` script (`biome check .`) runs in the blocking check job, so a Biome error
there fails CI.

`cargo test -- --ignored` runs every ignored test: real-data smoke tests that
index your own history into a temporary database (your history is only read),
embedding tests that download the model on first run, a keychain round-trip that
writes and then deletes a test entry in your OS credential store, and live calls
through your logged-in `claude` CLI. To run one, name it, e.g.
`cargo test -- --ignored real_claude_index`.

### Drift guards

`cargo test` also checks that code and docs agree, so some changes need a
matching edit elsewhere:

- **`cal` subcommand:** document it in `USAGE` (`cal help`) in `cli_core.rs`.
  Every `cal <command>` in `README.md` and the website's `cli`, `index` and
  `desktop` routes must be a real command.
- **MCP tool:** give it annotations (`readOnlyHint`; write tools also
  `destructiveHint: false`) and a description of at most 220 characters (4,500
  for all tools together; the server instructions stay under 400). Mention it in
  backticks in both `README.md` and `resources/recall/SKILL.md`.
- **`resources/recall/SKILL.md`:** any change needs the new text's hash added to
  `SHIPPED_SKILL_HASHES` in `integration.rs`. The failing test prints it.
- **Tauri command:** add a typed wrapper in `apps/desktop/src/lib/api.ts` (or
  `telemetry.ts`). Every wrapper must name a registered command.
- **Analytics event:** add the `Event` variant, its props and parser in
  `telemetry.rs` (and to the `all` list in `privacy_policy_names_every_event`),
  the mirror in `apps/desktop/src/lib/telemetry.ts`, and its wire name in the
  privacy policy's event list (`apps/web/src/content/legal.ts`). The CLI, hooks
  and MCP server (`cli_core.rs`, `mcp_server.rs`, `cli.rs`, `mcp.rs`, `main.rs`)
  must never reference telemetry.
- **Indexed source:** see the [indexer README](apps/desktop/src-tauri/src/indexer/README.md).
  The registry is checked against the `sources` table, `telemetry::SOURCES` and
  each indexer's tool-output capping.

## Adding support for another agent

Each indexed source is a small, documented contract, usually one indexer module
+ a migration that seeds its `sources` row + a few wiring points (its `SOURCES`
registry entry, a watcher target, the telemetry allowlist, frontend labels). Start
at [`apps/desktop/src-tauri/src/indexer/README.md`](apps/desktop/src-tauri/src/indexer/README.md).

## Conventions

- **Commits:** [Conventional Commits](https://www.conventionalcommits.org)
  (`feat:`, `fix:`, `chore:`, `docs:`, `refactor:`, `test:`).
- **Changesets:** if your change is user-facing, run `pnpm changeset` (patch by
  default) and commit the generated file so it lands in the changelog / version
  bump.
- **Privacy:** if a change adds network traffic, adds a feature that sends
  conversation content to an AI provider or another agent, or changes what
  analytics send, update the privacy policy (`apps/web/src/content/legal.ts`,
  bump `LEGAL_UPDATED`), `SECURITY.md`, the README's Security & privacy section
  and the website FAQ (`apps/web/src/content/faq.ts`) in the same PR, and say so
  in the changeset. Any index text that leaves the machine (to an AI provider,
  over MCP, through a hook, or into a written file) goes through
  `context::pack_thread` or `context::redact_secrets` first. A new analytics
  event needs its variant, props and parser in `telemetry.rs`, a mirror in
  `apps/desktop/src/lib/telemetry.ts`, and its wire name in the policy's event
  list (`cargo test` fails until it is there).
- **Migrations:** add a new `migrations/00NN_*.sql` with the next unused number
  and register it at the end of `MIGRATIONS` in `db/migrations.rs`. Never edit,
  renumber or reorder one that has shipped, or that you have already run against
  a real index. The schema version is only a count of applied migrations, so an
  existing index never re-runs an edited file. Add an upgrade test that starts
  from `db_at_version(<previous>)`. When a migration adds or drops an index, also
  add an `EXPLAIN QUERY PLAN` test for the queries that rely on it: the planner
  has no ANALYZE stats, so plans follow the schema alone. Read-only connections
  (`cal` queries, the app's read pool) never migrate, so code must still work on
  the previous schema. A query that names an index with `INDEXED BY` checks
  `db::has_index` first (see `search::fts_message_join`).
- **Style:** functional and concise; validate at system boundaries; don't add
  comments or type annotations to unchanged code. Run `pnpm check` (Biome lint +
  format, or `pnpm check:fix` to apply) before opening a PR.
- **Releases:** maintainers cut releases via the flow in [RELEASING.md](RELEASING.md).

## License of contributions

Callimachus is **dual-licensed**: [AGPL-3.0-or-later](LICENSE) for everyone, plus
commercial licenses sold by the copyright holder (see [COMMERCIAL.md](COMMERCIAL.md)).
For that to work, the maintainer must be able to relicense the whole codebase.

**By submitting a contribution you agree that:**

1. Your contribution is licensed under **AGPL-3.0-or-later**, and
2. You grant **Ari Shaller** (the copyright holder) a perpetual, worldwide,
   non-exclusive, royalty-free right to **also license your contribution under other
   terms**, including the commercial license (so it can ship in commercial builds), and
3. You have the right to grant this (the work is yours or you're authorized).

If you can't agree to this, please open an issue instead of sending code.
