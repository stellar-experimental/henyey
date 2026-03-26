//! Resource-aware transaction queue limiting.
//!
//! This module implements [`TxQueueLimiter`] which manages transaction queue
//! admission with multi-dimensional resource tracking and eviction support.
//! It wraps [`SurgePricingPriorityQueue`] to provide:
//!
//! - Resource-based queue limits (operations, bytes, Soroban resources)
//! - Eviction of lower-fee transactions to make room for higher-fee ones
//! - Tracking of evicted transaction fees to prevent fee degradation
//! - Separate priority ordering for flooding (highest fee first)
//!
//! # Parity
//!
//! This module corresponds to `TxQueueLimiter.h` in stellar-core v25.

use henyey_common::{NetworkId, Resource};
use henyey_tx::TransactionFrame;

use crate::surge_pricing::{
    DexLimitingLaneConfig, QueueEntry, SorobanGenericLaneConfig, SurgePricingLaneConfig,
    SurgePricingPriorityQueue, VisitTxResult, GENERIC_LANE,
};
use crate::tx_queue::{fee_rate_cmp, QueuedTransaction};

/// Scale a resource by a multiplier with saturating arithmetic.
fn scale_resource(resource: &Resource, multiplier: i64) -> Resource {
    use henyey_common::ResourceType;
    let values: Vec<i64> = (0..resource.size())
        .map(|i| {
            let ty = match i {
                0 => ResourceType::Operations,
                1 => ResourceType::Instructions,
                2 => ResourceType::TxByteSize,
                3 => ResourceType::DiskReadBytes,
                4 => ResourceType::WriteBytes,
                5 => ResourceType::ReadLedgerEntries,
                6 => ResourceType::WriteLedgerEntries,
                _ => ResourceType::Operations,
            };
            resource
                .try_get_val(ty)
                .unwrap_or(0)
                .saturating_mul(multiplier)
        })
        .collect();
    Resource::new(values)
}

/// Computes the minimum fee needed to beat a previously evicted transaction.
///
/// Returns 0 if the new transaction already has a better fee rate than the evicted one.
fn compute_better_fee(evicted_fee: i64, evicted_ops: u32, new_fee: i64, new_ops: u32) -> i64 {
    if evicted_ops == 0 {
        return 0;
    }

    // Check if new transaction already beats the evicted one
    if fee_rate_cmp(evicted_fee as u64, evicted_ops, new_fee as u64, new_ops)
        != std::cmp::Ordering::Greater
    {
        return 0;
    }

    // Need to beat evicted fee rate: new_fee / new_ops > evicted_fee / evicted_ops
    // Rearranging: new_fee > evicted_fee * new_ops / evicted_ops
    // Add 1 to ensure strictly greater
    let required_fee = (evicted_fee as i128 * new_ops as i128 / evicted_ops as i128) + 1;
    required_fee.min(i64::MAX as i128) as i64
}

/// Resource-aware transaction queue limiter.
///
/// Manages transaction queue admission with:
/// - Multi-dimensional resource tracking (ops, bytes, Soroban resources)
/// - Per-lane limits with eviction support
/// - Tracking of maximum evicted fee per lane
/// - Separate flood priority queue for broadcasting
///
/// # Example
///
/// ```ignore
/// use henyey_herder::TxQueueLimiter;
///
/// let limiter = TxQueueLimiter::new(4, max_ledger_resources, false);
///
/// // Check if a transaction can be added
/// let (can_add, min_fee) = limiter.can_add_tx(&new_tx, None, &mut evictions, ledger_version, seed);
/// if can_add {
///     // Evict transactions if needed
///     limiter.evict_transactions(&evictions, &new_tx, |tx| remove_from_queue(tx));
///     limiter.add_transaction(&new_tx);
/// }
/// ```
pub struct TxQueueLimiter {
    /// Maximum ledger resources (scaled by multiplier)
    max_resources: Resource,
    /// Whether this limiter is for Soroban transactions
    is_soroban: bool,
    /// Optional DEX operation limit
    max_dex_operations: Option<Resource>,
    /// Transaction queue for eviction ordering (lowest fee first)
    txs: Option<SurgePricingPriorityQueue>,
    /// Lane configuration for main queue
    lane_config: Option<Box<dyn SurgePricingLaneConfig + Send + Sync>>,
    /// Transaction queue for flood ordering (highest fee first)
    txs_to_flood: Option<SurgePricingPriorityQueue>,
    /// Lane configuration for flood queue
    flood_lane_config: Option<Box<dyn SurgePricingLaneConfig + Send + Sync>>,
    /// Maximum evicted inclusion fee per lane (fee, ops)
    lane_evicted_inclusion_fee: Vec<(i64, u32)>,
    /// Network ID for transaction hashing
    network_id: NetworkId,
}

