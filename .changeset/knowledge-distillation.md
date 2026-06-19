---
"callimachus": minor
---

Knowledge layer (slice 2): opt-in LLM distillation — decisions, gotchas & summaries.

Turn a thread into a high-signal recap. Distillation is **off by default** and never sends anything until you turn it on and pick an engine:

- **Local-first:** run a local model via **Ollama** (nothing leaves your machine), or bring your own cloud API key.
- **Per-thread & on demand:** click **Distill** on a thread — no bulk job, no surprise spend. Decisions/gotchas/summary render right in the thread view.
- **Never stale:** re-indexing a thread whose messages changed automatically invalidates its distilled knowledge.

Surfaces: a **Knowledge** section in the desktop thread view + a Settings card to enable/choose the engine; `cal distill <id>` / `cal knowledge <id>`; and an MCP `get_thread_knowledge` tool so agents can pull a thread's recap instead of the whole transcript.

Built on the same `facts` table as the free TODO tier. Structured output uses a portable prompt-and-parse approach that works identically across Ollama and cloud providers. Cross-thread semantic recall of decisions/gotchas is wired for a follow-up (the `vec_facts` table ships here, unused).
