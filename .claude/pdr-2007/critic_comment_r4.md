## 🔍 Critic Response (Round 4/5)

<details>
<summary>Full review report (click to expand)</summary>

1. The scan cap can cause indefinite starvation. If more than `max(budgets)+slack` high-priority candidates are already seen by all active peers, traversal will hit the same skipped prefix every flood tick and never reach lower-priority unseen txs until those higher txs leave the queue. That undermines the issue’s core goal. Either remove the cap for parity, or add a continuation/aging mechanism and tests proving an unseen candidate beyond the cap is eventually advertised.

2. The plan should explicitly preserve the carry-over cap. `BroadcastBudget` may report raw remaining ops, but app storage still needs `min(MAX_CARRYOVER_OPS)` for generic and configured DEX carry-over, matching current code and stellar-core.

3. Add a regression for “all skipped prefix larger than scan cap, later unseen candidate” if the cap remains. The current proposed “skip allowing a later candidate” test only proves the within-cap case.

VERDICT: REVISE

</details>

**Verdict: REVISE**

1. Remove the scan cap or add continuation/aging to avoid indefinite starvation behind an already-seen high-priority prefix.
2. Preserve the app carry-over cap with `min(MAX_CARRYOVER_OPS)`.
3. If any cap remains, test the beyond-cap unseen-candidate case.