impl TxQueueLimiter {
    fn lane_config(&self) -> Box<dyn SurgePricingLaneConfig + Send + Sync> {
        if self.is_soroban {
            Box::new(SorobanGenericLaneConfig::new(self.max_resources.clone()))
        } else {
            Box::new(DexLimitingLaneConfig::new(
                self.max_resources.clone(),
                self.max_dex_operations.clone(),
            ))
        }
    }

    fn make_queue(&self, seed: u64) -> SurgePricingPriorityQueue {
        SurgePricingPriorityQueue::new(self.lane_config(), seed)
    }

    fn frame_for(&self, tx: &QueuedTransaction) -> TransactionFrame {
        TransactionFrame::from_owned_with_network(tx.envelope.clone(), self.network_id)
    }

    fn queue_entry(tx: &QueuedTransaction) -> QueueEntry {
        QueueEntry::new(tx.clone(), 0)
    }

    /// Create a new transaction queue limiter.
    ///
    /// # Arguments
    ///
    /// * `multiplier` - Pool ledger multiplier for sizing limits
    /// * `max_ledger_resources` - Maximum resources per ledger
    /// * `is_soroban` - Whether this limiter is for Soroban transactions
    /// * `max_dex_ops` - Optional DEX operation limit (classic only)
    /// * `network_id` - Network ID for transaction hashing
    pub fn new(
        multiplier: u32,
        max_ledger_resources: Resource,
        is_soroban: bool,
        max_dex_ops: Option<u64>,
        network_id: NetworkId,
    ) -> Self {
        let max_resources = scale_resource(&max_ledger_resources, multiplier as i64);
        let max_dex_operations = if !is_soroban {
            max_dex_ops.map(|ops| Resource::new(vec![ops as i64 * multiplier as i64]))
        } else {
            None
        };

        Self {
            max_resources,
            is_soroban,
            max_dex_operations,
            txs: None,
            lane_config: None,
            txs_to_flood: None,
            flood_lane_config: None,
            lane_evicted_inclusion_fee: Vec::new(),
            network_id,
        }
    }

    /// Initialize the main transaction queue if not already done.
    fn ensure_initialized(&mut self, ledger_version: u32) {
        if self.txs.is_some() {
            return;
        }

        self.reset(ledger_version);
    }

    /// Initialize the flood queue if not already done.
    fn ensure_flood_initialized(&mut self, ledger_version: u32, seed: u64) {
        if self.txs_to_flood.is_some() {
            return;
        }

        self.reset_best_fee_txs(ledger_version, seed);
    }

    /// Reset the transaction queue with new limits.
    pub fn reset(&mut self, _ledger_version: u32) {
        let seed = rand::random::<u64>();
        self.txs = Some(self.make_queue(seed));
        self.lane_config = Some(self.lane_config());
        self.reset_eviction_state();
    }

    /// Reset the flood priority queue.
    pub fn reset_best_fee_txs(&mut self, _ledger_version: u32, seed: u64) {
        // For flood queue, we want highest priority first (different seed for tie-breaking)
        self.txs_to_flood = Some(self.make_queue(seed));
        self.flood_lane_config = Some(self.lane_config());
    }

    /// Reset eviction state tracking.
    pub fn reset_eviction_state(&mut self) {
        if let Some(ref txs) = self.txs {
            self.lane_evicted_inclusion_fee = vec![(0, 0); txs.get_num_lanes()];
        } else {
            self.lane_evicted_inclusion_fee.clear();
        }
    }

