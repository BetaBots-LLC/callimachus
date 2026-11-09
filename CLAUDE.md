# Callimachus

Local index and search for AI coding-agent history. CONTRIBUTING.md covers prerequisites, setup
and PR checks; this file is what an agent needs on top of that.

## Repo map

- `apps/desktop/src-tauri/`: a Cargo workspace of three crates.
  - `core/` (`callimachus-core`): everything without Tauri: db, indexer (with the watcher loop),
    search, embed, knowledge, export, context, the integrations, `cli_core` (cal),
    `mcp_server`, `jobs` (index / embed / distill / compact / harvest orchestration behind a
    `JobHost: EventSink` and `JobFlags`), `headless` (`cal index` / `watch` / `embed` /
    `distill --auto`) and `locks` (`<index>.gui.lock` / `<index>.watch.lock` / `<index>.distill.lock`). It cannot name
    telemetry.
    Its build.rs stamps the app crate's version as `CALLIMACHUS_VERSION` (core stays 0.0.0).
    The graph tables from 0034 (`facts.uid` / `source_seq`, `projects`, `file_edits`,
    `issue_clusters`) and the fact truth tables from 0035 (`fact_links`: supersedes /
    contradicts between facts; `fact_signals`: reaffirmed / stale / wrong; read and written
    only through `knowledge::truth`) and their functions are documented at the top of
    `core/src/lib.rs`. The
    watcher keeps them current (`jobs::CommitLinker` after each batch, the off-writer
    `issue_clusters_kick` when the index changed since the stored refresh, and after its launch
    catch-up the one-time `file_edits` backfill that 0034 flags in `app_config`); `cal index`
    runs the same upkeep after its pass (`headless::graph_upkeep`), since a plugin-only machine
    has no watcher.
  - The app crate (`src/`, `callimachus_lib`): window, tray, Tauri commands in `lib.rs` (thin
    over `core::jobs`; `TauriHost` emits through Tauri's `Emitter`) and `telemetry.rs`. It
    re-exports core's modules, so `crate::db` etc. resolve. One binary: `callimachus`
    (`src/main.rs`) is the GUI, and also acts as `cal` when argv0 is `cal` or its first
    argument is in `cli_core::COMMANDS` (how installed hooks call it), and as the MCP server
    with `--mcp`.
  - `standalone/` (`-p callimachus-standalone`): `cal` (`standalone/src/cli.rs` ->
    `cli_core.rs`) and `callimachus-mcp` (`standalone/src/mcp.rs` -> `mcp_server.rs`), thin
    entry points for the release's standalone downloads, linking core only (no tauri in
    `cargo tree -p callimachus-standalone -e normal`); keep them out of the app crate, since
    `tauri build` bundles every app-crate bin (RELEASING.md).
  Adding an agent source: `core/src/indexer/README.md`.
- `apps/desktop/src/`: React frontend; Tauri commands go through typed wrappers in `src/lib/api.ts`.
- `apps/vscode/`: VS Code / Cursor extension; runs `cal ... --json` with `CAL_JSON_VERSION=1`
  (`src/cal.ts`), a contract.
- `apps/web/`: callimachus.app (TanStack Start). The privacy policy is `src/content/legal.ts`.
- `schemas/cal/`, `schemas/mcp/`: the checked-in JSON Schemas of the versioned `cal --json-version 1`
  outputs and of each MCP tool's `output_schema` (copies under `apps/web/public/schemas/`).
- `packages/callimachus-mcp/`: npm launcher; downloads the release binary, checks `SHA256SUMS`.
- `plugins/claude-code/`: the Claude Code plugin (catalog: `.claude-plugin/marketplace.json`).
  While it is enabled the app installs only `cal` (`integration::claude_plugin_managed`).
