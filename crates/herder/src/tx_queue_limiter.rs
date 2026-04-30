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

use henyey_common::Resource;
use thiserror::Error;

use crate::surge_pricing::{
    DexLimitingLaneConfig, FloodLaneConfig, QueueEntry, SorobanGenericLaneConfig,
    SurgePricingLaneConfig, SurgePricingPriorityQueue, VisitTxResult, GENERIC_LANE,
};
use crate::tx_queue::QueuedTransaction;
use henyey_tx::FeeRate;

/// Returned when flood traversal APIs are used without an initialized flood queue
/// (`txs_to_flood` is missing). Use [`TxQueueLimiter::new_flood`] for dedicated flood
/// traversal, or initialize the limiter through normal admission paths (for example
/// [`TxQueueLimiter::can_add_tx`]) before calling [`TxQueueLimiter::mark_tx_for_flood`]
/// or [`TxQueueLimiter::visit_top_txs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("flood queue is not initialized")]
pub struct FloodQueueNotInitialized;

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
fn compute_better_fee(evicted_fee: &FeeRate, new_fee_rate: &FeeRate) -> i64 {
    if evicted_fee.op_count() == 0 {
        return 0;
    }

    // Check if new transaction already beats the evicted one (strictly)
    if evicted_fee.cmp_rate(new_fee_rate) == std::cmp::Ordering::Less {
        return 0;
    }

    // Need to beat evicted fee rate: new_fee / new_ops > evicted_fee / evicted_ops
    // Rearranging: new_fee > evicted_fee * new_ops / evicted_ops
    // Add 1 to ensure strictly greater
    let required_fee = (evicted_fee.inclusion_fee().as_i64() as i128
        * new_fee_rate.op_count() as i128
        / evicted_fee.op_count() as i128)
        + 1;
    required_fee.min(i64::MAX as i128) as i64
}

