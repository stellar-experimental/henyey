//! Persistent flood queue for transaction broadcasting.
//!
//! The [`FloodQueue`] tracks which transactions need to be advertised to peers.
//! It is populated on admission (via [`QueueStore::insert`]) and destructively
//! drained by [`TransactionQueue::broadcast_with_visitor`]. This matches

//! stellar-core's persistent `mTxsToFlood` inside `TxQueueLimiter`.
//!
//! # Design Rationale
//!
//! The previous approach rebuilt a fresh `TxQueueLimiter::new_flood` from all
//! `store.values()` on every flood tick (~200ms), causing O(n log n) sort + O(n)
//! visitor work for the entire mempool repeatedly — even for already-advertised
//! transactions. This dedicated type maintains persistent state that is:
//!
//! - Populated once on admission
//! - Destructively drained on broadcast (entries are removed as they are visited)
//! - Reset and repopulated on ledger close (rebroadcast)
//!
//! Unlike the previous `TxQueueLimiter` approach which overloaded one type for
//! both eviction and flood modes with optional fields, `FloodQueue` makes the
//! flood queue's invariants explicit and avoids the seed-mismatch bug where
//! `TxQueueLimiter::queue_entry()` hardcoded seed 0 for removals.

use henyey_common::Resource;

use crate::surge_pricing::{
    FloodLaneConfig, QueueEntry, SurgePricingLaneConfig, SurgePricingPriorityQueue, VisitTxResult,
};

use super::QueuedTransaction;

/// Persistent flood queue — tracks which transactions need to be advertised.
///
/// Populated on admission, destructively drained by `broadcast_with_visitor`.
/// Matches stellar-core's persistent `mTxsToFlood` inside `TxQueueLimiter`.
pub(crate) struct FloodQueue {
    queue: SurgePricingPriorityQueue,
    seed: u64,
    has_dex_lane: bool,
}

impl FloodQueue {
    /// Create a new flood queue.
    ///
    /// `has_dex_lane` determines whether DEX transactions get a separate lane
    /// (matching the queue's `TxQueueConfig::max_dex_ops` setting).
    pub fn new(has_dex_lane: bool) -> Self {
        let seed = if cfg!(test) { 0 } else { rand::random::<u64>() };
        let config: Box<dyn SurgePricingLaneConfig + Send + Sync> =
            Box::new(FloodLaneConfig::new(has_dex_lane));
        Self {
            queue: SurgePricingPriorityQueue::new(config, seed),
            seed,
            has_dex_lane,
        }
    }

    /// Mark a transaction for flooding. Called on admission to the queue.
    pub fn mark_for_flood(&mut self, tx: &QueuedTransaction, ledger_version: u32) {
        self.queue.add(tx.clone(), ledger_version);
    }

    /// Remove a transaction from the flood queue (e.g., on ban/eviction).
    ///
    /// Uses the stored seed so BTreeSet lookup succeeds — the `QueueEntry`
    /// must be constructed with the same seed that was used when inserting.
    pub fn remove(&mut self, tx: &QueuedTransaction, ledger_version: u32) {
        let entry = QueueEntry::new(tx.clone(), self.seed);
        let lane = self.queue.get_lane(&tx.envelope);
        self.queue.remove_entry(lane, &entry, ledger_version);
    }

    /// Destructively drain top transactions via visitor.
    ///
    /// Delegates to `pop_top_txs(allow_gaps=false)`: entries within budget
    /// are visited and erased; when generic budget is exceeded, the loop breaks
    /// and remaining entries persist for next tick. All visited entries are
    /// erased regardless of whether the visitor returns Processed or Skipped.
    pub fn visit_top_txs(
        &mut self,
        visitor: impl FnMut(&QueuedTransaction) -> VisitTxResult,
        lane_resources_left: &mut Vec<Resource>,
        ledger_version: u32,
        custom_limits: &[Resource],
    ) {
        let result = self
            .queue
            .pop_top_txs(false, ledger_version, visitor, Some(custom_limits));
        *lane_resources_left = result.lane_left_until_limit;
    }

