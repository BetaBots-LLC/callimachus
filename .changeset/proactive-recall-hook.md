---
"callimachus": patch
---

**Proactive recall: surface prior work before you redo it.** A new opt-in **Proactive recall** toggle (Settings, under Claude Code) wires up a `UserPromptSubmit` hook that, on every prompt, quietly checks whether you've already solved this in a past session and injects a short "you may have worked on this before" note into Claude's context, so the decision or gotcha gets reused instead of rediscovered. One switch configures the hook for you, no editing `settings.json`.

- **Opt-in, off by default:** it's a separate toggle from the base integration *because* it reads every prompt. Flip it on to enable, off to remove the hook cleanly. Enabling also ensures the `cal` CLI is installed so the hook resolves.
- **Silent + best-effort:** a fresh process per prompt that exits 0 with no output on a weak/no match, a missing index, or any error, so it never blocks or breaks the prompt.
- **Signal, not noise:** a strict similarity floor (well above the on-demand guard) and per-session dedup (`~/.callimachus/recall/<session>.json`) mean it only speaks up on a clearly relevant match and never repeats the same thread twice in a session. Scoped to the repo via the hook's `cwd`.
- **Reuses** the existing semantic `find_prior_work` over distilled decisions/gotchas, so it lights up once threads are distilled. All on-device.

Note: each prompt loads the embedding model in a fresh process, a sub-second pre-flight before the agent starts.