/// Wrapper that handles `Option<&FeeRate>` — returns 0 for `None`.
fn compute_better_fee_opt(evicted: Option<&FeeRate>, new_fee_rate: &FeeRate) -> i64 {
    match evicted {
        Some(e) => compute_better_fee(e, new_fee_rate),
        None => 0,
    }
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
    /// Maximum evicted fee rate per lane
    lane_evicted_inclusion_fee: Vec<Option<FeeRate>>,
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
            lane_evicted_inclusion_fee: Vec::new(),
        }
    }

    /// Create a temporary operation-only limiter for flood traversal.
    ///
    /// The internal flood queue is initialized immediately because
    /// `visit_top_txs` destructively drains it. Callers that need
    /// non-destructive behavior should build a fresh limiter per traversal.
    #[allow(dead_code)] // Superseded by FloodQueue but kept for reference/testing
    pub(crate) fn new_flood(has_dex_lane: bool, seed: u64) -> Self {
        Self {
            max_resources: Resource::new(vec![i64::MAX]),
            is_soroban: false,
            max_dex_operations: None,
            txs: None,
            lane_config: None,
            txs_to_flood: Some(SurgePricingPriorityQueue::new(
                Box::new(FloodLaneConfig::new(has_dex_lane)),
                seed,
            )),
            lane_evicted_inclusion_fee: Vec::new(),
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
    }

    /// Reset eviction state tracking.
    pub fn reset_eviction_state(&mut self) {
        if let Some(ref txs) = self.txs {
            self.lane_evicted_inclusion_fee = vec![None; txs.get_num_lanes()];
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
        assert_eq!(
            henyey_tx::envelope_utils::is_soroban_envelope(&tx.envelope),
            self.is_soroban,
            "Transaction type mismatch"
        );

        self.ensure_initialized(ledger_version);

        if let Some(ref mut txs) = self.txs {
            txs.add(tx.clone(), ledger_version);
        }
        if let Some(ref mut flood) = self.txs_to_flood {
            flood.add(tx.clone(), ledger_version);
        }
    }

    /// Remove a transaction from the limiter.
    pub fn remove_transaction(&mut self, tx: &QueuedTransaction, ledger_version: u32) {
        let lane = self
            .lane_config
            .as_ref()
            .map(|c| c.get_lane(&tx.envelope))
            .unwrap_or(GENERIC_LANE);

        if let Some(ref mut txs) = self.txs {
            let entry = Self::queue_entry(tx);
            txs.remove_entry(lane, &entry, ledger_version);
        }
        if let Some(ref mut flood) = self.txs_to_flood {
            let entry = Self::queue_entry(tx);
            flood.remove_entry(lane, &entry, ledger_version);
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
        let new_is_soroban = henyey_tx::envelope_utils::is_soroban_envelope(&new_tx.envelope);
        assert_eq!(new_is_soroban, self.is_soroban, "Transaction type mismatch");

        if let Some(old) = old_tx {
            let old_is_soroban = henyey_tx::envelope_utils::is_soroban_envelope(&old.envelope);
            assert_eq!(
                old_is_soroban, new_is_soroban,
                "Old and new transaction type mismatch"
            );
        }

        self.ensure_initialized(ledger_version);
        self.ensure_flood_initialized(ledger_version, broadcast_seed);

        let lane = self
            .lane_config
            .as_ref()
            .map(|c| c.get_lane(&new_tx.envelope))
            .unwrap_or(GENERIC_LANE);

        // Check if the new transaction beats any evicted fees
        let evicted_lane_fee = self
            .lane_evicted_inclusion_fee
            .get(lane)
            .cloned()
            .unwrap_or(None);
        let evicted_generic_fee = self
            .lane_evicted_inclusion_fee
            .get(GENERIC_LANE)
            .cloned()
            .unwrap_or(None);

        let min_fee_to_beat_lane =
            compute_better_fee_opt(evicted_lane_fee.as_ref(), &new_tx.fee_rate);
        let min_fee_to_beat_generic =
            compute_better_fee_opt(evicted_generic_fee.as_ref(), &new_tx.fee_rate);
        let min_inclusion_fee = min_fee_to_beat_lane.max(min_fee_to_beat_generic);

        if min_inclusion_fee > 0 {
            let resource_fee_discount =
                (new_tx.total_fee as i64).saturating_sub(new_tx.inclusion_fee_i64());
            return (
                false,
                min_inclusion_fee.saturating_add(resource_fee_discount),
            );
        }

        // Calculate old tx discount for replace-by-fee
        let old_tx_discount = old_tx.map(|old| {
            self.lane_config
                .as_ref()
                .map(|c| c.tx_resources(&old.envelope, ledger_version))
                .unwrap_or_else(|| Resource::new(vec![old.op_count() as i64]))
        });

        // Parity: update the generic lane limit to stay in sync after upgrades
        if let Some(ref mut txs) = self.txs {
            txs.update_generic_lane_limit(self.max_resources.clone());
        }

        // Check if transaction can fit with eviction
        let Some(ref txs) = self.txs else {
            return (false, 0);
        };

        match txs.can_fit_with_eviction(new_tx, old_tx_discount, ledger_version, None) {
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
        let tx_to_fit_lane = self
            .lane_config
            .as_ref()
            .map(|c| c.get_lane(&tx_to_fit.envelope))
            .unwrap_or(GENERIC_LANE);

        let resources_to_fit = self
            .lane_config
            .as_ref()
            .map(|c| c.tx_resources(&tx_to_fit.envelope, ledger_version))
            .unwrap_or_else(|| Resource::new(vec![tx_to_fit.op_count() as i64]));

        for (tx, evicted_due_to_lane_limit) in txs_to_evict {
            let evict_lane = self
                .lane_config
                .as_ref()
                .map(|c| c.get_lane(&tx.envelope))
                .unwrap_or(GENERIC_LANE);

            if *evicted_due_to_lane_limit {
                // Record in the specific lane
                self.lane_evicted_inclusion_fee[evict_lane] = Some(tx.fee_rate);
            } else {
                // Record in generic lane
                self.lane_evicted_inclusion_fee[GENERIC_LANE] = Some(tx.fee_rate);
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
    pub fn mark_tx_for_flood(
        &mut self,
        tx: &QueuedTransaction,
        ledger_version: u32,
    ) -> Result<(), FloodQueueNotInitialized> {
        let flood = self.txs_to_flood.as_mut().ok_or(FloodQueueNotInitialized)?;
        flood.add(tx.clone(), ledger_version);
        Ok(())
    }

    /// Visit transactions in priority order for flooding.
    ///
    /// This destructively drains the limiter's internal flood queue. Use a
    /// fresh flood limiter for non-destructive transaction-queue reads.
    pub fn visit_top_txs<F>(
        &mut self,
        mut visitor: F,
        lane_resources_left: &mut Vec<Resource>,
        ledger_version: u32,
        custom_limits: Option<&[Resource]>,
    ) -> Result<(), FloodQueueNotInitialized>
    where
        F: FnMut(&QueuedTransaction) -> VisitTxResult,
    {
        let flood = self.txs_to_flood.as_mut().ok_or(FloodQueueNotInitialized)?;
        let result = flood.pop_top_txs(false, ledger_version, |tx| visitor(tx), custom_limits);
        *lane_resources_left = result.lane_left_until_limit;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use henyey_common::Hash256;
    use std::sync::Arc;
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
            envelope: Arc::new(envelope),
            hash: Hash256::from_bytes(hash),
            total_fee: fee,
            fee_rate: FeeRate::new(henyey_tx::InclusionFee::new(fee as i64), ops),
            fee_per_op: if ops > 0 { fee / ops as u64 } else { 0 },
            received_at: std::time::Instant::now(),
            is_dex: false,
        }
    }

    #[test]
    fn test_limiter_creation() {
        let max_resources = Resource::new(vec![1000]);
        let limiter = TxQueueLimiter::new(4, max_resources.clone(), false, None);

        assert!(!limiter.is_soroban);
    }

    #[test]
    fn test_add_and_size() {
        let max_resources = Resource::new(vec![100]);
        let mut limiter = TxQueueLimiter::new(1, max_resources, false, None);

        let tx1 = make_test_tx(1000, 5, 1);
        let tx2 = make_test_tx(2000, 3, 2);

        limiter.add_transaction(&tx1, 25);
        limiter.add_transaction(&tx2, 25);

        // Should have 8 operations total
        assert_eq!(limiter.size(), 8);
    }

    #[test]
    fn test_can_add_tx_fits() {
        let max_resources = Resource::new(vec![100]);
        let mut limiter = TxQueueLimiter::new(1, max_resources, false, None);

        let tx = make_test_tx(1000, 5, 1);
        let mut evictions = Vec::new();

        let (can_add, min_fee) = limiter.can_add_tx(&tx, None, &mut evictions, 25, 12345);

        assert!(can_add);
        assert_eq!(min_fee, 0);
        assert!(evictions.is_empty());
    }

    #[test]
    fn test_eviction_tracking() {
        let max_resources = Resource::new(vec![10]);
        let mut limiter = TxQueueLimiter::new(1, max_resources, false, None);

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
    fn test_mark_tx_for_flood_requires_initialized_queue() {
        let max_resources = Resource::new(vec![10]);
        let mut limiter = TxQueueLimiter::new(1, max_resources, false, None);
        let tx = make_test_tx(100, 1, 1);

        let err = limiter.mark_tx_for_flood(&tx, 25).unwrap_err();
        assert_eq!(err, FloodQueueNotInitialized);
    }

    #[test]
    fn test_visit_top_txs_requires_initialized_queue() {
        let max_resources = Resource::new(vec![10]);
        let mut limiter = TxQueueLimiter::new(1, max_resources, false, None);
        let mut remaining = Vec::new();

        let err = limiter
            .visit_top_txs(
                |_| VisitTxResult::Processed,
                &mut remaining,
                25,
                Some(&[Resource::new(vec![1])]),
            )
            .unwrap_err();
        assert_eq!(err, FloodQueueNotInitialized);
    }

    #[test]
    fn test_flood_limiter_visits_marked_transactions() {
        let mut limiter = TxQueueLimiter::new_flood(false, 0);
        let tx = make_test_tx(100, 1, 1);
        let limits = vec![Resource::new(vec![1])];
        let mut remaining = Vec::new();
        let mut visited = Vec::new();

        limiter.mark_tx_for_flood(&tx, 25).unwrap();
        limiter
            .visit_top_txs(
                |tx| {
                    visited.push(tx.hash);
                    VisitTxResult::Processed
                },
                &mut remaining,
                25,
                Some(&limits),
            )
            .unwrap();

        assert_eq!(visited, vec![tx.hash]);
        assert_eq!(remaining, vec![Resource::new(vec![0])]);
    }

    #[test]
    #[should_panic(expected = "custom flood limits lane count must match queue lane count")]
    fn test_visit_top_txs_rejects_custom_limit_lane_mismatch() {
        let mut limiter = TxQueueLimiter::new_flood(true, 0);
        let tx = make_test_tx(100, 1, 1);
        let limits = vec![Resource::new(vec![1])];
        let mut remaining = Vec::new();

        limiter.mark_tx_for_flood(&tx, 25).unwrap();
        limiter
            .visit_top_txs(
                |_| VisitTxResult::Processed,
                &mut remaining,
                25,
                Some(&limits),
            )
            .unwrap();
    }

    #[test]
    #[should_panic(expected = "custom flood limit dimension mismatch for lane 0")]
    fn test_visit_top_txs_rejects_custom_limit_dimension_mismatch() {
        let mut limiter = TxQueueLimiter::new_flood(false, 0);
        let tx = make_test_tx(100, 1, 1);
        let limits = vec![Resource::new(vec![1, 1])];
        let mut remaining = Vec::new();

        limiter.mark_tx_for_flood(&tx, 25).unwrap();
        limiter
            .visit_top_txs(
                |_| VisitTxResult::Processed,
                &mut remaining,
                25,
                Some(&limits),
            )
            .unwrap();
    }

    #[test]
    fn test_compute_better_fee() {
        // New tx is strictly better - no fee needed
        let evicted = FeeRate::new(henyey_tx::InclusionFee::new(100), 10);
        let new_rate = FeeRate::new(henyey_tx::InclusionFee::new(200), 10);
        assert_eq!(compute_better_fee(&evicted, &new_rate), 0);

        // Evicted has better rate - need higher fee
        let new_rate2 = FeeRate::new(henyey_tx::InclusionFee::new(50), 10);
        let min_fee = compute_better_fee(&evicted, &new_rate2);
        assert!(min_fee > 50);

        // Edge case: zero ops in evicted
        let evicted_zero = FeeRate::new(henyey_tx::InclusionFee::new(100), 0);
        assert_eq!(compute_better_fee(&evicted_zero, &new_rate2), 0);
    }

    /// Regression test for #1496: equal fee rates must NOT return 0.
    /// stellar-core requires strictly higher fees; equal-rate should be rejected.
    #[test]
    fn test_compute_better_fee_rejects_equal_rate() {
        // tx1: fee=200, ops=2 → rate=100
        // tx2: fee=300, ops=3 → rate=100 (equal)
        // Before fix: returned 0 (admitted). After fix: returns positive fee.
        let evicted = FeeRate::new(henyey_tx::InclusionFee::new(200), 2);
        let new_rate = FeeRate::new(henyey_tx::InclusionFee::new(300), 3);
        let min_fee = compute_better_fee(&evicted, &new_rate);
        assert!(
            min_fee > 0,
            "Equal fee rates must require a higher fee, got {}",
            min_fee
        );
    }

    #[test]
    fn test_compute_better_fee_accepts_strictly_higher_rate() {
        // tx1: fee=200, ops=2 → rate=100
        // tx2: fee=301, ops=3 → rate=100.33 (strictly better)
        let evicted = FeeRate::new(henyey_tx::InclusionFee::new(200), 2);
        let new_rate = FeeRate::new(henyey_tx::InclusionFee::new(301), 3);
        assert_eq!(compute_better_fee(&evicted, &new_rate), 0);
    }
}
