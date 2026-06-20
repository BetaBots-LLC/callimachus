---
"callimachus": minor
---

**Memory curation + trust.** Now that Project Memory is auto-generated (and fed to agents via the MCP tool, CLI seeding, and the memory file), you can vet it.

- **Pin / edit / delete distilled facts** in the Projects view. Hover a decision, gotcha, or TODO for pin, edit (inline), and delete actions. Pinned facts rank first.
- **Curated facts survive re-distillation.** Pinning, editing, or deleting a fact takes it out of the LLM's hands: auto-distill and re-index keep your pinned/edited facts and never resurrect a deleted one (kept as a tombstone). Edited facts are re-embedded so cross-thread recall matches the new wording.
- **Conflict review.** A "Review conflicts" action asks the configured LLM which of a project's decisions contradict or supersede each other, and surfaces the pairs with a one-line reason and a quick hide action.
- Hidden facts are filtered out everywhere they surface: Project Memory, cross-thread recall, the per-thread knowledge panel, and open-TODO lists.