    /// Reset with new seed and repopulate from all queued transactions.
    ///
    /// Called after ledger-close invalidation completes (matching stellar-core's
    /// `resetBestFeeTxs()` + `rebroadcast()`), and when new peers connect.
    pub fn reset_and_repopulate<'a>(
        &mut self,
        txs: impl Iterator<Item = &'a QueuedTransaction>,
        ledger_version: u32,
    ) {
        let new_seed = if cfg!(test) { 0 } else { rand::random::<u64>() };
        self.seed = new_seed;
        let config: Box<dyn SurgePricingLaneConfig + Send + Sync> =
            Box::new(FloodLaneConfig::new(self.has_dex_lane));
        self.queue = SurgePricingPriorityQueue::new(config, new_seed);
        for tx in txs {
            self.queue.add(tx.clone(), ledger_version);
        }
    }

    /// Clear without repopulating (for `clear_data`/`reset_and_rebuild`).
    pub fn clear(&mut self) {
        let new_seed = if cfg!(test) { 0 } else { rand::random::<u64>() };
        self.seed = new_seed;
        let config: Box<dyn SurgePricingLaneConfig + Send + Sync> =
            Box::new(FloodLaneConfig::new(self.has_dex_lane));
        self.queue = SurgePricingPriorityQueue::new(config, new_seed);
    }

    /// Number of lanes in the flood queue.
    ///
    /// Used by `broadcast_with_visitor` to construct matching custom limits.
    pub fn num_lanes(&self) -> usize {
        self.queue.get_num_lanes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use henyey_common::Hash256;
    use std::sync::Arc;
    use std::time::Instant;
    use stellar_xdr::curr::{
        CreateAccountOp, DecoratedSignature, Memo, MuxedAccount, Operation, OperationBody,
        Preconditions, SequenceNumber, Signature as XdrSignature, SignatureHint, Transaction,
        TransactionEnvelope, TransactionExt, TransactionV1Envelope, Uint256,
    };

    fn make_test_envelope(fee: u32, source_byte: u8) -> TransactionEnvelope {
        let source = MuxedAccount::Ed25519(Uint256([source_byte; 32]));
        let operations = vec![Operation {
            source_account: None,
            body: OperationBody::CreateAccount(CreateAccountOp {
                destination: stellar_xdr::curr::AccountId(
                    stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(Uint256([255u8; 32])),
                ),
                starting_balance: 1_000_000_000,
            }),
        }];
        let tx = Transaction {
            source_account: source,
            fee,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: operations.try_into().unwrap(),
            ext: TransactionExt::V0,
        };
        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        })
    }

    fn make_queued_tx(fee: u64, hash_byte: u8) -> QueuedTransaction {
        let envelope = make_test_envelope(fee as u32, hash_byte);
        let mut hash = [0u8; 32];
        hash[0] = hash_byte;
        QueuedTransaction {
            envelope: Arc::new(envelope),
            hash: Hash256(hash),
            received_at: Instant::now(),
            fee_per_op: fee,
            fee_rate: henyey_tx::FeeRate::new(henyey_tx::InclusionFee::new(fee as i64), 1),
            total_fee: fee,
            is_dex: false,
        }
    }

    #[test]
    fn test_mark_and_drain() {
        let mut fq = FloodQueue::new(false);
        let tx1 = make_queued_tx(100, 1);
        let tx2 = make_queued_tx(200, 2);

        fq.mark_for_flood(&tx1, 25);
        fq.mark_for_flood(&tx2, 25);

        let mut visited = Vec::new();
        let mut remaining = Vec::new();
        let limits = vec![Resource::new(vec![i64::MAX])];
        fq.visit_top_txs(
            |tx| {
                visited.push(tx.hash);
                VisitTxResult::Processed
            },
            &mut remaining,
            25,
            &limits,
        );

        assert_eq!(visited.len(), 2);
        // Second call should visit nothing (queue drained)
        visited.clear();
        fq.visit_top_txs(
            |tx| {
                visited.push(tx.hash);
                VisitTxResult::Processed
            },
            &mut remaining,
            25,
            &limits,
        );
        assert_eq!(visited.len(), 0);
    }

    #[test]
    fn test_remove_before_drain() {
        let mut fq = FloodQueue::new(false);
        let tx1 = make_queued_tx(100, 1);
        let tx2 = make_queued_tx(200, 2);

        fq.mark_for_flood(&tx1, 25);
        fq.mark_for_flood(&tx2, 25);

        // Remove tx2 before draining
        fq.remove(&tx2, 25);

        let mut visited = Vec::new();
        let mut remaining = Vec::new();
        let limits = vec![Resource::new(vec![i64::MAX])];
        fq.visit_top_txs(
            |tx| {
                visited.push(tx.hash);
                VisitTxResult::Processed
            },
            &mut remaining,
            25,
            &limits,
        );

        assert_eq!(visited.len(), 1);
        assert_eq!(visited[0], tx1.hash);
    }

    #[test]
    fn test_reset_and_repopulate() {
        let mut fq = FloodQueue::new(false);
        let tx1 = make_queued_tx(100, 1);
        let tx2 = make_queued_tx(200, 2);

        fq.mark_for_flood(&tx1, 25);
        fq.mark_for_flood(&tx2, 25);

        // Drain
        let mut remaining = Vec::new();
        let limits = vec![Resource::new(vec![i64::MAX])];
        fq.visit_top_txs(|_| VisitTxResult::Processed, &mut remaining, 25, &limits);

        // Reset and repopulate
        let txs = vec![tx1.clone(), tx2.clone()];
        fq.reset_and_repopulate(txs.iter(), 25);

        // Should be able to drain again
        let mut visited = Vec::new();
        fq.visit_top_txs(
            |tx| {
                visited.push(tx.hash);
                VisitTxResult::Processed
            },
            &mut remaining,
            25,
            &limits,
        );
        assert_eq!(visited.len(), 2);
    }

    #[test]
    fn test_clear() {
        let mut fq = FloodQueue::new(false);
        let tx1 = make_queued_tx(100, 1);
        fq.mark_for_flood(&tx1, 25);

        fq.clear();

        let mut visited = Vec::new();
        let mut remaining = Vec::new();
        let limits = vec![Resource::new(vec![i64::MAX])];
        fq.visit_top_txs(
            |tx| {
                visited.push(tx.hash);
                VisitTxResult::Processed
            },
            &mut remaining,
            25,
            &limits,
        );
        assert_eq!(visited.len(), 0);
    }

    #[test]
    fn test_num_lanes_no_dex() {
        let fq = FloodQueue::new(false);
        assert_eq!(fq.num_lanes(), 1);
    }

    #[test]
    fn test_num_lanes_with_dex() {
        let fq = FloodQueue::new(true);
        assert_eq!(fq.num_lanes(), 2);
    }
}
