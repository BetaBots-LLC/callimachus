---
"callimachus": patch
---

fix: correct Anthropic model pricing in the spend estimate and improve currency
formatting. Prices Claude Fable/Mythos usage at their $10/$50-per-Mtok list rates
(previously unmatched and counted as untracked) and updates the stale Opus
($15/$75 → $5/$25) and Haiku ($0.80/$4 → $1/$5) rates. The Spend panel now formats
amounts with thousands separators and shows "<$0.01" for tiny-but-nonzero estimates
instead of collapsing them to "$0.00".
