---
"callimachus": patch
---

**ADR-style decisions + a contradiction guard.** Decisions can now carry a rationale (the "why"), and there's an active guard that surfaces settled decisions on a topic *before* an agent re-litigates one. Call `check_decision` (MCP) or `cal check "<proposal>"` with what you're about to do; it returns the closest prior decisions, each with its rationale and a match score, held to a strict similarity floor so a false "you already decided X" stays rare. Record the why with `record_decision`'s new `rationale` field or `cal remember decision "<text>" --because "<why>"`. Turns the existing passive, post-hoc conflict review into a guardrail an agent (or you) can hit at decision time. Backed by a `rationale` column on facts (migration 0020).
