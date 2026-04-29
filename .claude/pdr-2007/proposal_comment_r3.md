## 📝 Proposal Draft (Round 3/5)

## Problem

Issue #2007 tracks a parity gap in transaction advert flood budgeting. `App::flush_tx_adverts()` currently asks `TransactionQueue::broadcast_some(ops_budget, dex_ops_budget)` for a pre-budgeted vector of candidates, then filters out hashes that every active peer has already seen. This can spend generic and DEX advert budgets before the app knows whether any advert will actually be attempted.

Stellar-core performs the budget decision inside the broadcast traversal. `ClassicTransactionQueue::broadcastSome()` calls `broadcastTx(tx)` from the `popTopTxs(false)` visitor, and `popTopTxs(false)` subtracts resources only when the visitor returns `PROCESSED`. Already-broadcast txs return `SKIPPED` and are budget-neutral.

This issue will fix henyey's advert-budget handling for already-seen txs. It will not claim complete `broadcastSome()` parity because henyey still lacks stellar-core's arbitrage damping and ban-on-damping behavior.

## Implementation Plan

1. Replace the vector-returning `TransactionQueue::broadcast_some()` API with a constrained visitor API.
   - Add `BroadcastVisitResult { Processed, Skipped }`; do not reuse `VisitTxResult`, so `Rejected` cannot appear in this broadcast-budget path.
   - Add `BroadcastBudget { ops_remaining: usize, dex_ops_remaining: Option<usize> }`.
   - `dex_ops_remaining: None` means DEX flooding is uncapped and must remain `None` in app carry-over logic.
   - Snapshot fee-ordered `BroadcastCandidate` metadata while holding the queue store read lock, then drop the lock before invoking the visitor. Document candidates as advisory hash/op/DEX metadata snapshots; they do not guarantee the tx is still queued by the time adverts are sent.
   - Put the non-mutating lane/drop/budget traversal in one private helper used by the public API and tests.

2. Preserve stellar-core lane and skipped-budget semantics for the covered advert path.
   - Visit a candidate only after it fits current generic budget and, for DEX txs, current DEX budget.
   - If a DEX candidate exceeds remaining DEX budget, deactivate the DEX lane before visiting it.
   - If any candidate exceeds remaining generic budget, stop traversal.
   - Subtract generic and optional DEX budget only for `BroadcastVisitResult::Processed`.
   - Treat `BroadcastVisitResult::Skipped` as budget-neutral only after the fit checks above.

3. Update `App::flush_tx_adverts()`.
   - Resolve overlay and active peers before queue traversal. If overlay or peers are unavailable, preserve carry-over as today.
   - Extract a pure helper that plans one `BroadcastCandidate` against `PeerTxAdverts` and `per_peer` advert batches.
   - Return `Processed` when at least one active peer needs the hash and `Skipped` when every active peer has already seen it.
   - Keep send failure behavior unchanged for this issue: once at least one peer needs a hash, the traversal consumes budget before `try_send_to()` is attempted; failed sends are still retried because hashes are remembered only after successful sends. This preserves existing henyey retry behavior while fixing the already-seen skipped-tx budget bug.
   - Store carry-over from `BroadcastBudget` instead of recomputing `ops_used` after selection.

4. Update tests.
   - Update existing herder `broadcast_some` tests for the visitor API.
   - Add herder regressions proving skipped candidates do not consume generic budget or DEX budget.
   - Add herder tests proving non-fitting DEX candidates deactivate the DEX lane before visitor invocation.
   - Add an app helper test proving an already-seen high-priority candidate is skipped while a later candidate is planned for a peer.
   - Add an app helper or documented test case for send-failure budgeting remaining unchanged.
   - Add a bounded large/all-skipped traversal test so the intentional extra scanning behavior is explicit.

5. Update documentation.
   - Update `crates/herder/PARITY_STATUS.md` to describe the improved skipped-advert budget parity and the remaining partial parity around arbitrage damping.
   - Update any README/API comments that describe `broadcast_some()` as vector-returning.

## Verification

Run focused tests first:

```bash
CARGO_TARGET_DIR=$HOME/data/pdr-2007/cargo-target cargo test -p henyey-herder broadcast_some
CARGO_TARGET_DIR=$HOME/data/pdr-2007/cargo-target cargo test -p henyey-app tx_advert
```

Then run:

```bash
CARGO_TARGET_DIR=$HOME/data/pdr-2007/cargo-target cargo test --all
CARGO_TARGET_DIR=$HOME/data/pdr-2007/cargo-target cargo clippy --all
cargo fmt --all -- --check
```

---

*Submitting to adversarial critic for review…*