    /// Add a transaction to the limiter.
    ///
    /// # Panics
    ///
    /// Panics if the transaction type (Soroban vs classic) doesn't match
    /// the limiter configuration.
    pub fn add_transaction(&mut self, tx: &QueuedTransaction, ledger_version: u32) {
        let frame = self.frame_for(tx);
        assert_eq!(
            frame.is_soroban(),
            self.is_soroban,
            "Transaction type mismatch"
        );

        self.ensure_initialized(ledger_version);

        if let Some(ref mut txs) = self.txs {
            txs.add(tx.clone(), &self.network_id, ledger_version);
        }
        if let Some(ref mut flood) = self.txs_to_flood {
            flood.add(tx.clone(), &self.network_id, ledger_version);
        }
    }

    /// Remove a transaction from the limiter.
    pub fn remove_transaction(&mut self, tx: &QueuedTransaction, ledger_version: u32) {
        let frame = self.frame_for(tx);
        let lane = self
            .lane_config
            .as_ref()
            .map(|c| c.get_lane(&frame))
            .unwrap_or(GENERIC_LANE);

        if let Some(ref mut txs) = self.txs {
            // Create a queue entry to find and remove
            let entry = Self::queue_entry(tx);
            txs.remove_entry(lane, &entry, ledger_version, &self.network_id);
        }
        if let Some(ref mut flood) = self.txs_to_flood {
            let entry = Self::queue_entry(tx);
            flood.remove_entry(lane, &entry, ledger_version, &self.network_id);
        }
    }

    /// Check if a transaction can be added to the queue.
    ///
    /// # Arguments
    ///
    /// * `new_tx` - The transaction to add
    /// * `old_tx` - Optional existing transaction being replaced (for replace-by-fee)
    /// * `txs_to_evict` - Output: transactions that need to be evicted
    /// * `ledger_version` - Current ledger version
    /// * `broadcast_seed` - Seed for tie-breaking in flood queue
    ///
    /// # Returns
    ///
    /// `(can_add, min_fee)` where:
    /// - `can_add` is true if the transaction can be added
    /// - `min_fee` is the minimum fee required (only valid if can_add is false)
    ///   - 0 means the caller should wait (queue is full with higher-fee txs)
    ///   - >0 means the minimum fee needed to pass validation
    pub fn can_add_tx(
        &mut self,
        new_tx: &QueuedTransaction,
        old_tx: Option<&QueuedTransaction>,
        txs_to_evict: &mut Vec<(QueuedTransaction, bool)>,
        ledger_version: u32,
        broadcast_seed: u64,
    ) -> (bool, i64) {
        let frame = self.frame_for(new_tx);
        assert_eq!(
            frame.is_soroban(),
            self.is_soroban,
            "Transaction type mismatch"
        );

        if let Some(old) = old_tx {
            let old_frame = self.frame_for(old);
            assert_eq!(
                old_frame.is_soroban(),
                frame.is_soroban(),
                "Old and new transaction type mismatch"
            );
        }

        self.ensure_initialized(ledger_version);
        self.ensure_flood_initialized(ledger_version, broadcast_seed);

        let lane = self
            .lane_config
            .as_ref()
            .map(|c| c.get_lane(&frame))
            .unwrap_or(GENERIC_LANE);

        // Check if the new transaction beats any evicted fees
        let evicted_lane_fee = self
            .lane_evicted_inclusion_fee
            .get(lane)
            .cloned()
            .unwrap_or((0, 0));
        let evicted_generic_fee = self
            .lane_evicted_inclusion_fee
            .get(GENERIC_LANE)
            .cloned()
            .unwrap_or((0, 0));

        let min_fee_to_beat_lane = compute_better_fee(
            evicted_lane_fee.0,
            evicted_lane_fee.1,
            new_tx.total_fee as i64,
            new_tx.op_count,
        );
        let min_fee_to_beat_generic = compute_better_fee(
            evicted_generic_fee.0,
            evicted_generic_fee.1,
            new_tx.total_fee as i64,
            new_tx.op_count,
        );
        let min_inclusion_fee = min_fee_to_beat_lane.max(min_fee_to_beat_generic);

        if min_inclusion_fee > 0 {
            // Need to report full fee (inclusion fee for eviction threshold)
            return (false, min_inclusion_fee);
        }

        // Calculate old tx discount for replace-by-fee
        let old_tx_discount = old_tx.map(|old| {
            let old_frame = self.frame_for(old);
            self.lane_config
                .as_ref()
                .map(|c| c.tx_resources(&old_frame, ledger_version))
                .unwrap_or_else(|| Resource::new(vec![old.op_count as i64]))
        });

        // Parity: update the generic lane limit to stay in sync after upgrades
        if let Some(ref mut txs) = self.txs {
            txs.update_generic_lane_limit(self.max_resources.clone());
        }

        // Check if transaction can fit with eviction
        let Some(ref txs) = self.txs else {
            return (false, 0);
        };

        match txs.can_fit_with_eviction(new_tx, old_tx_discount, &self.network_id, ledger_version) {
            Some(evictions) => {
                *txs_to_evict = evictions;
                (true, 0)
            }
            None => (false, 0),
        }
    }

