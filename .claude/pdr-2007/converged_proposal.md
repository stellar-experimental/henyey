## Converged Proposal (Round 4/5)

Converged after forced acceptance at round 4 (critic did not fully approve).

## Problem

Issue #2007 tracks a parity gap in transaction advert flood budgeting. `App::flush_tx_adverts()` currently asks `TransactionQueue::broadcast_some(ops_budget, dex_ops_budget)` for a pre-budgeted vector of candidates, then filters out hashes that every active peer has already seen. This can spend generic and DEX advert budgets before the app knows whether any advert will actually be attempted.

Stellar-core performs the budget decision inside the broadcast traversal. Already-broadcast txs return `SKIPPED`, and `popTopTxs(false)` subtracts resources only for `PROCESSED`.

This issue fixes henyey's advert-budget handling for already-seen txs. It does not claim complete `broadcastSome()` parity because henyey still lacks stellar-core's arbitrage damping and ban-on-damping behavior.

## Implementation Plan

1. Replace the vector-returning `TransactionQueue::broadcast_some()` API with a constrained visitor API.
   - Add `BroadcastVisitResult { Processed, Skipped }`.
   - Add `BroadcastBudget { ops_remaining: usize, dex_ops_remaining: Option<usize>, candidates_visited: usize }`.
   - `dex_ops_remaining: None` means DEX flooding is uncapped and app carry-over must not update the DEX carry-over slot.
   - Snapshot fee-ordered `BroadcastCandidate` metadata under the queue store read lock, then drop the lock before invoking the visitor. Document candidates as advisory metadata snapshots.
   - Use a private non-mutating traversal helper that owns lane/drop/budget state.

2. Preserve reachability behind skipped prefixes.
   - Do not add a scan cap in this issue. A cap without continuation can starve unseen lower-priority txs behind an already-seen high-priority prefix.
   - Traversal remains bounded by the current queue snapshot length.
   - Add a regression proving an unseen candidate after a large skipped prefix is reached in the same traversal.

3. Preserve stellar-core lane semantics for the covered path.
   - For non-generic DEX candidates, check the DEX lane limit before generic. If the DEX candidate exceeds remaining DEX budget, deactivate the DEX lane and continue with non-DEX candidates, even if it also exceeds generic budget.
   - For generic-lane candidates, or DEX candidates that fit DEX but exceed generic, stop traversal.
   - Subtract generic and optional DEX budget only for `Processed`.
   - `Skipped` is budget-neutral only after the candidate has passed fit checks and been visited.

4. Update `App::flush_tx_adverts()`.
   - Resolve overlay and active peers before queue traversal. If unavailable, preserve carry-over as today.
   - Acquire and prune `tx_adverts_by_peer` once before traversal, then pass mutable advert state into the synchronous visitor. The visitor performs no async work and no overlay sends.
   - Extract/test a pure planning helper returning `Processed` when at least one active peer needs the hash and `Skipped` when every active peer has already seen it.
   - Keep send-failure behavior unchanged: planned adverts consume budget before `try_send_to()`, and failed sends retry because hashes are remembered only after successful sends.
   - Store carry-over from `BroadcastBudget`, capped with `min(MAX_CARRYOVER_OPS)` for generic and configured DEX carry-over.

5. Tests and docs.
   - Update existing herder `broadcast_some` tests for the visitor API.
   - Add herder regressions for skipped generic/DEX budget neutrality, DEX-over-both-budgets lane precedence, exact carry-over, `dex_ops_remaining: None`, and large skipped-prefix reachability.
   - Add app helper tests for already-seen high-priority skip allowing a later candidate and for unchanged send-failure planning semantics.
   - Update `crates/herder/PARITY_STATUS.md` and stale README/API comments.

## Verification

```bash
CARGO_TARGET_DIR=$HOME/data/pdr-2007/cargo-target cargo test -p henyey-herder broadcast_some
CARGO_TARGET_DIR=$HOME/data/pdr-2007/cargo-target cargo test -p henyey-app tx_advert
CARGO_TARGET_DIR=$HOME/data/pdr-2007/cargo-target cargo test --all
CARGO_TARGET_DIR=$HOME/data/pdr-2007/cargo-target cargo clippy --all
cargo fmt --all -- --check
```

---

*This proposal was refined through 4 round(s) of adversarial review using the `plan-do-review` skill.*
