//! Surge pricing lane configuration and priority queue helpers.
//!
//! This module implements the surge pricing mechanism used by Stellar to manage
//! transaction set construction when demand exceeds capacity. Transactions are
//! organized into lanes (e.g., generic, DEX, Soroban) with independent resource
//! limits, and are prioritized by fee rate within each lane.
//!
//! # Overview
//!
//! When the network is under heavy load, more transactions may be submitted than can
//! fit in a single ledger. Surge pricing ensures that:
//!
//! 1. Higher-fee transactions are prioritized
//! 2. Different transaction types don't crowd each other out
//! 3. Resource limits are respected across multiple dimensions
//!
//! # Concepts
//!
//! ## Lanes
//!
//! Lanes partition transactions by type, each with independent resource limits:
//!
//! - **Generic Lane (0)**: Catches all transactions not assigned to specialized lanes
//! - **DEX Lane (1)**: For transactions containing DEX operations (ManageSellOffer, etc.)
//! - **Soroban Lane**: For smart contract transactions (handled separately)
//!
//! The generic lane acts as an "umbrella" - its limits apply to all transactions
//! regardless of their specific lane assignment.
//!
//! ## Lane Configurations
//!
//! Different [`SurgePricingLaneConfig`] implementations define lane assignment rules:
//!
//! - [`DexLimitingLaneConfig`]: Separates DEX transactions into their own lane with
//!   independent limits, preventing DEX traffic from crowding out other transactions
//! - [`SorobanGenericLaneConfig`]: Single-lane config for Soroban transactions with
//!   multi-dimensional resource limits (instructions, memory, etc.)
//! - [`OpsOnlyLaneConfig`]: Simple operation-count-only lane config for queue limits
//!
//! ## Priority Queue
//!
//! The [`SurgePricingPriorityQueue`] manages transaction selection by fee priority,
//! supporting eviction of lower-fee transactions when limits are exceeded.
//!
//! ## Fee Rate Comparison
//!
//! Transactions are compared by their fee rate (fee / operation count), not absolute
//! fee. This ensures fair comparison between transactions of different sizes. Ties
//! are broken deterministically using a seeded hash to ensure consistent ordering
//! across nodes.

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashSet};

use henyey_common::{any_greater, subtract_non_negative, Resource, NUM_CLASSIC_TX_BYTES_RESOURCES};
use henyey_common::{Hash256, NUM_CLASSIC_TX_RESOURCES};
use henyey_tx::TransactionFrame;

use crate::tx_queue::{fee_rate_cmp, QueuedTransaction};

/// Lane index for generic (non-specialized) transactions.
pub(crate) const GENERIC_LANE: usize = 0;

/// Lane index for DEX (decentralized exchange) transactions.
pub(crate) const DEX_LANE: usize = 1;

/// Trait for lane configuration strategies.
///
/// Different lane configurations determine how transactions are assigned to lanes
/// and what resource limits apply to each lane. Implementations define:
///
/// - How to classify transactions into lanes (based on operation types, etc.)
/// - What resource limits apply to each lane
/// - How to measure a transaction's resource consumption
pub(crate) trait SurgePricingLaneConfig: Send + Sync {
    /// Determine which lane a transaction belongs to based on its contents.
    fn get_lane(&self, frame: &TransactionFrame) -> usize;

    /// Get the resource limits for each lane.
    fn lane_limits(&self) -> &[Resource];

    /// Calculate the resources consumed by a transaction.
    fn tx_resources(&self, frame: &TransactionFrame, ledger_version: u32) -> Resource;

    /// Update the generic lane (lane 0) limit.
    ///
    /// Parity: stellar-core calls this in `canAddTx()` to keep lane limits
    /// in sync with current max scaled ledger resources after upgrades.
    fn update_generic_lane_limit(&mut self, limit: Resource);
}

/// Lane configuration that separates DEX transactions into their own lane.
///
/// This prevents high-volume DEX trading from crowding out other transaction
/// types. The DEX lane has its own resource limit independent of the generic lane.
#[derive(Clone)]
pub(crate) struct DexLimitingLaneConfig {
    lane_limits: Vec<Resource>,
    use_byte_limit: bool,
}