    /// Evict transactions to make room for a new transaction.
    ///
    /// # Arguments
    ///
    /// * `txs_to_evict` - Transactions to evict (from `can_add_tx`)
    /// * `tx_to_fit` - The transaction being added
    /// * `evict` - Callback to remove each evicted transaction
    pub fn evict_transactions<F>(
        &mut self,
        txs_to_evict: &[(QueuedTransaction, bool)],
        tx_to_fit: &QueuedTransaction,
        ledger_version: u32,
        mut evict: F,
    ) where
        F: FnMut(&QueuedTransaction),
    {
        let frame = self.frame_for(tx_to_fit);
        let tx_to_fit_lane = self
            .lane_config
            .as_ref()
            .map(|c| c.get_lane(&frame))
            .unwrap_or(GENERIC_LANE);

        let resources_to_fit = self
            .lane_config
            .as_ref()
            .map(|c| c.tx_resources(&frame, ledger_version))
            .unwrap_or_else(|| Resource::new(vec![tx_to_fit.op_count as i64]));

        for (tx, evicted_due_to_lane_limit) in txs_to_evict {
            let evict_frame = self.frame_for(tx);
            let evict_lane = self
                .lane_config
                .as_ref()
                .map(|c| c.get_lane(&evict_frame))
                .unwrap_or(GENERIC_LANE);

            if *evicted_due_to_lane_limit {
                // Record in the specific lane
                self.lane_evicted_inclusion_fee[evict_lane] = (tx.total_fee as i64, tx.op_count);
            } else {
                // Record in generic lane
                self.lane_evicted_inclusion_fee[GENERIC_LANE] = (tx.total_fee as i64, tx.op_count);
            }

            evict(tx);

            // Check if we've freed enough space
            if let Some(ref txs) = self.txs {
                let total = txs.total_resources();
                if (total.clone() + resources_to_fit.clone()).leq(&self.max_resources) {
                    // Also check lane-specific limit
                    let lane_resources = txs.lane_resources(tx_to_fit_lane);
                    let lane_limit = txs.lane_limits(tx_to_fit_lane);
                    if tx_to_fit_lane == GENERIC_LANE
                        || (lane_resources.clone() + resources_to_fit.clone()).leq(&lane_limit)
                    {
                        break;
                    }
                }
            }
        }
    }

    /// Get total resources in the main queue.
    pub fn total_resources(&self) -> Option<Resource> {
        self.txs.as_ref().map(|q| q.total_resources())
    }

    /// Get the number of operations in the queue.
    #[cfg(test)]
    pub fn size(&self) -> usize {
        use henyey_common::ResourceType;
        self.txs
            .as_ref()
            .map(|q| {
                q.total_resources()
                    .try_get_val(ResourceType::Operations)
                    .unwrap_or(0) as usize
            })
            .unwrap_or(0)
    }

    /// Mark a transaction for flooding.
    pub fn mark_tx_for_flood(&mut self, tx: &QueuedTransaction, ledger_version: u32) {
        if let Some(ref mut flood) = self.txs_to_flood {
            flood.add(tx.clone(), &self.network_id, ledger_version);
        }
    }

