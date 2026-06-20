---
"callimachus": minor
---

**Coach: your history, surfaced proactively.** A new dashboard that turns the memory layer from something you query into something that tells you what it knows.

- **Coding heatmap.** A GitHub-style grid of the last 52 weeks that fills the width, with a hover tooltip per day. It counts your own activity (user/assistant messages, excluding subagent transcripts), backed by a new `messages.ts` index so it stays fast on large histories.
- **This week's digest.** The decisions and gotchas captured from your sessions in the last 7 days (LLM-distilled or agent-recorded), each clickable straight to its source thread.

Available from the new **Coach** tab (and the Cmd-K palette). The heatmap works without the Knowledge layer; the digest fills in once distillation has run.