impl DexLimitingLaneConfig {
    pub(crate) fn new(limit: Resource, dex_limit: Option<Resource>) -> Self {
        let use_byte_limit = limit.size() == NUM_CLASSIC_TX_BYTES_RESOURCES;
        let mut lane_limits = vec![limit];
        if let Some(limit) = dex_limit {
            lane_limits.push(limit);
        }
        Self {
            lane_limits,
            use_byte_limit,
        }
    }
}

impl SurgePricingLaneConfig for DexLimitingLaneConfig {
    fn get_lane(&self, frame: &TransactionFrame) -> usize {
        if self.lane_limits.len() > DEX_LANE && frame.has_dex_operations() {
            DEX_LANE
        } else {
            GENERIC_LANE
        }
    }

    fn lane_limits(&self) -> &[Resource] {
        &self.lane_limits
    }

    fn tx_resources(&self, frame: &TransactionFrame, ledger_version: u32) -> Resource {
        frame.resources(self.use_byte_limit, ledger_version)
    }

    fn update_generic_lane_limit(&mut self, limit: Resource) {
        self.lane_limits[GENERIC_LANE] = limit;
    }
}

/// Lane configuration for Soroban smart contract transactions.
///
/// Soroban transactions have multi-dimensional resource limits including
/// instructions, memory, and ledger entry counts. This config applies those
/// limits to a single lane.
pub(crate) struct SorobanGenericLaneConfig {
    lane_limits: Vec<Resource>,
}

impl SorobanGenericLaneConfig {
    pub(crate) fn new(limit: Resource) -> Self {
        Self {
            lane_limits: vec![limit],
        }
    }
}

impl SurgePricingLaneConfig for SorobanGenericLaneConfig {
    fn get_lane(&self, frame: &TransactionFrame) -> usize {
        if !frame.is_soroban() {
            panic!("non-soroban tx in soroban lane config");
        }
        GENERIC_LANE
    }

    fn lane_limits(&self) -> &[Resource] {
        &self.lane_limits
    }

    fn tx_resources(&self, frame: &TransactionFrame, ledger_version: u32) -> Resource {
        frame.resources(false, ledger_version)
    }

    fn update_generic_lane_limit(&mut self, limit: Resource) {
        self.lane_limits[GENERIC_LANE] = limit;
    }
}

/// Simple lane configuration that only considers operation count.
///
/// Used for queue admission limits where the primary concern is
/// limiting total operations rather than multi-dimensional resources.
pub(crate) struct OpsOnlyLaneConfig {
    lane_limits: Vec<Resource>,
}

impl OpsOnlyLaneConfig {
    pub(crate) fn new(limit: Resource) -> Self {
        Self {
            lane_limits: vec![limit],
        }
    }
}

impl SurgePricingLaneConfig for OpsOnlyLaneConfig {
    fn get_lane(&self, _frame: &TransactionFrame) -> usize {
        GENERIC_LANE
    }

    fn lane_limits(&self) -> &[Resource] {
        &self.lane_limits
    }

    fn tx_resources(&self, frame: &TransactionFrame, _ledger_version: u32) -> Resource {
        let ops = i64::try_from(frame.resource_operation_count()).unwrap_or(i64::MAX);
        Resource::new(vec![ops])
    }

    fn update_generic_lane_limit(&mut self, limit: Resource) {
        self.lane_limits[GENERIC_LANE] = limit;
    }
}

/// Exclusion context for `can_fit_with_eviction`.
///
/// When checking whether a candidate tx fits in a persistent eviction queue,
/// some entries may be logically absent (e.g., replaced-by-fee txs, or txs
/// already selected for eviction by a prior lane-config pass). This struct
/// lets the caller specify which entries to skip and how much to discount
/// from the queue's resource accounting.
pub(crate) struct EvictionExclusion {
    /// Hashes to skip during cursor iteration.
    pub hashes: HashSet<Hash256>,
    /// Per-lane resource discount from excluded txs.
    /// `lane_resource_discount[i]` is subtracted from `lane_current_count[i]`.
    pub lane_resource_discount: Vec<Resource>,
}