- `plugins/cursor/`: the Cursor plugin (catalog: `.cursor-plugin/marketplace.json`, which
  cursor.directory's parser also reads). Its hooks and the app's Cursor hooks can both run;
  `cal hook --for cursor` claims each conversation once (`claim_cursor_injection`).

## Commands

```bash
pnpm turbo run typecheck build lint                    # JS side, as CI runs it
node --test packages/callimachus-mcp/test/*.test.mjs   # npm launcher
node --test plugins/test/*.test.mjs                    # Claude Code plugin
cd apps/desktop/src-tauri
cargo fmt --check && cargo clippy --locked --workspace --lib --bins --tests -- -D warnings
cargo test --locked --workspace
cargo test -p callimachus-core -- --ignored real_claude_index --nocapture  # #[ignore] by name
```

- fmt, clippy (`-D warnings`) and tests block CI (`RUST_TOOLCHAIN` in ci.yml pins Rust). All
  three crates are default members, so a bare `cargo test` here also covers every crate; the
  `#[ignore]` tests all live in core. The app crate reads `apps/desktop/dist` at compile time:
  build the frontend once or create the dir (core and standalone need no dist).
- Biome (`pnpm check`) is advisory in CI. Its pre-existing errors are mostly the shadcn files in
  `apps/desktop/src/components/ui/`, plus a few in `apps/vscode/src/webview/`; add no new ones.
  `apps/web` runs Biome as its `lint` script, so there it blocks.
- `#[ignore]` tests use your real agent history (into temp DBs), the OS keychain, the `claude`
  CLI or a ~128 MB model download.
- Browser-only UI work: `pnpm --filter callimachus dev` then http://localhost:1420/mock.html?view=settings&section=general (src/dev/mock-tauri.ts fakes every Tauri command with scenario URL params; never shipped).

## Data safety

The dev build and the installed app share one index,
`~/Library/Application Support/dev.shaller.callimachus/index.db` on macOS (`%LOCALAPPDATA%\dev.shaller.callimachus` on Windows, `~/.local/share/dev.shaller.callimachus` on Linux; `db::default_index_path`, overridable with `CALLIMACHUS_DB`),
which holds the user's whole history and their Settings (`app_config`).

- Opening it writable runs pending migrations, after which the older installed app can no longer
  open it (0.11.0 then crashed at launch; 0.11.1+ shows an error screen). `pnpm desktop:dev`
  therefore points `CALLIMACHUS_DB` at `index.dev.db` next to it (`scripts/desktop-dev.mjs`);
  only a raw `tauri dev` or an explicit `CALLIMACHUS_DB` opens the real index. Other writers:
  the MCP server, the `cal` write commands (`star`, `tag`, `distill`, `done`, `remember`,
  `snapshot`, `commits`, `audit-pr`, `memory import` without `--review`, and the headless jobs
  `index`, `watch`, `embed`, `distill --auto`), and the PreCompact and SessionEnd hooks, which
  open with `db::open_sidecar_current` and never migrate (they skip until the app or a `cal`
  command has upgraded the index). `cal hook`, `memory`, `memory import --review`, `stats`,
  `search` and the other queries open read-only; `cal memory publish` reads the index
  read-only but writes `.callimachus/memory/` into the checkout it runs in, so never run it in
  this repo to test it.
- A dev app launch also runs `integration::refresh_installed`, which can rewrite the installed
  skill and re-point dangling hooks, MCP entries and the `cal` link at the dev binary.
- Smoke-test built binaries on an APFS clone with a throwaway HOME (never `cargo` or `pnpm`
  themselves: rustup and the pnpm store live under HOME). The app binary answers cal subcommands;
  `target/debug/cal` comes only from `cargo build -p callimachus-standalone` and may be stale:

```bash
D="$HOME/Library/Application Support/dev.shaller.callimachus"; S=/tmp/cal-smoke
mkdir -p "$S/home" && cp -c "$D"/index.db* "$S"/
env -u CLAUDE_CONFIG_DIR -u CODEX_HOME HOME="$S/home" CALLIMACHUS_DB="$S/index.db" \
  FASTEMBED_CACHE_DIR="$D/models" apps/desktop/src-tauri/target/debug/callimachus stats
```

## Migrations

- Add `src-tauri/migrations/00NN_<name>.sql` (the directory stays there, not under `core/`)
  with the next number (0036) and register it in `core/src/db/migrations.rs`. Never edit a
  committed migration (0001-0035): `rusqlite_migration` tracks only `user_version`, so an
  edited file never re-runs on an index that already applied it.
- An upgrade runs in whichever process opens the index first and holds the write lock, so time
  it on a large index (`db::migrate` pre-reads big files sequentially first, unless every pending
  migration is in `SCAN_FREE_MIGRATIONS`: list one there only when it touches no large table).
- Pin query plans in tests with `EXPLAIN QUERY PLAN` (helpers in `core/src/db/migrations.rs`,
  `core/src/search/`).
- Never use `INDEXED BY` without a `db::has_index` fallback (see `search::fts_message_join`):
  read-only sidecars and pooled readers never migrate, and `INDEXED BY` a missing index is an
  error, not a slow plan.

## Tests that fail when docs and code drift

- `mcp_server::docs_list_every_tool`: each MCP tool is in backticks in `SKILL.md` and `README.md`.
- `mcp_server::tool_surface_stays_lean_and_annotated`: descriptions <= 220 chars (4,500 total),
  instructions <= 400, annotations on every tool; add a new write tool to its `writes`.
- `cli_core::docs_only_reference_real_commands`: each `cal <cmd>` in `README.md` and
  `apps/web/src/routes/{cli,index,desktop,mcp}.tsx` and `apps/web/src/content/faq.ts` is in
  `COMMANDS` (`USAGE` must list it too); the test fails when one of those files is missing.
- `integration::every_shipped_skill_is_recognized`: an edit to `resources/recall/SKILL.md` needs
  its printed hash in `SHIPPED_SKILL_HASHES`.
- `integration::claude_plugin_mirrors_the_app_install`: the plugin's skill is a byte copy of
  that file, its `hooks/hooks.json` matches the base `HookSpec`s and the app's version, and
  `plugins/claude-code/mcp/` is a byte copy of `packages/callimachus-mcp`'s `bin/` and `lib/`.
- `integration::cursor_plugin_mirrors_the_app_install`: plugins/cursor's hooks match the
  `CURSOR_*` HookSpecs, its scripts probe `cal help` for a line ending ` or cursor` (keep that
  in `USAGE`), and its version and `npx` pin are the release; `plugins/test/cursor-plugin.test.mjs`
  also replays cursor.directory's parser over the repo.
- `command_surface_tests` (lib.rs): `generate_handler!` matches the `invoke` calls in `api.ts`.
- `telemetry::privacy_policy_names_every_event` (reads `legal.ts`),
  `sidecar_entry_points_never_touch_telemetry`, `every_source_is_on_the_telemetry_allowlist`, and
  in `core/src/indexer/mod.rs` `the_source_registry_matches_the_schema_and_telemetry` (its
  telemetry half is the allowlist test, since core cannot see telemetry) and
  `caps_tool_output_matches_the_indexers`.
- `cli_contract::cal_json_schemas_match_the_checked_in_files` and
  `mcp_server::mcp_output_schemas_match_the_checked_in_files` rewrite `schemas/cal/*.v1.json`,
  `schemas/mcp/<tool>.json` and their copies under `apps/web/public/schemas/` when a shape
  changes (commit them); `mcp_server::every_tool_answer_matches_its_output_schema` checks every
  tool's structured content against its `output_schema`. Plain `--json` stays the bare,
  unversioned shape the released VS Code extension reads; only `--json-version 1` /
  `CAL_JSON_VERSION=1` gives the envelopes.

## Hooks and memory budgets

Every surface that injects project memory renders through `export::agent_memory_md_budgeted`
with an `export::MemoryBudget`: `HOOK` (SessionStart startup / clear, under Claude Code's 10k-char
hook output cap), `COMPACT` (after compaction, snapshot carry), `AGENTS_FILE` (the managed
AGENTS.md / CLAUDE.md block) and `MCP` (`project_memory`). Change sizes there, not at call sites.
MCP transcript tools take a `budget` (`TRANSCRIPT_BUDGET_*` in `mcp_server.rs`, default 24k).
Hook events, matchers and timeouts are the `HookSpec` consts in `integration.rs` (the plugin's
own SessionStart guard is `PLUGIN_ONLY_HOOKS` in its tests; GitHub Copilot and Cursor take flat
entries, `HookLayout::Flat`). Cursor also runs the hooks in `~/.claude/settings.json`, so every
Claude form (`hook`, `session-end`, `recall-now`, `recall-error`, `snapshot-hook`) stays silent
on a payload with `cursor_version` (`cli_core::is_cursor_payload`). Hook commands (`hook`, `recall-error`,
`snapshot-hook`, `session-end`, `recall-now`, `index --if-stale`) exit 0 silently on any error,
never create an index and never migrate one themselves (`index --if-stale` starts a detached
`cal index` only on a non-empty index; that pass upgrades the schema only when it runs from
the npm launcher's `cal` with no app running, else it logs why to `index.db.index.log`);
`cal`, hooks and the MCP server never download the model (`embed::Embedder::cached_only`); the
one exception is `cal embed` without `--no-download`. `cal watch` refuses while the app holds
`index.db.gui.lock`, the app's watcher waits while `cal watch` holds `index.db.watch.lock`, and
nobody starts an auto-distill round while `index.db.distill.lock` is held, and two `cal index`
passes never overlap (`index.db.index.lock`) (`locks`, one set of
lock files per index file). Text that leaves the machine (LLM prompts, MCP results, exports)
goes through `context::redact_secrets`.

## Telemetry

GUI only and opt-in (rules in the `telemetry.rs` header). Events are a closed `Event` enum; each
string property comes from an allowlist mirrored in `apps/desktop/src/lib/telemetry.ts`. A new
event or property also updates `legal.ts` (bump `LEGAL_UPDATED`) and `SECURITY.md` in the same
PR. Never reference telemetry from the core crate (it cannot name it; the jobs reach it
through `JobHost::index_completed` / `distill_completed`), `standalone/src/cli.rs`,
`standalone/src/mcp.rs` or `main.rs`. Only the release workflow compiles in the PostHog token (`option_env!`, `build.yml`).

## Releases and changesets

Release flow: RELEASING.md. `pnpm version-packages` syncs versions, so never hand-edit them in
`Cargo.toml`, `tauri.conf.json` or `server.json`. A user-facing change gets a `patch` changeset
unless told otherwise. `callimachus`, `callimachus-vscode` and `callimachus-mcp` are one fixed
group; `callimachus-web` is ignored, so note site or policy changes in the `callimachus` one.

## Conventions

- Tauri commands that touch SQLite, files, git, the keychain or the model are `async` and hand
  the work to `blocking` / `read_blocking` (lib.rs); a sync command runs on the main thread and
  freezes the window. GUI diagnostics use `app_log!` (`callimachus.log` next to the index).
- Comments say why: the constraint, the measured number, the bug that motivated the code.
- Tests sit next to the code in `#[cfg(test)] mod tests`, on temp-file DBs opened with
  `db::open`. Policy code takes injected probes (`is_macos_ephemeral_with`, `pick_cache_dir`).
- No em-dashes in prose (docs, changesets, comments, UI copy). Conventional Commits.
