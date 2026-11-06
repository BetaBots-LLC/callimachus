# Callimachus

Local index and search for AI coding-agent history. CONTRIBUTING.md covers prerequisites, setup
and PR checks; this file is what an agent needs on top of that.

## Repo map

- `apps/desktop/src-tauri/`: a Cargo workspace. The app crate (`callimachus_lib`) has one binary:
  `callimachus` (`src/main.rs`) is the GUI, and also acts as `cal` when argv0 is `cal` or its
  first argument is in `cli_core::COMMANDS` (how installed hooks call it), and as the MCP server
  with `--mcp`. The `standalone/` member (`-p callimachus-standalone`) holds `cal`
  (`standalone/src/cli.rs` -> `cli_core.rs`) and `callimachus-mcp` (`standalone/src/mcp.rs` ->
  `mcp_server.rs`), thin entry points for the release's standalone downloads; keep them out of
  the app crate, since `tauri build` bundles every app-crate bin (RELEASING.md). Adding an agent
  source: `src/indexer/README.md`.
- `apps/desktop/src/`: React frontend; Tauri commands go through typed wrappers in `src/lib/api.ts`.
- `apps/vscode/`: VS Code / Cursor extension; runs `cal ... --json` (`src/cal.ts`), a contract.
- `apps/web/`: callimachus.app (TanStack Start). The privacy policy is `src/content/legal.ts`.
- `packages/callimachus-mcp/`: npm launcher; downloads the release binary, checks `SHA256SUMS`.

## Commands

```bash
pnpm turbo run typecheck build lint                    # JS side, as CI runs it
node --test packages/callimachus-mcp/test/*.test.mjs   # npm launcher
cd apps/desktop/src-tauri
cargo fmt --check && cargo clippy --locked --workspace --lib --bins --tests -- -D warnings
cargo test --locked --workspace
cargo test -- --ignored real_claude_index --nocapture  # real data: run #[ignore] tests by name
```

- fmt, clippy (`-D warnings`) and tests block CI (`RUST_TOOLCHAIN` in ci.yml pins Rust). The
  crate reads `apps/desktop/dist` at compile time: build the frontend once or create the dir.
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
  only a raw `tauri dev` or an explicit `CALLIMACHUS_DB` opens the real index. Other writers: the MCP server, the PreCompact hook and the
  `cal` write commands (`star`, `tag`, `distill`, `done`, `remember`, `snapshot`, `commits`,
  `audit-pr`). `cal hook`, `memory`, `stats`, `search` and the other queries open read-only.
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

- Add `migrations/00NN_<name>.sql` with the next number (0032) and register it in
  `db/migrations.rs`. Never edit a committed migration (0001-0031): `rusqlite_migration` tracks
  only `user_version`, so an edited file never re-runs on an index that already applied it.
- An upgrade runs in whichever process opens the index first and holds the write lock, so time
  it on a large index (`db::migrate` pre-reads big files sequentially first).
- Pin query plans in tests with `EXPLAIN QUERY PLAN` (helpers in `db/migrations.rs`, `search/`).
- Never use `INDEXED BY` without a `db::has_index` fallback (see `search::fts_message_join`):
  read-only sidecars and pooled readers never migrate, and `INDEXED BY` a missing index is an
  error, not a slow plan.

## Tests that fail when docs and code drift

- `mcp_server::docs_list_every_tool`: each MCP tool is in backticks in `SKILL.md` and `README.md`.
- `mcp_server::tool_surface_stays_lean_and_annotated`: descriptions <= 220 chars (4,500 total),
  instructions <= 400, annotations on every tool; add a new write tool to its `writes`.
- `cli_core::docs_only_reference_real_commands`: each `cal <cmd>` in `README.md` and
  `apps/web/src/routes/{cli,index,desktop}.tsx` is in `COMMANDS` (`USAGE` must list it too).
- `integration::every_shipped_skill_is_recognized`: an edit to `resources/recall/SKILL.md` needs
  its printed hash in `SHIPPED_SKILL_HASHES`.
- `command_surface_tests` (lib.rs): `generate_handler!` matches the `invoke` calls in `api.ts`.
- `telemetry::privacy_policy_names_every_event` (reads `legal.ts`),
  `sidecar_entry_points_never_touch_telemetry`, and in `indexer/mod.rs`
  `the_source_registry_matches_the_schema_and_telemetry`, `caps_tool_output_matches_the_indexers`.

## Hooks and memory budgets

Every surface that injects project memory renders through `export::agent_memory_md_budgeted`
with an `export::MemoryBudget`: `HOOK` (SessionStart startup / clear, under Claude Code's 10k-char
hook output cap), `COMPACT` (after compaction, snapshot carry), `AGENTS_FILE` (the managed
AGENTS.md / CLAUDE.md block) and `MCP` (`project_memory`). Change sizes there, not at call sites.
MCP transcript tools take a `budget` (`TRANSCRIPT_BUDGET_*` in `mcp_server.rs`, default 24k).
Hook events, matchers and timeouts are the `HookSpec` consts in `integration.rs`. Hook commands
(`hook`, `snapshot-hook`, `recall-now`) exit 0 silently on any error and never create an index;
`cal`, hooks and the MCP server never download the model (`embed::Embedder::cached_only`). Text
that leaves the machine (LLM prompts, MCP results, exports) goes through `context::redact_secrets`.

## Telemetry

GUI only and opt-in (rules in the `telemetry.rs` header). Events are a closed `Event` enum; each
string property comes from an allowlist mirrored in `apps/desktop/src/lib/telemetry.ts`. A new
event or property also updates `legal.ts` (bump `LEGAL_UPDATED`) and `SECURITY.md` in the same
PR. Never reference telemetry from `cli_core.rs`, `mcp_server.rs`, `standalone/src/cli.rs`,
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