impl EvictionExclusion {
    #[allow(dead_code)] // Used in tx_queue/mod.rs after refactor
    pub(crate) fn new(num_lanes: usize, resource_dim: usize) -> Self {
        Self {
            hashes: HashSet::new(),
            lane_resource_discount: vec![Resource::make_empty(resource_dim); num_lanes],
        }
    }

    #[allow(dead_code)] // Used in tx_queue/mod.rs after refactor
    pub(crate) fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }
}

/// An entry in the surge pricing priority queue.
///
/// Wraps a [`QueuedTransaction`] with the fee and ordering information needed
/// for priority queue operations. Entries are ordered by fee rate (fee/ops),
/// with tie-breaking based on a deterministic hash to ensure consistent ordering.
#[derive(Clone)]
pub(crate) struct QueueEntry {
    inclusion_fee: u64,
    op_count: u32,
    tie_breaker: [u8; 32],
    hash: [u8; 32],
    pub(crate) tx: QueuedTransaction,
}

impl QueueEntry {
    pub(crate) fn new(tx: QueuedTransaction, seed: u64) -> Self {
        let mut tie_breaker = tx.hash.0;
        if seed != 0 {
            let mut seed_bytes = seed.to_be_bytes();
            for (idx, byte) in seed_bytes.iter_mut().enumerate() {
                tie_breaker[idx] ^= *byte;
            }
        }
        Self {
            inclusion_fee: tx.inclusion_fee,
            op_count: tx.op_count,
            tie_breaker,
            hash: tx.hash.0,
            tx,
        }
    }
}

impl PartialEq for QueueEntry {
    fn eq(&self, other: &Self) -> bool {
        self.hash == other.hash
    }
}

impl Eq for QueueEntry {}

impl PartialOrd for QueueEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for QueueEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        let ord = fee_rate_cmp(
            self.inclusion_fee,
            self.op_count,
            other.inclusion_fee,
            other.op_count,
        );
        if ord != Ordering::Equal {
            return ord;
        }
        other
            .tie_breaker
            .cmp(&self.tie_breaker)
            .then_with(|| other.hash.cmp(&self.hash))
    }
}

/// Result of visiting a transaction during priority queue iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisitTxResult {
    /// Transaction was skipped (e.g., sequence gap).
    Skipped,
    /// Transaction was rejected (e.g., validation failure).
    Rejected,
    /// Transaction was successfully processed.
    Processed,
}

/// Result of `pop_top_txs`, containing the remaining lane resources.
pub(crate) struct PopResult {
    /// Remaining resource budget per lane after processing.
    pub lane_left_until_limit: Vec<Resource>,
}

/// Priority queue for surge pricing transaction selection.
///
/// Manages transactions across multiple lanes, selecting highest-fee transactions
/// while respecting per-lane resource limits. Supports eviction of lower-fee
/// transactions when queue limits are exceeded.
///
/// # Algorithm
///
/// The queue maintains separate ordered sets for each lane. When selecting
/// transactions:
///
/// 1. Find the highest-fee transaction across all active lanes
/// 2. Check if it fits within both its specific lane limit AND the generic lane limit
/// 3. If it fits, add it to the result and update both limits
/// 4. If it doesn't fit, mark that lane as having transactions that didn't fit
/// 5. Repeat until no more transactions can be added
///
/// # Eviction
///
/// When adding a transaction that would exceed limits, the queue can evict
/// lower-fee transactions to make room. This ensures high-fee transactions
/// are always prioritized.
pub(crate) struct SurgePricingPriorityQueue {
    lane_config: Box<dyn SurgePricingLaneConfig>,
    lane_limits: Vec<Resource>,
    lane_current_count: Vec<Resource>,
    lanes: Vec<BTreeSet<QueueEntry>>,
    seed: u64,
}

impl SurgePricingPriorityQueue {
    pub(crate) fn new(lane_config: Box<dyn SurgePricingLaneConfig>, seed: u64) -> Self {
        let lane_limits = lane_config.lane_limits().to_vec();
        let resource_len = lane_limits
            .first()
            .map(|limit| limit.size())
            .unwrap_or(NUM_CLASSIC_TX_RESOURCES);
        let lane_current_count = lane_limits
            .iter()
            .map(|_| Resource::make_empty(resource_len))
            .collect();
        let lanes = lane_limits.iter().map(|_| BTreeSet::new()).collect();
        Self {
            lane_config,
            lane_limits,
            lane_current_count,
            lanes,
            seed,
        }
    }

