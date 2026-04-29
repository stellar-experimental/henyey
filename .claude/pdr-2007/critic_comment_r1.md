## 🔍 Critic Response (Round 1/5)

<details>
<summary>Full critique (click to expand)</summary>

The proposal’s core diagnosis is correct. `TransactionQueue::broadcast_some()` currently decrements generic/DEX budgets before `App::flush_tx_adverts()` knows whether any peer will receive the hash, and stellar-core’s `broadcastSome()` does budget accounting through a visitor that only charges `PROCESSED`.

However, I would not approve this as written. It fixes the symptom, but leaves several important API and parity details underspecified.

Key gaps:

- The proposed visitor uses `VisitTxResult`, but henyey’s enum also has `Rejected`. The plan only defines `Processed` and `Skipped` behavior. Either use a dedicated two-state broadcast result, or explicitly define/reject `Rejected` for broadcast traversal.

- Tests only in `crates/herder/src/tx_queue/mod.rs` are insufficient. The bug is in the interaction between queue traversal and `App::flush_tx_adverts()` peer filtering. Add at least one app-level or pure helper regression proving “all active peers have seen this high-priority tx” returns `Skipped`, preserves carry-over, and allows a later candidate to be advertised.

- Be precise about DEX lane semantics. A skipped DEX tx is budget-neutral only after it fits current generic and DEX limits and the visitor is invoked. If the top DEX tx exceeds remaining DEX budget, stellar-core drops the lane before visiting it. Tests and docs should encode both cases.

- Avoid invoking an arbitrary caller callback while holding `TransactionQueue`’s store read lock if possible. A callback can accidentally re-enter the queue or hold app locks while the queue lock is held. Prefer snapshotting `FeeEntry`/candidate metadata first, or introduce a constrained internal traversal helper with documented lock rules.

- The plan preserves duplicated lane traversal over `QueueStore::fee_index` instead of reusing or structurally aligning with the existing surge-pricing traversal. A more durable fix would factor a non-mutating “visit fitting candidates” traversal that shares the same lane/drop/budget semantics as `pop_top_txs`, rather than maintaining a parallel hand-rolled version.

- `BroadcastCandidate` containing only hash/op/DEX metadata may be too narrow for future parity work. Stellar-core’s broadcast path can inspect the full tx for arbitrage damping and bans skipped damped txs. Consider a queue-owned candidate/view type that can expose the queued transaction or resources internally while keeping the app-facing hash metadata simple.

- The new skipped-neutral traversal can scan far more of the queue than today when many high-priority txs are already seen by all peers. That may be acceptable, but it should be acknowledged and tested with a bounded/large queue scenario or mitigated with a separate flood worklist/cache.

VERDICT: REVISE

1. Define the broadcast visitor result precisely: either replace `VisitTxResult` with a dedicated `BroadcastVisitResult { Processed, Skipped }`, or specify and test what `Rejected` means.
2. Add app-level regression coverage for `flush_tx_adverts()` or an extracted pure helper proving already-seen candidates are skipped without consuming carry-over budget.
3. Clarify and test that DEX skipped-budget neutrality only applies to candidates that already fit current generic and DEX limits; non-fitting DEX candidates must still drop the DEX lane.
4. Avoid or explicitly constrain callbacks under the queue store lock; prefer snapshotting candidate metadata or factoring a safe internal traversal helper.
5. Refactor toward a shared/non-mutating surge-pricing traversal so broadcast lane/drop semantics cannot diverge from `pop_top_txs`.
6. Reconsider the candidate API shape so future full-tx parity behavior such as arbitrage damping is not blocked by a hash-only visitor.
7. Add a performance-risk note or test for the all-skipped/large-queue case, since skipped-neutral accounting can scan many more transactions per flood tick.

</details>

**Verdict: REVISE**

1. Define the broadcast visitor result precisely: either replace `VisitTxResult` with a dedicated `BroadcastVisitResult { Processed, Skipped }`, or specify and test what `Rejected` means.
2. Add app-level regression coverage for `flush_tx_adverts()` or an extracted pure helper proving already-seen candidates are skipped without consuming carry-over budget.
3. Clarify and test that DEX skipped-budget neutrality only applies to candidates that already fit current generic and DEX limits; non-fitting DEX candidates must still drop the DEX lane.
4. Avoid or explicitly constrain callbacks under the queue store lock; prefer snapshotting candidate metadata or factoring a safe internal traversal helper.
5. Refactor toward a shared/non-mutating surge-pricing traversal so broadcast lane/drop semantics cannot diverge from `pop_top_txs`.
6. Reconsider the candidate API shape so future full-tx parity behavior such as arbitrage damping is not blocked by a hash-only visitor.
7. Add a performance-risk note or test for the all-skipped/large-queue case, since skipped-neutral accounting can scan many more transactions per flood tick.
