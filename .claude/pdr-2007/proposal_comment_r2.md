## 📝 Proposal Draft (Round 2/5)

## Problem

Issue #2007 tracks a parity gap in transaction advert flooding. `App::flush_tx_adverts()` currently asks `TransactionQueue::broadcast_some(ops_budget, dex_ops_budget)` for a pre-budgeted vector of candidates, then filters out hashes that every peer has already seen. This split means `TransactionQueue::broadcast_some()` can consume generic and DEX flood budgets, and can drop the DEX lane, before the app knows whether the tx will actually be advertised to any peer.

Stellar-core does this in one traversal. `ClassicTransactionQueue::broadcastSome()` passes a visitor to `visitTopTxs()`/`popTopTxs(false)`, calls `broadcastTx(tx)` inside the visitor, and returns `VisitTxResult::SKIPPED` for already-broadcast txs. `popTopTxs(false)` only subtracts resources for `PROCESSED`, so skipped txs do not consume generic budget or DEX budget.

## Revised Implementation Plan

1. Replace the vector-returning `TransactionQueue::broadcast_some()` API with a constrained broadcast visitor API.
   - Introduce `BroadcastVisitResult { Processed, Skipped }` instead of reusing `VisitTxResult`, so `Rejected` is not representable in flood-budget traversal.
   - Introduce `BroadcastBudget { ops_remaining, dex_ops_remaining }` as the return value.
   - Snapshot fee-ordered candidate metadata from `QueueStore::fee_index` while holding the store read lock, then drop the lock before invoking the caller visitor. This keeps callbacks from running under the queue lock.
   - Factor the traversal into a private non-mutating helper over the candidate snapshot. The helper owns the lane/drop/budget state so tests exercise the same logic used by the public API.

2. Preserve stellar-core lane semantics precisely.
   - A candidate is visited only if it fits current generic budget and, for DEX txs, current DEX budget.
   - If a DEX candidate does not fit remaining DEX budget, the DEX lane is deactivated before visiting it, matching `popTopTxs(false)`.
   - If any candidate does not fit remaining generic budget, traversal stops.
   - Only `BroadcastVisitResult::Processed` subtracts generic and optional DEX budget.
   - `BroadcastVisitResult::Skipped` is budget-neutral but only after the candidate has already passed the fit checks above.

3. Update `App::flush_tx_adverts()` to make per-peer already-seen filtering the broadcast visitor.
   - Resolve overlay and active peers before queue traversal. If overlay or peers are unavailable, preserve carry-over as today.
   - Extract a small pure helper that plans a single `BroadcastCandidate` against `PeerTxAdverts` and `per_peer` batches.
   - The helper returns `Processed` when at least one active peer needs the hash and `Skipped` when all active peers have already seen it.
   - Store carry-over from `BroadcastBudget` instead of recomputing `ops_used` after selection.
   - Keep send/remember behavior unchanged: hashes are remembered only after `overlay.try_send_to()` succeeds.

4. Add focused regression coverage.
   - In `crates/herder/src/tx_queue/mod.rs`, update existing `broadcast_some` tests for the visitor API and add regressions proving skipped candidates do not consume generic or DEX budget.
   - Add tests proving non-fitting DEX candidates still deactivate the DEX lane before any visitor call.
   - In `crates/app/src/app/tx_flooding.rs` or a nearby test module, test the extracted planning helper: an already-seen high-priority candidate is skipped and does not prevent a later candidate from being planned for a peer.
   - Add a large all-skipped traversal test to make the scan behavior explicit and bounded by queue size. This is the intended parity tradeoff: skipped txs may require scanning more candidates in one flood tick, but they no longer consume budget incorrectly.

## Scope Notes

This change intentionally keeps `BroadcastCandidate` at hash/op/DEX metadata because henyey's current advert path does not need the full envelope. The new visitor API leaves room to extend the candidate view later if arbitrage damping or other full-tx broadcast parity work is added, without making rejected states representable in the current budget path.

Queue contents remain unchanged by broadcast traversal, preserving current henyey mempool behavior.

## Verification

Run focused tests first:

```bash
CARGO_TARGET_DIR=$HOME/data/pdr-2007/cargo-target cargo test -p henyey-herder broadcast_some
CARGO_TARGET_DIR=$HOME/data/pdr-2007/cargo-target cargo test -p henyey-app tx_advert
```

Then run the required workspace checks:

```bash
CARGO_TARGET_DIR=$HOME/data/pdr-2007/cargo-target cargo test --all
CARGO_TARGET_DIR=$HOME/data/pdr-2007/cargo-target cargo clippy --all
cargo fmt --all -- --check
```

---

*Submitting to adversarial critic for review…*