    pub(crate) fn get_num_lanes(&self) -> usize {
        self.lane_limits.len()
    }

    /// Number of lanes in this queue (alias for EvictionExclusion sizing).
    pub(crate) fn lane_count(&self) -> usize {
        self.lane_limits.len()
    }

    /// Dimensionality of the resource vectors in this queue.
    pub(crate) fn resource_dim(&self) -> usize {
        self.lane_limits
            .first()
            .map(|l| l.size())
            .unwrap_or(NUM_CLASSIC_TX_RESOURCES)
    }

    /// Update the generic lane limit to match current max resources.
    ///
    /// Parity: stellar-core calls `updateGenericLaneLimit()` in `canAddTx()`
    /// to keep limits in sync after protocol upgrades.
    pub(crate) fn update_generic_lane_limit(&mut self, limit: Resource) {
        self.lane_config.update_generic_lane_limit(limit.clone());
        self.lane_limits[GENERIC_LANE] = limit;
    }

    pub(crate) fn lane_limits(&self, lane: usize) -> Resource {
        self.lane_limits[lane].clone()
    }

    pub(crate) fn total_resources(&self) -> Resource {
        self.lane_current_count.iter().fold(
            Resource::make_empty(self.lane_limits[0].size()),
            |acc, r| acc + r.clone(),
        )
    }

    pub(crate) fn lane_resources(&self, lane: usize) -> Resource {
        self.lane_current_count[lane].clone()
    }

    pub(crate) fn add(
        &mut self,
        tx: QueuedTransaction,
        network_id: &henyey_common::NetworkId,
        ledger_version: u32,
    ) {
        let frame = TransactionFrame::from_owned_with_network(tx.envelope.clone(), *network_id);
        let lane = self.lane_config.get_lane(&frame);
        let resources = self.lane_config.tx_resources(&frame, ledger_version);
        let inserted = self
            .lanes
            .get_mut(lane)
            .expect("lane")
            .insert(QueueEntry::new(tx, self.seed));
        if inserted {
            self.lane_current_count[lane] += resources;
        }
    }

    pub(crate) fn tx_resources(&self, frame: &TransactionFrame, ledger_version: u32) -> Resource {
        self.lane_config.tx_resources(frame, ledger_version)
    }

    pub(crate) fn get_lane(&self, frame: &TransactionFrame) -> usize {
        self.lane_config.get_lane(frame)
    }

    fn erase(
        &mut self,
        lane: usize,
        entry: &QueueEntry,
        ledger_version: u32,
        network_id: &henyey_common::NetworkId,
    ) {
        // Match stellar-core ordering: get stored element → compute resources →
        // assert → subtract → remove (SurgePricingUtils.cpp:279-287).
        let Some(stored) = self.lanes[lane].get(entry) else {
            return;
        };
        let frame =
            TransactionFrame::from_owned_with_network(stored.tx.envelope.clone(), *network_id);
        let resources = self.lane_config.tx_resources(&frame, ledger_version);
        assert!(
            resources.leq(&self.lane_current_count[lane]),
            "erase: resources exceed lane current count"
        );
        self.lane_current_count[lane] -= resources;
        self.lanes[lane].remove(entry);
    }

    fn top_entry(&self, lane: usize) -> Option<QueueEntry> {
        self.lanes[lane].iter().next_back().cloned()
    }

    pub(crate) fn peek_top(&self) -> Option<(usize, QueueEntry)> {
        let mut best: Option<(usize, QueueEntry)> = None;
        for (lane, _) in self.lane_limits.iter().enumerate() {
            let Some(entry) = self.top_entry(lane) else {
                continue;
            };
            match &best {
                None => best = Some((lane, entry)),
                Some((_, best_entry)) => {
                    if entry > *best_entry {
                        best = Some((lane, entry));
                    }
                }
            }
        }
        best
    }

