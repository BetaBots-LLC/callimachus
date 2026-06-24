---
"callimachus": patch
---

**Spend Ledger: what your AI coding actually cost.** Callimachus now captures per-message token usage + model (from the source files' usage blocks, currently Claude Code's input/output/cache tokens) and turns it into dollars with a built-in pricing table. Because it's the only tool with a unified cross-tool index, it's the only place that can show total spend, a per-model breakdown, and your most expensive threads in one view. Surfaces:

- **Stats:** a "Spend" card (estimated total, by-model, priciest threads).
- **`cal cost [project]`:** the same as text/JSON.

The estimate uses published list prices, it's a cost x-ray, not a billing record, and calls on models with no price on file are flagged, not guessed. Token usage is captured during indexing into new `messages` columns (migration 0022); **run Reindex once** to backfill, since data indexed before this feature has no usage stored (the source files still carry it). All on-device.