    /// Visit transactions in priority order for flooding.
    pub fn visit_top_txs<F>(
        &mut self,
        mut visitor: F,
        lane_resources_left: &mut Vec<Resource>,
        ledger_version: u32,
    ) where
        F: FnMut(&QueuedTransaction) -> VisitTxResult,
    {
        if let Some(ref mut flood) = self.txs_to_flood {
            let mut had_not_fitting = vec![false; flood.get_num_lanes()];
            flood.pop_top_txs(
                false,
                &self.network_id,
                ledger_version,
                |tx| visitor(tx),
                lane_resources_left,
                &mut had_not_fitting,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use henyey_common::Hash256;
    use stellar_xdr::curr::{
        DecoratedSignature, Memo, MuxedAccount, Operation, OperationBody, Preconditions,
        SequenceNumber, Signature, SignatureHint, Transaction, TransactionEnvelope,
        TransactionV1Envelope, Uint256,
    };

    fn make_test_tx(fee: u64, ops: u32, seq: i64) -> QueuedTransaction {
        let mut operations = Vec::new();
        for _ in 0..ops {
            operations.push(Operation {
                source_account: None,
                body: OperationBody::Inflation,
            });
        }

        let tx = Transaction {
            source_account: MuxedAccount::Ed25519(Uint256([seq as u8; 32])),
            fee: fee as u32,
            seq_num: SequenceNumber(seq),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: operations.try_into().unwrap(),
            ext: stellar_xdr::curr::TransactionExt::V0,
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0; 4]),
                signature: Signature(vec![0; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        });

        let mut hash = [0u8; 32];
        hash[0] = seq as u8;
        hash[1] = (fee % 256) as u8;

        QueuedTransaction {
            envelope,
            hash: Hash256::from_bytes(hash),
            total_fee: fee,
            op_count: ops,
            fee_per_op: if ops > 0 { fee / ops as u64 } else { 0 },
            received_at: std::time::Instant::now(),
        }
    }

    #[test]
    fn test_limiter_creation() {
        let network_id = NetworkId::testnet();
        let max_resources = Resource::new(vec![1000]);
        let limiter = TxQueueLimiter::new(4, max_resources.clone(), false, None, network_id);

        assert!(!limiter.is_soroban);
    }

    #[test]
    fn test_add_and_size() {
        let network_id = NetworkId::testnet();
        let max_resources = Resource::new(vec![100]);
        let mut limiter = TxQueueLimiter::new(1, max_resources, false, None, network_id);

        let tx1 = make_test_tx(1000, 5, 1);
        let tx2 = make_test_tx(2000, 3, 2);

        limiter.add_transaction(&tx1, 25);
        limiter.add_transaction(&tx2, 25);

        // Should have 8 operations total
        assert_eq!(limiter.size(), 8);
    }

    #[test]
    fn test_can_add_tx_fits() {
        let network_id = NetworkId::testnet();
        let max_resources = Resource::new(vec![100]);
        let mut limiter = TxQueueLimiter::new(1, max_resources, false, None, network_id);

        let tx = make_test_tx(1000, 5, 1);
        let mut evictions = Vec::new();

        let (can_add, min_fee) = limiter.can_add_tx(&tx, None, &mut evictions, 25, 12345);

        assert!(can_add);
        assert_eq!(min_fee, 0);
        assert!(evictions.is_empty());
    }

    #[test]
    fn test_eviction_tracking() {
        let network_id = NetworkId::testnet();
        let max_resources = Resource::new(vec![10]);
        let mut limiter = TxQueueLimiter::new(1, max_resources, false, None, network_id);

        // Add a transaction
        let tx1 = make_test_tx(100, 5, 1);
        limiter.add_transaction(&tx1, 25);

        // Try to add a higher-fee transaction that requires eviction
        let tx2 = make_test_tx(200, 8, 2);
        let mut evictions = Vec::new();

        let (can_add, _) = limiter.can_add_tx(&tx2, None, &mut evictions, 25, 12345);

        // Should be able to add with eviction
        assert!(can_add);
    }

    #[test]
    fn test_compute_better_fee() {
        // New tx is already better - no fee needed
        assert_eq!(compute_better_fee(100, 10, 200, 10), 0);

        // Evicted has better rate - need higher fee
        let min_fee = compute_better_fee(100, 10, 50, 10);
        assert!(min_fee > 50);

        // Edge case: zero ops in evicted
        assert_eq!(compute_better_fee(100, 0, 50, 10), 0);
    }
}