    pub(crate) fn remove_entry(
        &mut self,
        lane: usize,
        entry: &QueueEntry,
        ledger_version: u32,
        network_id: &henyey_common::NetworkId,
    ) {
        self.erase(lane, entry, ledger_version, network_id);
    }

    pub(crate) fn pop_top_txs(
        &mut self,
        allow_gaps: bool,
        network_id: &henyey_common::NetworkId,
        ledger_version: u32,
        mut visitor: impl FnMut(&QueuedTransaction) -> VisitTxResult,
    ) -> PopResult {
        let mut lane_left_until_limit = self.lane_limits.clone();
        let mut lane_active = vec![true; self.lane_limits.len()];

        loop {
            let mut best: Option<(usize, QueueEntry)> = None;
            for (lane, _) in self.lane_limits.iter().enumerate() {
                if !lane_active[lane] {
                    continue;
                }
                let Some(entry) = self.top_entry(lane) else {
                    continue;
                };
                match &best {
                    None => best = Some((lane, entry)),
                    Some((_, best_entry)) => {
                        if entry > *best_entry {
                            best = Some((lane, entry));
                        }
                    }
                }
            }

            let Some((lane, entry)) = best else {
                break;
            };

            let frame =
                TransactionFrame::from_owned_with_network(entry.tx.envelope.clone(), *network_id);
            let resources = self.lane_config.tx_resources(&frame, ledger_version);
            let exceeds_lane = any_greater(&resources, &lane_left_until_limit[lane]);
            let exceeds_generic = any_greater(&resources, &lane_left_until_limit[GENERIC_LANE]);

            if exceeds_lane || exceeds_generic {
                if allow_gaps {
                    self.erase(lane, &entry, ledger_version, network_id);
                    continue;
                } else if lane != GENERIC_LANE && exceeds_lane {
                    lane_active[lane] = false;
                    continue;
                } else {
                    break;
                }
            }

            let visit_res = visitor(&entry.tx);
            if visit_res == VisitTxResult::Processed {
                lane_left_until_limit[GENERIC_LANE] -= resources.clone();
                if lane != GENERIC_LANE {
                    lane_left_until_limit[lane] -= resources;
                }
            }
            self.erase(lane, &entry, ledger_version, network_id);
        }
        PopResult {
            lane_left_until_limit,
        }
    }

    pub(crate) fn can_fit_with_eviction(
        &self,
        tx: &QueuedTransaction,
        tx_discount: Option<Resource>,
        network_id: &henyey_common::NetworkId,
        ledger_version: u32,
        exclusion: Option<&EvictionExclusion>,
    ) -> Option<Vec<(QueuedTransaction, bool)>> {
        let frame = TransactionFrame::from_owned_with_network(tx.envelope.clone(), *network_id);
        let lane = self.lane_config.get_lane(&frame);
        let mut tx_resources = self.lane_config.tx_resources(&frame, ledger_version);
        if let Some(discount) = tx_discount {
            tx_resources = subtract_non_negative(&tx_resources, &discount);
        }

        // Compute effective resource counts, accounting for excluded entries.
        let effective_lane_count: Vec<Resource> = if let Some(excl) = exclusion {
            self.lane_current_count
                .iter()
                .enumerate()
                .map(|(i, count)| {
                    if i < excl.lane_resource_discount.len() {
                        subtract_non_negative(count, &excl.lane_resource_discount[i])
                    } else {
                        count.clone()
                    }
                })
                .collect()
        } else {
            self.lane_current_count.clone()
        };

        let effective_total = effective_lane_count.iter().fold(
            Resource::make_empty(self.lane_limits[0].size()),
            |acc, r| acc + r.clone(),
        );

        if any_greater(&tx_resources, &self.lane_limits[GENERIC_LANE])
            || any_greater(&tx_resources, &self.lane_limits[lane])
        {
            return None;
        }

        if !effective_total.can_add(&tx_resources)
            || !effective_lane_count[lane].can_add(&tx_resources)
        {
            return None;
        }

        let new_total = effective_total + tx_resources.clone();
        let new_lane = effective_lane_count[lane].clone() + tx_resources.clone();
        if new_total.leq(&self.lane_limits[GENERIC_LANE]) && new_lane.leq(&self.lane_limits[lane]) {
            return Some(Vec::new());
        }

        let mut needed_total = subtract_non_negative(&new_total, &self.lane_limits[GENERIC_LANE]);
        let mut needed_lane = subtract_non_negative(&new_lane, &self.lane_limits[lane]);

        #[derive(Clone)]
        struct LaneCursor<'a> {
            lane: usize,
            entries: Vec<&'a QueueEntry>,
            index: usize,
            active: bool,
        }

