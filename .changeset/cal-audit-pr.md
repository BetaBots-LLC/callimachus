---
"callimachus": minor
---

**`cal audit-pr`: a one-call PR-audit context bundle for external tools.** A local PR-audit app (or any reviewer) can now shell out to Callimachus and get, in one JSON object, the history behind a change that a diff alone can't show:

```
cal audit-pr <repo> --changed-files src/auth.rs,src/lib.rs --shas <sha1>,<sha2>
```

The bundle:
- **`bySha`** , commit provenance: for each branch SHA, the AI session(s) inferred to have produced it (by file-overlap) with that thread's distilled summary/decisions/gotchas. "Commit abc came from the session where you decided JWT because X, then flagged an offline-refresh gotcha." SHAs with no link return an explicit empty array, never omitted.
- **`byFile`** , for each changed file, prior threads that touched it + their reasoning.
- **`recurringErrors`** , the repo's cross-tool recurring error signatures (the caller intersects with the touched-thread set client-side; there's no error→file edge).
- **`projectMemory`** , the repo's distilled decisions / gotchas / open TODOs as a standing pre-merge checklist.

It refreshes thread↔commit links first (so feed branch SHAs pre-squash), and degrades gracefully: with distillation off, knowledge fields are null but provenance, file-touch, open TODOs, and repo errors still populate. Reuses the existing gitlink / search / knowledge / issues primitives; the only net-new query is `commits_by_sha` (the inverse of `linked_commits`), backed by a new `thread_commits(sha)` index (migration 0023). The interactive deep-dive (per-hunk `check_decision`, `get_thread`, snapshots) stays on the existing MCP server.
