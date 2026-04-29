## 🔍 Critic Response (Round 3/5)

<details>
<summary>Full critique (click to expand)</summary>

Findings:

1. The proposal needs an explicit scan bound. With skipped candidates budget-neutral and henyey reading from the main queue rather than stellar-core’s mutable flood queue, an all-skipped queue can be scanned from the top every flush forever. A test documenting “extra scanning” is not enough; define a cap or a persistent broadcast frontier/history strategy.

2. Clarify DEX-vs-generic precedence. In stellar-core, a DEX tx that exceeds both DEX lane budget and generic budget should drop/deactivate the DEX lane and continue with non-DEX candidates, because lane-limit failure wins for non-generic lanes. The current wording also says “any candidate exceeds remaining generic budget, stop,” which could be implemented incorrectly.

3. Specify the app locking/API shape. `flush_tx_adverts()` cannot `await` inside a synchronous queue visitor, so the plan should explicitly acquire/prune `tx_adverts_by_peer` before traversal and pass mutable state into a synchronous planning closure/helper, with no async work inside the visitor.

4. Add tests for the ambiguous/high-risk cases: repeated all-skipped traversal boundedness, DEX candidate exceeding both DEX and generic budgets, exact carry-over after skipped candidates and lane deactivation, and `dex_ops_remaining: None` not updating/staling DEX carry-over behavior.

Overall, the proposal is directionally correct and fixes the main budget-before-filtering bug, but it underspecifies the most important operational risk created by budget-neutral skips in henyey’s non-mutating queue design.

VERDICT: REVISE

</details>

**Verdict: REVISE**

1. Add an explicit scan bound or persistent broadcast frontier/history strategy.
2. Clarify DEX-vs-generic precedence when a DEX candidate exceeds both budgets.
3. Specify that app advert planning is synchronous inside the visitor, with locks acquired before traversal and no `await` in the closure.
4. Add tests for bounded all-skipped traversal, DEX exceeding both budgets, exact carry-over after skips/lane deactivation, and `dex_ops_remaining: None`.