        impl LaneCursor<'_> {
            fn current(&self) -> Option<&QueueEntry> {
                if !self.active {
                    return None;
                }
                self.entries.get(self.index).copied()
            }

            fn advance(&mut self) {
                self.index = self.index.saturating_add(1);
                if self.index >= self.entries.len() {
                    self.active = false;
                }
            }

            fn drop_lane(&mut self) {
                self.active = false;
            }
        }

        let exclude_hashes = exclusion.map(|e| &e.hashes);
        let mut cursors: Vec<LaneCursor> = self
            .lanes
            .iter()
            .enumerate()
            .map(|(lane, set)| {
                let entries: Vec<&QueueEntry> = if let Some(excl) = exclude_hashes {
                    set.iter()
                        .filter(|e| !excl.contains(&Hash256::from_bytes(e.hash)))
                        .collect()
                } else {
                    set.iter().collect()
                };
                let active = !entries.is_empty();
                LaneCursor {
                    lane,
                    entries,
                    index: 0,
                    active,
                }
            })
            .collect();

        let mut evictions: Vec<(QueuedTransaction, bool)> = Vec::new();
        let tx_account = tx.account_key();

        while needed_total.any_positive() || needed_lane.any_positive() {
            let mut evicted_due_to_lane_limit = false;
            let (evict_lane, entry) = loop {
                let mut best: Option<(usize, QueueEntry)> = None;
                for cursor in cursors.iter() {
                    if let Some(entry) = cursor.current() {
                        match &best {
                            None => best = Some((cursor.lane, entry.clone())),
                            Some((_, best_entry)) => {
                                if entry < best_entry {
                                    best = Some((cursor.lane, entry.clone()));
                                }
                            }
                        }
                    }
                }

                let (evict_lane, entry) = best?;

                let can_evict = lane == GENERIC_LANE
                    || lane == evict_lane
                    || any_greater(&needed_total, &needed_lane);
                if !can_evict {
                    evicted_due_to_lane_limit = true;
                    if let Some(cursor) = cursors.get_mut(evict_lane) {
                        cursor.drop_lane();
                    }
                    continue;
                }
                break (evict_lane, entry);
            };

            if fee_rate_cmp(
                entry.inclusion_fee,
                entry.op_count,
                tx.inclusion_fee,
                tx.op_count,
            ) != Ordering::Less
            {
                return None;
            }

            if entry.tx.account_key() == tx_account {
                return None;
            }

            let evict_frame =
                TransactionFrame::from_owned_with_network(entry.tx.envelope.clone(), *network_id);
            let evict_resources = self.lane_config.tx_resources(&evict_frame, ledger_version);
            evictions.push((entry.tx.clone(), evicted_due_to_lane_limit));

            needed_total = subtract_non_negative(&needed_total, &evict_resources);
            if lane == GENERIC_LANE || lane == evict_lane {
                needed_lane = subtract_non_negative(&needed_lane, &evict_resources);
            }

            if let Some(cursor) = cursors.get_mut(evict_lane) {
                cursor.advance();
            }
        }

        Some(evictions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use henyey_common::Resource;

    #[test]
    fn test_update_generic_lane_limit_dex_config() {
        let initial = Resource::new(vec![100]);
        let dex_limit = Resource::new(vec![50]);
        let mut config = DexLimitingLaneConfig::new(initial.clone(), Some(dex_limit.clone()));

        assert_eq!(config.lane_limits()[GENERIC_LANE], initial);
        assert_eq!(config.lane_limits()[DEX_LANE], dex_limit);

        // Update generic lane
        let new_limit = Resource::new(vec![200]);
        config.update_generic_lane_limit(new_limit.clone());
        assert_eq!(config.lane_limits()[GENERIC_LANE], new_limit);
        // DEX lane should be unchanged
        assert_eq!(config.lane_limits()[DEX_LANE], dex_limit);
    }

    #[test]
    fn test_update_generic_lane_limit_soroban_config() {
        // Soroban resources have 7 dimensions
        let initial = Resource::new(vec![1000, 500, 200, 100, 50, 25, 10]);
        let mut config = SorobanGenericLaneConfig::new(initial.clone());

        assert_eq!(config.lane_limits()[GENERIC_LANE], initial);

        let new_limit = Resource::new(vec![2000, 1000, 400, 200, 100, 50, 20]);
        config.update_generic_lane_limit(new_limit.clone());
        assert_eq!(config.lane_limits()[GENERIC_LANE], new_limit);
    }

    #[test]
    fn test_update_generic_lane_limit_ops_only_config() {
        let initial = Resource::new(vec![500]);
        let mut config = OpsOnlyLaneConfig::new(initial.clone());

        assert_eq!(config.lane_limits()[GENERIC_LANE], initial);

        let new_limit = Resource::new(vec![1000]);
        config.update_generic_lane_limit(new_limit.clone());
        assert_eq!(config.lane_limits()[GENERIC_LANE], new_limit);
    }

    #[test]
    fn test_update_generic_lane_limit_priority_queue() {
        let initial = Resource::new(vec![100]);
        let config = Box::new(OpsOnlyLaneConfig::new(initial.clone()));
        let mut queue = SurgePricingPriorityQueue::new(config, 42);

        assert_eq!(queue.lane_limits(GENERIC_LANE), initial);

        let new_limit = Resource::new(vec![300]);
        queue.update_generic_lane_limit(new_limit.clone());
        assert_eq!(queue.lane_limits(GENERIC_LANE), new_limit);
    }

    /// Verify that `erase()` panics when resources exceed `lane_current_count`,
    /// matching stellar-core's `releaseAssert(res <= mLaneCurrentCount[lane])`
    /// in SurgePricingUtils.cpp:284.
    #[test]
    #[should_panic(expected = "erase: resources exceed lane current count")]
    fn test_erase_panics_when_resources_exceed_lane_count() {
        use crate::tx_queue::QueuedTransaction;
        use henyey_common::NetworkId;
        use stellar_xdr::curr::{
            DecoratedSignature, Memo, MuxedAccount, Operation, OperationBody, Preconditions,
            SequenceNumber, Signature, SignatureHint, Transaction, TransactionEnvelope,
            TransactionV1Envelope, Uint256,
        };

        let network_id = NetworkId::testnet();
        let config = Box::new(OpsOnlyLaneConfig::new(Resource::new(vec![100])));
        let mut queue = SurgePricingPriorityQueue::new(config, 42);

        // Build a minimal 1-op transaction.
        let tx = Transaction {
            source_account: MuxedAccount::Ed25519(Uint256([1u8; 32])),
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![Operation {
                source_account: None,
                body: OperationBody::Inflation,
            }]
            .try_into()
            .unwrap(),
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
        let queued = QueuedTransaction {
            envelope,
            hash: henyey_common::Hash256::from_bytes([1u8; 32]),
            total_fee: 100,
            inclusion_fee: 100,
            op_count: 1,
            fee_per_op: 100,
            received_at: std::time::Instant::now(),
        };

        // Add the transaction (lane_current_count[0] becomes Resource([1])).
        queue.add(queued.clone(), &network_id, 22);
        assert_eq!(
            queue.lane_current_count[GENERIC_LANE],
            Resource::new(vec![1])
        );

        // Corrupt lane_current_count to zero — simulates an accounting bug.
        queue.lane_current_count[GENERIC_LANE] = Resource::new(vec![0]);

        // Build a QueueEntry matching the stored one for the erase() lookup.
        let entry = QueueEntry::new(queued, queue.seed);

        // erase() should panic: resources (1 op) > lane_current_count (0).
        queue.erase(GENERIC_LANE, &entry, 22, &network_id);
    }
}
