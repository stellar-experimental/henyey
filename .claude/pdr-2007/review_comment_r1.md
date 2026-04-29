## 🔬 Review-Fix Report (Round 1/3)

<details>
<summary>Full review report (click to expand)</summary>

# Fix Review: 00ac5d97

## Commit Summary
- **Hash**: `00ac5d97e2bb029aaaf4098704592436f82e9e7e`
- **Message**: `Rework broadcast skipped-tx budgeting`
- **Author**: Tomer Weller `<tomer.weller@gmail.com>`
- **Files changed**:
  - `crates/app/src/app/tx_flooding.rs`: 159 lines changed
  - `crates/herder/PARITY_STATUS.md`: 6 lines changed
  - `crates/herder/README.md`: 2 lines changed
  - `crates/herder/src/lib.rs`: 4 lines changed
  - `crates/herder/src/tx_queue/mod.rs`: 288 lines changed
  - Total: 354 insertions, 105 deletions

## Problem Analysis
- **Bug**: `flush_tx_adverts()` previously asked `TransactionQueue::broadcast_some()` for a pre-budgeted candidate list before checking whether each candidate was already advertised to all active peers. Already-seen candidates could consume generic and DEX flood budget before the app layer skipped them.
- **Root cause**: Budget accounting lived inside `TransactionQueue::broadcast_some()` before the app-layer per-peer advert-history filter ran. The filter later avoided sending already-seen hashes, but the queue had already decremented budget and possibly stopped traversal.
- **Impact**: Observable transaction-advert flooding could under-advertise useful transactions when a high-priority prefix was already known by peers. This affects propagation/liveness and carry-over accounting, not ledger state execution or consensus determinism.

## Fix Analysis
- **Approach**: The fix changes `TransactionQueue::broadcast_some()` to visitor-driven traversal. It passes each fitting `BroadcastCandidate` to a visitor and decrements `BroadcastBudget` only when the visitor returns `BroadcastVisitResult::Processed`. `App::flush_tx_adverts()` now uses `plan_tx_advert_candidate()` to return `Skipped` when a candidate is already seen by all active peers.
- **Correctness**: Yes. The fix addresses the root cause by moving the “does this candidate actually produce any new advert?” decision into the budget traversal. `Processed` consumes generic and DEX budget; `Skipped` consumes neither.
- **Design fit**: Good. The visitor mirrors the existing surge-pricing visitor model, where only `Processed` decrements lane resources. New public types are re-exported for the app call site.
- **Edge cases**: Covered or preserved. Empty queue and zero budget paths remain tested; DEX budget `None`, DEX lane drop, generic exhaustion, and large skipped prefixes are covered. Subtraction is guarded by prior fit checks, so no underflow path is apparent.
- **Parity**: Matches stellar-core’s core invariant. Stellar-core `broadcastTx()` returns already/skipped without counting those txs against per-timeslice resources; `popTopTxs(false)` only subtracts resources on `PROCESSED` and drops a limited non-generic lane before stopping on generic exhaustion.
- **Side effects**: The `broadcast_some()` API changes from returning `Vec<BroadcastCandidate>` to returning `BroadcastBudget` through a visitor. Search found only the app call site and tests using it, all updated. Carry-over now uses the returned remaining budget directly, preserving caps.
- **Verdict**: SOUND — the fix addresses the root cause, preserves DEX lane semantics, and aligns with stellar-core’s skipped-visit budget behavior.

## Test Coverage
- **Regression test included**: Yes.
- **Test quality**: Good. The generic and DEX skipped-budget tests would have caught the original budget-before-filtering bug. App helper tests verify already-seen candidates return `Skipped` and planned candidates are not marked sent before actual send.
- **Existing coverage**: Broad `broadcast_some()` tests cover priority order, generic budget caps, empty/zero budgets, removal, DEX limits, lane drop, generic break, uncapped DEX, and returned candidate metadata.
- **Gaps**: A full async `flush_tx_adverts()` integration test with real peer advert history would be useful but is not necessary to validate the root fix.

## Similar Issues

No similar issues identified.

## Refactoring Opportunities

No refactoring needed. The fix is proportionate and the pattern is isolated.

## Recommendations

None required. Optional follow-up: add one higher-level `flush_tx_adverts()` test that exercises already-seen peer history through the full app planning path and asserts carry-over remains budget-neutral.

</details>

**Verdict: SOUND**
