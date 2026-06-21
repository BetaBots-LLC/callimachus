---
"callimachus": patch
---

**Search results are now diversified across threads.** A single long thread with many matching messages could previously fill the entire result list and bury every other thread. Both keyword and hybrid search now cap how many message-hits any one thread contributes (3), so other threads surface. The keyword path over-fetches before capping so the freed slots back-fill with other threads rather than shrinking the list; the hybrid path applies the cap once on the fused output, leaving the pre-fusion per-thread signal intact. Per-thread depth is still one click away via opening the thread. Factored into a tested `cap_per_thread` helper.
