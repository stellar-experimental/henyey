## 📝 Proposal Draft (Round 1/5)

## Problem

Issue #2007 tracks a parity gap in transaction advert flooding. `App::flush_tx_adverts()` currently asks `TransactionQueue::broadcast_some(ops_budget, dex_ops_budget)` for a pre-budgeted vector of candidates, then filters out hashes that every peer has already seen. This split means `TransactionQueue::broadcast_some()` can consume generic and DEX flood budgets, and can drop the DEX lane, before the app knows whether the tx will actually be advertised to any peer.

Stellar-core does this in one traversal. `ClassicTransactionQueue::broadcastSome()` passes a visitor to `visitTopTxs()`/`popTopTxs(false)`, calls `broadcastTx(tx)` inside the visitor, and returns `VisitTxResult::SKIPPED` for already-broadcast txs. `popTopTxs(false)` only subtracts resources for `PROCESSED`, so skipped txs do not consume generic budget, DEX budget, or trigger later lane behavior based on consumed budget.

## Implementation Plan

1. Rework `TransactionQueue::broadcast_some()` from a pre-budgeted vector producer into a visitor-driven budget traversal.
   - Keep the current fee-priority and lane-drop ordering over `QueueStore::fee_index`.
   - Build a `BroadcastCandidate` for each tx that fits current generic/DEX limits.
   - Invoke a caller-supplied visitor returning `VisitTxResult`.
   - Subtract generic and DEX budget only when the visitor returns `VisitTxResult::Processed`.
   - Treat `Skipped` as budget-neutral, matching stellar-core's already-broadcast handling.
   - Return a small summary containing remaining generic and optional DEX budgets so app carry-over can be derived from the traversal itself.

2. Update `App::flush_tx_adverts()` to perform per-peer already-seen filtering inside the `broadcast_some()` visitor.
   - Resolve overlay and peers before queue traversal. If overlay or peers are unavailable, preserve carry-over as today.
   - Hold `tx_adverts_by_peer` while the visitor decides whether a candidate is new to at least one active peer and builds `per_peer` advert batches.
   - Return `Processed` only when at least one peer needs the hash; return `Skipped` when all active peers have already seen it.
   - Remove the separate post-selection `ops_used`/`dex_ops_used` accounting and instead store carry-over from the returned remaining budgets.

3. Preserve existing external behavior outside skipped-budget accounting.
   - `BroadcastCandidate` remains the app-facing shape for hash/op/DEX metadata.
   - Advert send/remember behavior remains unchanged: hashes are remembered only after `overlay.try_send_to()` succeeds.
   - Queue contents are not removed by broadcast traversal, preserving the current henyey mempool behavior.

4. Add focused regression coverage in `crates/herder/src/tx_queue/mod.rs`.
   - Verify skipped high-priority txs do not consume generic budget, allowing later lower-priority txs to be processed.
   - Verify skipped DEX txs do not consume DEX budget or deactivate the DEX lane, allowing later DEX txs to be processed if they fit.
   - Update existing `broadcast_some` tests for the visitor API while preserving assertions for priority ordering, generic budget caps, DEX lane drop, and uncapped DEX behavior.

## Verification

Run focused herder tests first:

```bash
CARGO_TARGET_DIR=$HOME/data/pdr-2007/cargo-target cargo test -p henyey-herder broadcast_some
```

Then run the required workspace checks:

```bash
CARGO_TARGET_DIR=$HOME/data/pdr-2007/cargo-target cargo test --all
CARGO_TARGET_DIR=$HOME/data/pdr-2007/cargo-target cargo clippy --all
cargo fmt --all -- --check
```

---

*Submitting to adversarial critic for review…*
