---
"callimachus": patch
---

**Codebase hardening (internal).** CI now gates Rust quality, not just tests: `cargo fmt --check` and `cargo clippy --lib --bins --tests -- -D warnings` run on every PR alongside the existing typecheck/build/test, so lint regressions and formatting drift can't land. Cleared the 8 pre-existing clippy warnings to make the gate green. Added the first tests over previously-untested entry points: the embedded migrations validate + apply cleanly on a fresh DB (catches a malformed/out-of-order migration in CI instead of on a user's first launch), the `cal` flag parser, and the MCP `search_threads` / `get_thread` tools end-to-end. No user-facing behavior change.
