---
"callimachus": patch
---

**Hybrid search now weights fusion by semantic similarity, not just rank.** Reciprocal Rank Fusion previously gave every semantic match the same `1/(K+rank)` weight, so a marginal 0.4-cosine match counted as much as a strong 0.9 one at the same position. The semantic arm's RRF contribution is now scaled by a similarity factor over the `[0.35, 1.0]` retained range (`0.5×` at the floor, full weight at the top), so strong matches outrank marginal ones. The keyword (BM25) arm is untouched and the factor never exceeds 1.0, so the tuned keyword/semantic balance can't blow out, it only ever demotes weak semantic hits. Fusion is factored into testable `fuse_rrf` / `sem_weight` helpers with unit tests locking the behavior.
