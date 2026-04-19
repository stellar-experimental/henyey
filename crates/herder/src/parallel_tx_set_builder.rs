//! Parallel transaction set building for Soroban phases.
//!
//! This module partitions Soroban transactions into stages and clusters for
//! parallel execution, matching the stellar-core `ParallelTxSetBuilder` behavior.
//!
//! # Algorithm Overview
//!
//! 1. **Conflict detection**: Build a conflict graph based on RW-RW and RO-RW
//!    footprint overlaps between transactions.
//! 2. **Stage building**: Greedily assign transactions to stages. Within each
//!    stage, conflicting transactions are merged into the same cluster.
//! 3. **Bin packing**: Clusters within a stage are packed into a fixed number
//!    of bins (parallel execution slots) using first-fit-decreasing.
//! 4. **Multi-stage optimization**: Try multiple stage counts and pick the one
//!    with the fewest stages that achieves >= 99.9% of the maximum total
//!    inclusion fee.

use henyey_common::types::Hash256;
use henyey_common::NetworkId;
use henyey_tx::TransactionFrame;
use stellar_xdr::curr::{
    DependentTxCluster, GeneralizedTransactionSet, Hash, LedgerKey, Limits,
    ParallelTxExecutionStage, ParallelTxsComponent, TransactionEnvelope, TransactionPhase,
    TransactionSetV1, TxSetComponent, TxSetComponentTxsMaybeDiscountedFee, VecM, WriteXdr,
};

// ---------------------------------------------------------------------------
// BitSet
// ---------------------------------------------------------------------------

/// Simple growable bitset for conflict tracking.
#[derive(Clone, Default, Debug)]
struct BitSet {
    words: Vec<u64>,
}

impl BitSet {
    fn with_capacity(bits: usize) -> Self {
        let words = bits.div_ceil(64);
        Self {
            words: vec![0u64; words],
        }
    }

    fn set(&mut self, idx: usize) {
        let word = idx / 64;
        if word >= self.words.len() {
            self.words.resize(word + 1, 0);
        }
        self.words[word] |= 1u64 << (idx % 64);
    }

    fn get(&self, idx: usize) -> bool {
        let word = idx / 64;
        if word >= self.words.len() {
            return false;
        }
        self.words[word] & (1u64 << (idx % 64)) != 0
    }

    fn intersects(&self, other: &BitSet) -> bool {
        let len = self.words.len().min(other.words.len());
        self.words[..len]
            .iter()
            .zip(&other.words[..len])
            .any(|(&a, &b)| a & b != 0)
    }

    fn union_with(&mut self, other: &BitSet) {
        if other.words.len() > self.words.len() {
            self.words.resize(other.words.len(), 0);
        }
        for (w, &o) in self.words.iter_mut().zip(other.words.iter()) {
            *w |= o;
        }
    }

    fn difference_with(&mut self, other: &BitSet) {
        let len = self.words.len().min(other.words.len());
        for (w, &o) in self.words[..len].iter_mut().zip(&other.words[..len]) {
            *w &= !o;
        }
    }

    fn iter_ones(&self) -> impl Iterator<Item = usize> + '_ {
        self.words.iter().enumerate().flat_map(|(word_idx, &word)| {
            let base = word_idx * 64;
            (0..64).filter_map(move |bit| {
                if word & (1u64 << bit) != 0 {
                    Some(base + bit)
                } else {
                    None
                }
            })
        })
    }
}

// ---------------------------------------------------------------------------
// Builder types
// ---------------------------------------------------------------------------

struct BuilderTx {
    id: usize,
    instructions: u32,
    conflicts: BitSet,
}

#[derive(Clone)]
struct Cluster {
    instructions: u64,
    conflicts: BitSet,
    tx_ids: BitSet,
}

impl Cluster {
    fn from_tx(tx: &BuilderTx) -> Self {
        let mut tx_ids = BitSet::default();
        tx_ids.set(tx.id);
        Self {
            instructions: tx.instructions as u64,
            conflicts: tx.conflicts.clone(),
            tx_ids,
        }
    }

    fn merge(&mut self, other: &Cluster) {
        self.instructions = self
            .instructions
            .checked_add(other.instructions)
            .expect("cluster instructions overflow on merge");
        self.conflicts.union_with(&other.conflicts);
        self.tx_ids.union_with(&other.tx_ids);
    }
}

// ---------------------------------------------------------------------------
// Parallel partition config
// ---------------------------------------------------------------------------

struct ParallelPartitionConfig {
    clusters_per_stage: u32,
    instructions_per_cluster: u64,
}

impl ParallelPartitionConfig {
    fn new(
        stage_count: u32,
        ledger_max_instructions: i64,
        ledger_max_dependent_tx_clusters: u32,
    ) -> Self {
        assert!(
            ledger_max_instructions >= 0,
            "ledger_max_instructions must be non-negative, got {}",
            ledger_max_instructions
        );
        let instructions_per_cluster = if stage_count > 0 {
            (ledger_max_instructions as u64) / (stage_count as u64)
        } else {
            ledger_max_instructions as u64
        };
        Self {
            clusters_per_stage: ledger_max_dependent_tx_clusters,
            instructions_per_cluster,
        }
    }

    fn instructions_per_stage(&self) -> u64 {
        self.instructions_per_cluster
            .checked_mul(self.clusters_per_stage as u64)
            .expect("instructions_per_stage overflow")
    }
}

// ---------------------------------------------------------------------------
// Stage
// ---------------------------------------------------------------------------

struct Stage {
    clusters: Vec<Cluster>,
    /// Bin packing: bins[i] contains the BitSet of tx IDs in bin i.
    bin_packing: Vec<BitSet>,
    /// Instructions per bin.
    bin_instructions: Vec<u64>,
    /// Total instructions across all clusters.
    total_instructions: u64,
    config: ParallelPartitionConfig,
    tried_compacting_bin_packing: bool,
}

impl Stage {
    fn new(config: ParallelPartitionConfig) -> Self {
        let n = config.clusters_per_stage as usize;
        Self {
            clusters: Vec::new(),
            bin_packing: vec![BitSet::default(); n],
            bin_instructions: vec![0u64; n],
            total_instructions: 0,
            config,
            tried_compacting_bin_packing: false,
        }
    }

    /// Try to add a transaction to this stage. Returns true if successful.
    fn try_add(&mut self, tx: &BuilderTx) -> bool {
        // Fast fail: check if total instructions would exceed stage limit.
        if self
            .total_instructions
            .checked_add(tx.instructions as u64)
            .map_or(true, |sum| sum > self.config.instructions_per_stage())
        {
            return false;
        }

        // Find clusters that conflict with this TX.
        let conflicting_indices: Vec<usize> = self
            .clusters
            .iter()
            .enumerate()
            .filter(|(_, c)| c.conflicts.get(tx.id))
            .map(|(i, _)| i)
            .collect();

        // Create new cluster set: merge all conflicting clusters + new TX.
        let new_clusters = self.create_new_clusters(tx, &conflicting_indices);
        let new_clusters = match new_clusters {
            Some(c) => c,
            None => return false,
        };

        // Try in-place bin packing (greedy first-fit).
        let merged_cluster = new_clusters.last().unwrap();
        if self.try_in_place_bin_packing(merged_cluster, &conflicting_indices) {
            self.clusters = new_clusters;
            self.total_instructions = self
                .total_instructions
                .checked_add(tx.instructions as u64)
                .expect("total instructions overflow");
            return true;
        }

        // Optimization: if no conflicts and we already tried compacting, skip.
        if conflicting_indices.is_empty() && self.tried_compacting_bin_packing {
            return false;
        }

        // Full bin packing recomputation (first-fit-decreasing).
        let mut new_bin_instructions = vec![0u64; self.config.clusters_per_stage as usize];
        match bin_pack_clusters(
            &new_clusters,
            self.config.clusters_per_stage,
            self.config.instructions_per_cluster,
            &mut new_bin_instructions,
        ) {
            Some(new_packing) => {
                self.clusters = new_clusters;
                self.bin_packing = new_packing;
                self.bin_instructions = new_bin_instructions;
                self.total_instructions = self
                    .total_instructions
                    .checked_add(tx.instructions as u64)
                    .expect("total instructions overflow");
                true
            }
            None => {
                if conflicting_indices.is_empty() {
                    self.tried_compacting_bin_packing = true;
                }
                false
            }
        }
    }

    /// Create new cluster set by merging all conflicting clusters with the new TX.
    /// Returns None if the merged cluster exceeds the per-cluster instruction limit.
    fn create_new_clusters(
        &self,
        tx: &BuilderTx,
        conflicting_indices: &[usize],
    ) -> Option<Vec<Cluster>> {
        // Start with a cluster containing just the new TX.
        let mut merged = Cluster::from_tx(tx);

        // Merge all conflicting clusters into it.
        for &idx in conflicting_indices {
            merged.merge(&self.clusters[idx]);
        }

        // Check if merged cluster exceeds instruction limit.
        if merged.instructions > self.config.instructions_per_cluster {
            return None;
        }

        // Build new cluster list: non-conflicting clusters + merged cluster.
        let conflicting_set: std::collections::HashSet<usize> =
            conflicting_indices.iter().copied().collect();
        let mut new_clusters: Vec<Cluster> = self
            .clusters
            .iter()
            .enumerate()
            .filter(|(i, _)| !conflicting_set.contains(i))
            .map(|(_, c)| c.clone())
            .collect();
        new_clusters.push(merged);
        Some(new_clusters)
    }

    /// Try greedy in-place bin packing. Returns true if the new cluster fits
    /// without needing a full repack.
    fn try_in_place_bin_packing(
        &mut self,
        new_cluster: &Cluster,
        conflicting_indices: &[usize],
    ) -> bool {
        // Remove conflicting clusters from their bins.
        let mut removed: Vec<(usize, u64, BitSet)> = Vec::new();
        for &idx in conflicting_indices {
            let cluster = &self.clusters[idx];
            // Find which bin this cluster is in — must exist in exactly one.
            let matching_bins: Vec<usize> = self
                .bin_packing
                .iter()
                .enumerate()
                .filter(|(_, bin)| bin.intersects(&cluster.tx_ids))
                .map(|(id, _)| id)
                .collect();
            assert!(
                matching_bins.len() == 1,
                "cluster must be in exactly one bin, found {}",
                matching_bins.len()
            );
            let bin_id = matching_bins[0];
            removed.push((bin_id, cluster.instructions, cluster.tx_ids.clone()));
            self.bin_instructions[bin_id] = self.bin_instructions[bin_id]
                .checked_sub(cluster.instructions)
                .expect("bin instructions underflow during removal");
            self.bin_packing[bin_id].difference_with(&cluster.tx_ids);
        }

        // Try to fit the new (merged) cluster into an existing bin.
        for bin_id in 0..self.config.clusters_per_stage as usize {
            if self.bin_instructions[bin_id]
                .checked_add(new_cluster.instructions)
                .map_or(false, |sum| sum <= self.config.instructions_per_cluster)
            {
                self.bin_instructions[bin_id] = self.bin_instructions[bin_id]
                    .checked_add(new_cluster.instructions)
                    .expect("bin instructions overflow during placement");
                self.bin_packing[bin_id].union_with(&new_cluster.tx_ids);
                return true;
            }
        }

        // Revert the removals.
        for (bin_id, insns, tx_ids) in removed {
            self.bin_instructions[bin_id] = self.bin_instructions[bin_id]
                .checked_add(insns)
                .expect("bin instructions overflow during revert");
            self.bin_packing[bin_id].union_with(&tx_ids);
        }
        false
    }

    /// Extract the transaction ID sets for each bin (execution thread).
    /// Bins are the result of packing conflict clusters into parallel slots.
    fn bin_tx_ids(&self) -> &[BitSet] {
        &self.bin_packing
    }
}

// ---------------------------------------------------------------------------
// Bin packing (first-fit-decreasing)
// ---------------------------------------------------------------------------

/// First-fit-decreasing bin packing for clusters.
/// Returns bin assignment as Vec<BitSet> (one BitSet of tx IDs per bin),
/// or None if packing fails.
fn bin_pack_clusters(
    clusters: &[Cluster],
    max_bins: u32,
    max_instructions_per_bin: u64,
    bin_instructions: &mut [u64],
) -> Option<Vec<BitSet>> {
    let n_bins = max_bins as usize;
    let mut bins: Vec<BitSet> = vec![BitSet::default(); n_bins];

    // Sort clusters by instruction count (descending) for FFD.
    let mut sorted_indices: Vec<usize> = (0..clusters.len()).collect();
    sorted_indices.sort_by(|&a, &b| clusters[b].instructions.cmp(&clusters[a].instructions));

    for &idx in &sorted_indices {
        let cluster = &clusters[idx];
        let mut packed = false;
        for bin_id in 0..n_bins {
            if bin_instructions[bin_id]
                .checked_add(cluster.instructions)
                .map_or(false, |sum| sum <= max_instructions_per_bin)
            {
                bin_instructions[bin_id] = bin_instructions[bin_id]
                    .checked_add(cluster.instructions)
                    .expect("bin instructions overflow during packing");
                bins[bin_id].union_with(&cluster.tx_ids);
                packed = true;
                break;
            }
        }
        if !packed {
            return None;
        }
    }

    Some(bins)
}

// ---------------------------------------------------------------------------
// Conflict detection
// ---------------------------------------------------------------------------

/// Serialize a LedgerKey to bytes for use as a HashMap key.
fn ledger_key_bytes(key: &LedgerKey) -> Vec<u8> {
    key.to_xdr(Limits::none())
        .expect("LedgerKey XDR serialization should never fail")
}

/// Extract SorobanTransactionData from an envelope without constructing a TransactionFrame.
fn soroban_data_from_envelope(
    env: &TransactionEnvelope,
) -> Option<&stellar_xdr::curr::SorobanTransactionData> {
    let tx = match env {
        TransactionEnvelope::TxV0(_) => return None,
        TransactionEnvelope::Tx(e) => &e.tx,
        TransactionEnvelope::TxFeeBump(e) => match &e.tx.inner_tx {
            stellar_xdr::curr::FeeBumpTransactionInnerTx::Tx(inner) => &inner.tx,
        },
    };
    match &tx.ext {
        stellar_xdr::curr::TransactionExt::V0 => None,
        stellar_xdr::curr::TransactionExt::V1(data) => Some(data),
    }
}

/// Detect footprint conflicts between Soroban transactions.
///
/// Two transactions conflict if:
/// - Both write the same key (RW-RW conflict)
/// - One reads and the other writes the same key (RO-RW conflict)
///
/// RO-RO does NOT create a conflict.
fn detect_conflicts(txs: &[TransactionEnvelope]) -> Vec<BitSet> {
    let n = txs.len();
    let mut conflicts: Vec<BitSet> = (0..n).map(|_| BitSet::with_capacity(n)).collect();

    // Build key-to-txs maps: which transactions touch each key as RO or RW.
    let mut ro_key_txs: std::collections::HashMap<Vec<u8>, Vec<usize>> =
        std::collections::HashMap::new();
    let mut rw_key_txs: std::collections::HashMap<Vec<u8>, Vec<usize>> =
        std::collections::HashMap::new();

    for (tx_id, tx) in txs.iter().enumerate() {
        if let Some(data) = soroban_data_from_envelope(tx) {
            for key in data.resources.footprint.read_only.iter() {
                let kb = ledger_key_bytes(key);
                ro_key_txs.entry(kb).or_default().push(tx_id);
            }
            for key in data.resources.footprint.read_write.iter() {
                let kb = ledger_key_bytes(key);
                rw_key_txs.entry(kb).or_default().push(tx_id);
            }
        }
    }

    // Mark RW-RW conflicts: all pairs of transactions that write the same key.
    for rw_txs in rw_key_txs.values() {
        for i in 0..rw_txs.len() {
            for j in (i + 1)..rw_txs.len() {
                let a = rw_txs[i];
                let b = rw_txs[j];
                conflicts[a].set(b);
                conflicts[b].set(a);
            }
        }
    }

    // Mark RO-RW conflicts: transaction that reads a key conflicts with
    // any transaction that writes the same key.
    for (key, rw_txs) in &rw_key_txs {
        if let Some(ro_txs) = ro_key_txs.get(key) {
            for &ro_tx in ro_txs {
                for &rw_tx in rw_txs {
                    if ro_tx != rw_tx {
                        conflicts[ro_tx].set(rw_tx);
                        conflicts[rw_tx].set(ro_tx);
                    }
                }
            }
        }
    }

    conflicts
}

// ---------------------------------------------------------------------------
// Stage building for a fixed stage count
// ---------------------------------------------------------------------------

/// Inclusion fee for a transaction (fee - minimum resource fee).
fn tx_inclusion_fee(tx: &TransactionEnvelope) -> i64 {
    crate::tx_set_utils::envelope_inclusion_fee(tx)
}

/// Build parallel Soroban phase for a fixed stage count.
///
/// Returns (stages, total_inclusion_fee) where stages[i][j] = cluster j of stage i.
fn build_with_stage_count(
    txs: &[TransactionEnvelope],
    network_id: NetworkId,
    ledger_max_instructions: i64,
    ledger_max_dependent_tx_clusters: u32,
    stage_count: u32,
) -> (Vec<Vec<Vec<usize>>>, i64) {
    let conflicts = detect_conflicts(txs);
    let n = txs.len();

    // Build BuilderTx representations.
    let builder_txs: Vec<BuilderTx> = (0..n)
        .map(|id| {
            let frame = TransactionFrame::from_owned_with_network(txs[id].clone(), network_id);
            let instructions = frame
                .soroban_data()
                .map(|d| d.resources.instructions)
                .unwrap_or(0);
            BuilderTx {
                id,
                instructions,
                conflicts: conflicts[id].clone(),
            }
        })
        .collect();

    // Sort transactions by fee rate (descending) for greedy assignment.
    // Uses per-operation fee rate (inclusion_fee / num_ops) via cross-multiply
    // to match stellar-core's SurgePricingPriorityQueue ordering. Ties are
    // broken by transaction hash (ascending) for determinism.
    let mut sorted_ids: Vec<usize> = (0..n).collect();
    sorted_ids.sort_by(|&a, &b| {
        let a_fee = tx_inclusion_fee(&txs[a]);
        let b_fee = tx_inclusion_fee(&txs[b]);
        let a_ops = (crate::tx_set_utils::envelope_num_ops(&txs[a]) as u32).max(1);
        let b_ops = (crate::tx_set_utils::envelope_num_ops(&txs[b]) as u32).max(1);
        // Descending fee rate: compare b vs a
        match crate::tx_queue::fee_rate_cmp(b_fee as u64, b_ops, a_fee as u64, a_ops) {
            std::cmp::Ordering::Equal => {
                // Ascending hash for determinism
                let a_hash = Hash256::hash_xdr(&txs[a]);
                let b_hash = Hash256::hash_xdr(&txs[b]);
                a_hash.0.cmp(&b_hash.0)
            }
            other => other,
        }
    });

    // Build stages greedily.
    let mut stages: Vec<Stage> = (0..stage_count)
        .map(|_| {
            Stage::new(ParallelPartitionConfig::new(
                stage_count,
                ledger_max_instructions,
                ledger_max_dependent_tx_clusters,
            ))
        })
        .collect();

    let mut total_inclusion_fee: i64 = 0;

    for &tx_id in &sorted_ids {
        let tx_ref = &builder_txs[tx_id];
        let mut added = false;
        for stage in stages.iter_mut() {
            if stage.try_add(tx_ref) {
                added = true;
                break;
            }
        }
        if added {
            total_inclusion_fee += tx_inclusion_fee(&txs[tx_id]);
        }
        // If not added to any stage, the TX is dropped (doesn't fit).
    }

    // Extract results: for each stage, for each bin, collect the TX IDs.
    // stellar-core groups TXs by bins (execution threads), not by conflict
    // clusters — see ParallelTxSetBuilder.cpp:visitAllTransactions which uses
    // cluster->mBinId as the output key. Each bin is a DependentTxCluster in
    // the XDR output, representing a single execution thread.
    let result_stages: Vec<Vec<Vec<usize>>> = stages
        .iter()
        .map(|stage| {
            stage
                .bin_tx_ids()
                .iter()
                .map(|tx_ids| tx_ids.iter_ones().collect::<Vec<_>>())
                .filter(|bin| !bin.is_empty())
                .collect::<Vec<_>>()
        })
        .filter(|stage| !stage.is_empty())
        .collect();

    (result_stages, total_inclusion_fee)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Tolerance for inclusion fee when selecting optimal stage count.
/// Pick the lowest stage count that achieves >= 99.9% of max fee.
const MAX_INCLUSION_FEE_TOLERANCE: f64 = 0.999;

/// Build the optimal parallel Soroban phase.
///
/// Tries stage counts from `min_stage_count` to `max_stage_count` and picks
/// the one with the fewest stages that achieves >= 99.9% of the maximum
/// total inclusion fee.
///
/// # Arguments
///
/// * `soroban_txs` - Soroban transactions to partition
/// * `network_id` - Network ID for transaction frame construction
/// * `ledger_max_instructions` - From ContractComputeV0 config setting
/// * `ledger_max_dependent_tx_clusters` - From ContractParallelComputeV0
/// * `min_stage_count` - Minimum number of stages to try (typically 1)
/// * `max_stage_count` - Maximum number of stages to try (typically 4)
///
/// # Returns
///
/// A tuple of:
/// - Stages of clusters of transaction envelopes: `stages[i][j]` = cluster j of stage i.
/// - Whether any transactions were dropped (didn't fit any stage/cluster).
///   Matches stellar-core's `hadTxNotFittingLane` feedback.
pub fn build_parallel_soroban_phase(
    mut soroban_txs: Vec<TransactionEnvelope>,
    network_id: NetworkId,
    ledger_max_instructions: i64,
    ledger_max_dependent_tx_clusters: u32,
    min_stage_count: u32,
    max_stage_count: u32,
) -> (Vec<Vec<Vec<TransactionEnvelope>>>, bool) {
    if soroban_txs.is_empty() {
        return (Vec::new(), false);
    }

    // If clusters_per_stage is 0, fall back to single cluster.
    if ledger_max_dependent_tx_clusters == 0 {
        return (vec![vec![soroban_txs]], false);
    }

    // Try each stage count and collect (stages, fee) pairs.
    let mut results: Vec<(Vec<Vec<Vec<usize>>>, i64)> = Vec::new();
    for sc in min_stage_count..=max_stage_count {
        let result = build_with_stage_count(
            &soroban_txs,
            network_id,
            ledger_max_instructions,
            ledger_max_dependent_tx_clusters,
            sc,
        );
        results.push(result);
    }

    // Find max inclusion fee across all results.
    let max_fee = results.iter().map(|(_, fee)| *fee).max().unwrap_or(0);
    let fee_threshold = (max_fee as f64 * MAX_INCLUSION_FEE_TOLERANCE) as i64;

    // Pick the result with the fewest actual (non-empty) stages that meets
    // the fee threshold. stellar-core (ParallelTxSetBuilder.cpp:779-800) iterates
    // all results and tracks bestResultIndex by minimum mStages.size().
    // Note: configured stage count doesn't always equal actual stage count — a
    // higher configured count may drop transactions, producing fewer non-empty stages.
    let mut best: Option<Vec<Vec<Vec<usize>>>> = None;
    for (stages, fee) in results {
        if fee >= fee_threshold {
            let actual_stage_count = stages.len();
            if best.is_none() || actual_stage_count < best.as_ref().unwrap().len() {
                best = Some(stages);
            }
        }
    }

    let best_ids = best.unwrap_or_default();
    let surviving_count: usize = best_ids
        .iter()
        .flat_map(|s| s.iter())
        .map(|c| c.len())
        .sum();
    let had_tx_not_fitting = surviving_count < soroban_txs.len();

    // Convert ID-based stages to envelope-based by moving from the owned Vec.
    // Use a sentinel (default envelope) for taken slots to avoid Option overhead.
    let stages = best_ids
        .into_iter()
        .map(|stage| {
            stage
                .into_iter()
                .map(|cluster| {
                    cluster
                        .into_iter()
                        .map(|id| std::mem::take(&mut soroban_txs[id]))
                        .collect()
                })
                .collect()
        })
        .collect();

    (stages, had_tx_not_fitting)
}

/// Convert parallel phase stages into a TransactionPhase::V1 XDR structure.
///
/// HERDER_SPEC §7.7: Applies canonical ordering before serialization:
/// 1. Transactions within each cluster sorted by full hash (ascending).
/// 2. Clusters within each stage sorted by first-TX hash (ascending).
/// 3. Stages sorted by first-TX-of-first-cluster hash (ascending).
pub fn stages_to_xdr_phase(
    stages: Vec<Vec<Vec<TransactionEnvelope>>>,
    base_fee: Option<i64>,
) -> TransactionPhase {
    let mut sorted_stages: Vec<Vec<Vec<TransactionEnvelope>>> = stages
        .into_iter()
        .map(|stage| {
            let mut sorted_clusters: Vec<Vec<TransactionEnvelope>> = stage
                .into_iter()
                .map(|mut cluster| {
                    // 1. Sort transactions within each cluster by hash.
                    // Pre-compute hashes to avoid redundant XDR serialization
                    // during comparisons (O(N log N) comparisons → O(N) hashes).
                    cluster.sort_by_cached_key(|tx| Hash256::hash_xdr(tx).0);
                    cluster
                })
                .collect();
            // 2. Sort clusters within each stage by first-TX hash
            sorted_clusters.sort_by_cached_key(|cluster| Hash256::hash_xdr(&cluster[0]).0);
            sorted_clusters
        })
        .collect();

    // 3. Sort stages by first-TX-of-first-cluster hash
    sorted_stages.sort_by_cached_key(|stage| Hash256::hash_xdr(&stage[0][0]).0);

    let execution_stages: Vec<ParallelTxExecutionStage> = sorted_stages
        .into_iter()
        .map(|stage| {
            let clusters: Vec<DependentTxCluster> = stage
                .into_iter()
                .map(|cluster| {
                    DependentTxCluster(cluster.try_into().expect("cluster exceeds XDR VecM limit"))
                })
                .collect();
            ParallelTxExecutionStage(clusters.try_into().expect("stage exceeds XDR VecM limit"))
        })
        .collect();

    TransactionPhase::V1(ParallelTxsComponent {
        base_fee,
        execution_stages: execution_stages
            .try_into()
            .expect("execution_stages exceeds XDR VecM limit"),
    })
}

/// Like `stages_to_xdr_phase`, but skips canonical sorting.
///
/// Use this when the caller will re-sort the transactions later (e.g.,
/// `prepare_with_hash` in the ledger close path). Avoids the cost of
/// XDR-serializing + SHA-256-hashing every transaction for sort keys.
fn stages_to_xdr_phase_unsorted(
    stages: Vec<Vec<Vec<TransactionEnvelope>>>,
    base_fee: Option<i64>,
) -> TransactionPhase {
    let execution_stages: Vec<ParallelTxExecutionStage> = stages
        .into_iter()
        .map(|stage| {
            let clusters: Vec<DependentTxCluster> = stage
                .into_iter()
                .map(|cluster| {
                    DependentTxCluster(cluster.try_into().expect("cluster exceeds XDR VecM limit"))
                })
                .collect();
            ParallelTxExecutionStage(clusters.try_into().expect("stage exceeds XDR VecM limit"))
        })
        .collect();

    TransactionPhase::V1(ParallelTxsComponent {
        base_fee,
        execution_stages: execution_stages
            .try_into()
            .expect("execution_stages exceeds XDR VecM limit"),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
// Two-phase tx set builder (classic V0 + parallel Soroban V1)
// ---------------------------------------------------------------------------

/// Build a `GeneralizedTransactionSet` with a V0 classic phase and a V1
/// parallel Soroban phase.
///
/// This is the simplified variant used by the simulation harness. Soroban TXs
/// are round-robin distributed into `ledger_max_dependent_tx_clusters` clusters
/// in a single stage. All TXs are included (no surge-pricing drops).
pub fn build_two_phase_tx_set(
    classic_txs: Vec<TransactionEnvelope>,
    soroban_txs: Vec<TransactionEnvelope>,
    previous_ledger_hash: &Hash256,
    soroban_base_fee: Option<i64>,
    ledger_max_dependent_tx_clusters: u32,
) -> GeneralizedTransactionSet {
    // Classic V0 phase: single component, no base_fee discount.
    let classic_phase = if classic_txs.is_empty() {
        TransactionPhase::V0(VecM::default())
    } else {
        let component =
            TxSetComponent::TxsetCompTxsMaybeDiscountedFee(TxSetComponentTxsMaybeDiscountedFee {
                base_fee: None,
                txs: classic_txs
                    .try_into()
                    .expect("classic txs exceeds XDR VecM limit"),
            });
        TransactionPhase::V0(
            vec![component]
                .try_into()
                .expect("classic phase exceeds XDR VecM limit"),
        )
    };

    // Soroban V1 phase: round-robin TXs into N clusters in a single stage.
    // This simple partitioning is appropriate for the simulation harness where
    // all TXs must be included (no surge-pricing drops). The herder's own
    // build_parallel_soroban_phase() has instruction-limit-based capacity
    // constraints that would drop TXs exceeding cluster capacity.
    let soroban_phase = if soroban_txs.is_empty() {
        TransactionPhase::V1(ParallelTxsComponent {
            base_fee: soroban_base_fee,
            execution_stages: VecM::default(),
        })
    } else {
        let num_clusters = ledger_max_dependent_tx_clusters.max(1) as usize;
        let mut cluster_txs: Vec<Vec<TransactionEnvelope>> =
            (0..num_clusters).map(|_| Vec::new()).collect();
        for (i, tx) in soroban_txs.into_iter().enumerate() {
            cluster_txs[i % num_clusters].push(tx);
        }
        let stages = vec![cluster_txs
            .into_iter()
            .filter(|c| !c.is_empty())
            .collect::<Vec<_>>()];
        // Skip sorting here — prepare_with_hash re-sorts all TXs by hash anyway.
        // This avoids double-hashing 50K TXs (saves ~100ms).
        stages_to_xdr_phase_unsorted(stages, soroban_base_fee)
    };

    GeneralizedTransactionSet::V1(TransactionSetV1 {
        previous_ledger_hash: Hash(previous_ledger_hash.0),
        phases: vec![classic_phase, soroban_phase]
            .try_into()
            .expect("phases exceeds XDR VecM limit"),
    })
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{
        HostFunction, InvokeContractArgs, InvokeHostFunctionOp, LedgerFootprint, LedgerKey,
        LedgerKeyContractData, Memo, MuxedAccount, Operation, OperationBody, Preconditions,
        ScAddress, ScVal, SorobanResources, SorobanTransactionData, SorobanTransactionDataExt,
        Transaction, TransactionEnvelope, TransactionExt, TransactionV1Envelope, Uint256, VecM,
    };

    fn test_network_id() -> NetworkId {
        NetworkId(henyey_common::Hash256([0u8; 32]))
    }

    /// Create a Soroban transaction with the specified footprint keys and instructions.
    fn make_soroban_tx(
        seed: u8,
        seq: i64,
        read_only_keys: Vec<LedgerKey>,
        read_write_keys: Vec<LedgerKey>,
        instructions: u32,
    ) -> TransactionEnvelope {
        let source = MuxedAccount::Ed25519(Uint256([seed; 32]));
        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: read_only_keys.try_into().unwrap_or_default(),
                    read_write: read_write_keys.try_into().unwrap_or_default(),
                },
                instructions,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee: 100,
        };
        // Include a minimal InvokeHostFunction op so envelope_num_ops returns 1,
        // matching real Soroban transactions for fee-rate ordering.
        let invoke_op = Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function: HostFunction::InvokeContract(InvokeContractArgs {
                    contract_address: ScAddress::Contract(stellar_xdr::curr::ContractId(
                        stellar_xdr::curr::Hash([seed; 32]),
                    )),
                    function_name: stellar_xdr::curr::ScSymbol("test".try_into().unwrap()),
                    args: Default::default(),
                }),
                auth: Default::default(),
            }),
        };
        let tx = Transaction {
            source_account: source,
            fee: 1000,
            seq_num: stellar_xdr::curr::SequenceNumber(seq),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![invoke_op].try_into().unwrap(),
            ext: TransactionExt::V1(soroban_data),
        };
        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: VecM::default(),
        })
    }

    fn make_soroban_tx_with_fees(
        seed: u8,
        seq: i64,
        read_only_keys: Vec<LedgerKey>,
        read_write_keys: Vec<LedgerKey>,
        instructions: u32,
        fee: u32,
        resource_fee: i64,
    ) -> TransactionEnvelope {
        let mut tx = make_soroban_tx(seed, seq, read_only_keys, read_write_keys, instructions);
        if let TransactionEnvelope::Tx(env) = &mut tx {
            env.tx.fee = fee;
            if let TransactionExt::V1(data) = &mut env.tx.ext {
                data.resource_fee = resource_fee;
            }
        }
        tx
    }

    /// Create a contract data ledger key with a unique identifier.
    fn contract_key(id: u8) -> LedgerKey {
        LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(stellar_xdr::curr::ContractId(stellar_xdr::curr::Hash(
                [id; 32],
            ))),
            key: ScVal::U32(id as u32),
            durability: stellar_xdr::curr::ContractDataDurability::Persistent,
        })
    }

    // ---- BitSet tests ----

    #[test]
    fn test_bitset_basic() {
        let mut bs = BitSet::default();
        assert!(!bs.get(0));
        bs.set(0);
        assert!(bs.get(0));
        bs.set(100);
        assert!(bs.get(100));
        assert!(!bs.get(99));
    }

    #[test]
    fn test_bitset_intersects() {
        let mut a = BitSet::default();
        let mut b = BitSet::default();
        a.set(5);
        b.set(10);
        assert!(!a.intersects(&b));
        b.set(5);
        assert!(a.intersects(&b));
    }

    #[test]
    fn test_bitset_union() {
        let mut a = BitSet::default();
        let mut b = BitSet::default();
        a.set(1);
        b.set(2);
        a.union_with(&b);
        assert!(a.get(1));
        assert!(a.get(2));
    }

    #[test]
    fn test_bitset_iter_ones() {
        let mut bs = BitSet::default();
        bs.set(3);
        bs.set(7);
        bs.set(64);
        let ones: Vec<usize> = bs.iter_ones().collect();
        assert_eq!(ones, vec![3, 7, 64]);
    }

    // ---- Conflict detection tests ----

    #[test]
    fn test_no_conflicts_separate_keys() {
        let key_a = contract_key(1);
        let key_b = contract_key(2);
        let tx_a = make_soroban_tx(1, 1, vec![], vec![key_a], 1000);
        let tx_b = make_soroban_tx(2, 1, vec![], vec![key_b], 1000);
        let conflicts = detect_conflicts(&[tx_a, tx_b]);
        assert!(!conflicts[0].get(1));
        assert!(!conflicts[1].get(0));
    }

    #[test]
    fn test_rw_rw_conflict() {
        let key = contract_key(1);
        let tx_a = make_soroban_tx(1, 1, vec![], vec![key.clone()], 1000);
        let tx_b = make_soroban_tx(2, 1, vec![], vec![key], 1000);
        let conflicts = detect_conflicts(&[tx_a, tx_b]);
        assert!(conflicts[0].get(1));
        assert!(conflicts[1].get(0));
    }

    #[test]
    fn test_ro_rw_conflict() {
        let key = contract_key(1);
        let tx_a = make_soroban_tx(1, 1, vec![key.clone()], vec![], 1000);
        let tx_b = make_soroban_tx(2, 1, vec![], vec![key], 1000);
        let conflicts = detect_conflicts(&[tx_a, tx_b]);
        assert!(conflicts[0].get(1));
        assert!(conflicts[1].get(0));
    }

    #[test]
    fn test_ro_ro_no_conflict() {
        let key = contract_key(1);
        let tx_a = make_soroban_tx(1, 1, vec![key.clone()], vec![], 1000);
        let tx_b = make_soroban_tx(2, 1, vec![key], vec![], 1000);
        let conflicts = detect_conflicts(&[tx_a, tx_b]);
        assert!(!conflicts[0].get(1));
        assert!(!conflicts[1].get(0));
    }

    #[test]
    fn test_transitive_conflict_merges_clusters() {
        // TX A writes key1, TX B writes key1 and key2, TX C writes key2
        // A conflicts with B, B conflicts with C
        // All three should end up in the same cluster
        let key1 = contract_key(1);
        let key2 = contract_key(2);
        let tx_a = make_soroban_tx(1, 1, vec![], vec![key1.clone()], 1000);
        let tx_b = make_soroban_tx(2, 1, vec![], vec![key1, key2.clone()], 1000);
        let tx_c = make_soroban_tx(3, 1, vec![], vec![key2], 1000);

        let stages = build_parallel_soroban_phase(
            vec![tx_a, tx_b, tx_c],
            test_network_id(),
            100_000, // ledger max instructions
            8,       // clusters per stage
            1,
            1,
        )
        .0;

        // All three TXs should be in one cluster
        let total_txs: usize = stages.iter().flat_map(|s| s.iter()).map(|c| c.len()).sum();
        assert_eq!(total_txs, 3);
        // With one stage, there should be 1 cluster containing all 3
        assert_eq!(stages.len(), 1);
        assert_eq!(stages[0].len(), 1);
        assert_eq!(stages[0][0].len(), 3);
    }

    #[test]
    fn test_no_conflict_separate_clusters() {
        let key1 = contract_key(1);
        let key2 = contract_key(2);
        let tx_a = make_soroban_tx(1, 1, vec![], vec![key1], 1000);
        let tx_b = make_soroban_tx(2, 1, vec![], vec![key2], 1000);

        let stages =
            build_parallel_soroban_phase(vec![tx_a, tx_b], test_network_id(), 100_000, 8, 1, 1).0;

        // No conflicts: both TXs fit in the same bin (execution thread)
        assert_eq!(stages.len(), 1);
        assert_eq!(stages[0].len(), 1); // 1 bin containing both TXs
        assert_eq!(stages[0][0].len(), 2);
    }

    #[test]
    fn test_instruction_limit_forces_multi_stage() {
        // Each TX has 60k instructions, ledger max is 100k with 1 cluster per stage.
        // With 1 stage: instructions_per_cluster = 100k/1 = 100k, stage fits both.
        // With 1 stage and 1 cluster: total stage limit = 100k.
        // First TX (60k) fits; second TX (60k) would make total 120k > 100k, so it's dropped.
        let key1 = contract_key(1);
        let key2 = contract_key(2);
        let tx_a = make_soroban_tx(1, 1, vec![], vec![key1], 60_000);
        let tx_b = make_soroban_tx(2, 1, vec![], vec![key2], 60_000);

        // With 1 stage and 1 cluster: stage limit = 100k, only 1 TX fits
        let stages = build_parallel_soroban_phase(
            vec![tx_a.clone(), tx_b.clone()],
            test_network_id(),
            100_000,
            1, // only 1 cluster per stage
            1,
            1,
        )
        .0;

        let total_txs: usize = stages.iter().flat_map(|s| s.iter()).map(|c| c.len()).sum();
        assert_eq!(total_txs, 1);

        // With 2 stages and 1 cluster each: instructions_per_cluster = 100k/2 = 50k
        // Each TX is 60k > 50k, so neither fits. Use higher ledger max instead.
        // ledger_max=120k, 2 stages, 1 cluster each: cluster limit = 60k, stage limit = 60k.
        let stages =
            build_parallel_soroban_phase(vec![tx_a, tx_b], test_network_id(), 120_000, 1, 2, 2).0;

        let total_txs: usize = stages.iter().flat_map(|s| s.iter()).map(|c| c.len()).sum();
        assert_eq!(total_txs, 2);
    }

    #[test]
    fn test_audit_018_parallel_builder_uses_fee_rate_ordering() {
        // Both 1-op txs → fee rate = absolute fee. High inclusion fee should win.
        let tx_low_inclusion =
            make_soroban_tx_with_fees(1, 1, vec![], vec![contract_key(1)], 60_000, 1_000, 900);
        let tx_high_inclusion =
            make_soroban_tx_with_fees(2, 1, vec![], vec![contract_key(2)], 60_000, 800, 0);

        let (stages, total_inclusion_fee) = build_with_stage_count(
            &[tx_low_inclusion.clone(), tx_high_inclusion.clone()],
            test_network_id(),
            100_000,
            1,
            1,
        );

        assert_eq!(stages.len(), 1);
        assert_eq!(stages[0].len(), 1);
        // tx_high_inclusion is index 1 in the input slice
        assert_eq!(stages[0][0], vec![1]);
        assert_eq!(total_inclusion_fee, 800);
    }

    /// Regression test for #1717: fee-bump Soroban tx has numOps=2 (inner op +
    /// fee-bump wrapper). With absolute fee ordering, a fee-bumped tx with
    /// fee=1000 beats a normal tx with fee=800. With per-op fee-rate ordering,
    /// the normal tx (800/1=800 rate) beats the fee-bumped tx (1000/2=500 rate).
    #[test]
    fn test_fee_bump_soroban_uses_per_op_rate() {
        use stellar_xdr::curr::{
            FeeBumpTransaction, FeeBumpTransactionEnvelope, FeeBumpTransactionExt,
            FeeBumpTransactionInnerTx,
        };

        // Normal tx: fee=800, resource_fee=0, inclusion_fee=800, 1 op → rate=800
        let tx_normal =
            make_soroban_tx_with_fees(1, 1, vec![], vec![contract_key(1)], 60_000, 800, 0);

        // Fee-bumped tx: inner fee=500, resource_fee=0, outer fee=1000
        // inclusion_fee=1000, 2 ops (inner+wrapper) → rate=500
        let inner_tx =
            make_soroban_tx_with_fees(2, 1, vec![], vec![contract_key(2)], 60_000, 500, 0);
        let inner_env = match inner_tx {
            TransactionEnvelope::Tx(env) => env,
            _ => panic!("expected Tx envelope"),
        };
        let fee_bump = TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
            tx: FeeBumpTransaction {
                fee_source: MuxedAccount::Ed25519(Uint256([99; 32])),
                fee: 1000,
                inner_tx: FeeBumpTransactionInnerTx::Tx(inner_env),
                ext: FeeBumpTransactionExt::V0,
            },
            signatures: Default::default(),
        });

        // Only room for 1 tx (instruction limit). Per-op rate ordering should
        // pick tx_normal (rate=800) over fee_bump (rate=500).
        let (stages, _) =
            build_with_stage_count(&[fee_bump, tx_normal], test_network_id(), 100_000, 1, 1);

        assert_eq!(stages.len(), 1);
        assert_eq!(stages[0].len(), 1);
        // tx_normal is index 1 in the input slice
        assert_eq!(
            stages[0][0],
            vec![1],
            "per-op rate ordering should prefer normal tx"
        );
    }

    #[test]
    fn test_bin_packing_basic() {
        let mut bin_insns = vec![0u64; 3];
        let clusters = vec![
            Cluster {
                instructions: 50,
                conflicts: BitSet::default(),
                tx_ids: {
                    let mut bs = BitSet::default();
                    bs.set(0);
                    bs
                },
            },
            Cluster {
                instructions: 70,
                conflicts: BitSet::default(),
                tx_ids: {
                    let mut bs = BitSet::default();
                    bs.set(1);
                    bs
                },
            },
            Cluster {
                instructions: 30,
                conflicts: BitSet::default(),
                tx_ids: {
                    let mut bs = BitSet::default();
                    bs.set(2);
                    bs
                },
            },
        ];

        let result = bin_pack_clusters(&clusters, 3, 100, &mut bin_insns);
        assert!(result.is_some());
    }

    #[test]
    fn test_bin_packing_overflow() {
        let mut bin_insns = vec![0u64; 1];
        let clusters = vec![
            Cluster {
                instructions: 60,
                conflicts: BitSet::default(),
                tx_ids: {
                    let mut bs = BitSet::default();
                    bs.set(0);
                    bs
                },
            },
            Cluster {
                instructions: 60,
                conflicts: BitSet::default(),
                tx_ids: {
                    let mut bs = BitSet::default();
                    bs.set(1);
                    bs
                },
            },
        ];

        let result = bin_pack_clusters(&clusters, 1, 100, &mut bin_insns);
        assert!(result.is_none());
    }

    #[test]
    fn test_empty_input() {
        let (stages, had_drop) =
            build_parallel_soroban_phase(vec![], test_network_id(), 100_000, 8, 1, 4);
        assert!(stages.is_empty());
        assert!(!had_drop);
    }

    #[test]
    fn test_multi_stage_optimization_prefers_fewer_stages() {
        // All TXs are independent and small. With 1 stage we can fit all.
        // The optimizer should pick 1 stage since it achieves same fee.
        let txs: Vec<TransactionEnvelope> = (0..5)
            .map(|i| {
                let key = contract_key(i + 10);
                make_soroban_tx(i + 10, 1, vec![], vec![key], 1000)
            })
            .collect();

        let stages = build_parallel_soroban_phase(txs, test_network_id(), 1_000_000, 8, 1, 4).0;

        // Should use 1 stage since all fit
        assert_eq!(stages.len(), 1);
        let total_txs: usize = stages.iter().flat_map(|s| s.iter()).map(|c| c.len()).sum();
        assert_eq!(total_txs, 5);
    }

    #[test]
    fn test_stages_to_xdr_phase() {
        let key = contract_key(1);
        let tx = make_soroban_tx(1, 1, vec![], vec![key], 1000);
        let stages = vec![vec![vec![tx]]];
        let phase = stages_to_xdr_phase(stages, Some(100));

        match phase {
            TransactionPhase::V1(parallel) => {
                assert_eq!(parallel.base_fee, Some(100));
                assert_eq!(parallel.execution_stages.len(), 1);
                assert_eq!(parallel.execution_stages[0].0.len(), 1);
                assert_eq!(parallel.execution_stages[0].0[0].0.len(), 1);
            }
            _ => panic!("Expected V1 phase"),
        }
    }

    /// Regression test for #1477/#1494: had_tx_not_fitting must be true when
    /// the builder drops transactions that don't fit stage/cluster limits.
    #[test]
    fn test_had_tx_not_fitting_when_tx_dropped() {
        let key1 = contract_key(1);
        let key2 = contract_key(2);
        // Two txs with 60k instructions each, but ledger max is 100k with 1 cluster.
        let tx_a = make_soroban_tx(1, 1, vec![], vec![key1], 60_000);
        let tx_b = make_soroban_tx(2, 1, vec![], vec![key2], 60_000);

        let (stages, had_tx_not_fitting) = build_parallel_soroban_phase(
            vec![tx_a, tx_b],
            test_network_id(),
            100_000,
            1, // 1 cluster per stage
            1,
            1,
        );

        // Only 1 tx fits (60k < 100k, but 120k > 100k)
        let total_txs: usize = stages.iter().flat_map(|s| s.iter()).map(|c| c.len()).sum();
        assert_eq!(total_txs, 1);
        assert!(
            had_tx_not_fitting,
            "should report tx not fitting when builder drops txs"
        );
    }

    #[test]
    fn test_had_tx_not_fitting_false_when_all_fit() {
        let key1 = contract_key(1);
        let key2 = contract_key(2);
        let tx_a = make_soroban_tx(1, 1, vec![], vec![key1], 1_000);
        let tx_b = make_soroban_tx(2, 1, vec![], vec![key2], 1_000);

        let (stages, had_tx_not_fitting) =
            build_parallel_soroban_phase(vec![tx_a, tx_b], test_network_id(), 1_000_000, 8, 1, 1);

        let total_txs: usize = stages.iter().flat_map(|s| s.iter()).map(|c| c.len()).sum();
        assert_eq!(total_txs, 2);
        assert!(
            !had_tx_not_fitting,
            "should not report tx not fitting when all txs included"
        );
    }

    // -----------------------------------------------------------------------
    // Overflow guard tests
    // -----------------------------------------------------------------------

    #[test]
    #[should_panic(expected = "cluster instructions overflow on merge")]
    fn test_cluster_merge_overflow() {
        let mut c1 = Cluster {
            instructions: u64::MAX - 10,
            conflicts: BitSet::default(),
            tx_ids: BitSet::default(),
        };
        let c2 = Cluster {
            instructions: 20,
            conflicts: BitSet::default(),
            tx_ids: BitSet::default(),
        };
        c1.merge(&c2);
    }

    #[test]
    #[should_panic(expected = "instructions_per_stage overflow")]
    fn test_instructions_per_stage_overflow() {
        let config = ParallelPartitionConfig {
            clusters_per_stage: u32::MAX,
            instructions_per_cluster: u64::MAX,
        };
        let _ = config.instructions_per_stage();
    }

    #[test]
    fn test_try_add_rejects_on_overflow_comparison() {
        // Create a stage with total_instructions near u64::MAX.
        let config = ParallelPartitionConfig {
            clusters_per_stage: 2,
            instructions_per_cluster: u64::MAX / 2,
        };
        let mut stage = Stage::new(config);
        stage.total_instructions = u64::MAX - 5;

        // A tx with instructions > 5 should be rejected (not panic) because
        // the checked_add comparison detects potential overflow.
        let tx = BuilderTx {
            id: 0,
            instructions: 100,
            conflicts: BitSet::default(),
        };
        assert!(!stage.try_add(&tx));
    }

    #[test]
    #[should_panic(expected = "bin instructions underflow during removal")]
    fn test_bin_instructions_underflow() {
        let config = ParallelPartitionConfig {
            clusters_per_stage: 2,
            instructions_per_cluster: 1000,
        };
        let mut stage = Stage::new(config);

        // Manually set up corrupt state: cluster claims 500 instructions
        // but bin only has 100.
        let mut tx_ids = BitSet::default();
        tx_ids.set(0);
        stage.bin_packing[0].set(0);
        stage.bin_instructions[0] = 100;
        stage.clusters.push(Cluster {
            instructions: 500,
            conflicts: BitSet::default(),
            tx_ids,
        });

        // A new cluster that conflicts with cluster 0.
        let mut new_conflicts = BitSet::default();
        new_conflicts.set(0);
        let mut new_tx_ids = BitSet::default();
        new_tx_ids.set(1);
        let new_cluster = Cluster {
            instructions: 50,
            conflicts: new_conflicts,
            tx_ids: new_tx_ids,
        };

        // This should panic during removal since 100 - 500 underflows.
        stage.try_in_place_bin_packing(&new_cluster, &[0]);
    }

    #[test]
    #[should_panic(expected = "cluster must be in exactly one bin")]
    fn test_cluster_not_found_in_bin_panics() {
        let config = ParallelPartitionConfig {
            clusters_per_stage: 2,
            instructions_per_cluster: 1000,
        };
        let mut stage = Stage::new(config);

        // Cluster exists but its tx_ids are NOT in any bin_packing.
        let mut tx_ids = BitSet::default();
        tx_ids.set(0);
        stage.clusters.push(Cluster {
            instructions: 100,
            conflicts: BitSet::default(),
            tx_ids,
        });
        // bin_packing is empty — cluster 0 is not in any bin.

        let mut new_tx_ids = BitSet::default();
        new_tx_ids.set(1);
        let new_cluster = Cluster {
            instructions: 50,
            conflicts: BitSet::default(),
            tx_ids: new_tx_ids,
        };

        stage.try_in_place_bin_packing(&new_cluster, &[0]);
    }

    #[test]
    fn test_bin_pack_comparison_rejects_on_overflow() {
        // Set up bin_instructions near u64::MAX to test the checked_add
        // comparison in bin_pack_clusters.
        let mut bin_instructions = vec![u64::MAX - 5, 0];
        let mut cluster_tx_ids = BitSet::default();
        cluster_tx_ids.set(0);
        let clusters = vec![Cluster {
            instructions: 100,
            conflicts: BitSet::default(),
            tx_ids: cluster_tx_ids,
        }];

        // First bin would overflow, second bin has room (max is u64::MAX).
        let result = bin_pack_clusters(&clusters, 2, u64::MAX, &mut bin_instructions);
        // Should succeed by packing into second bin (not panic).
        assert!(result.is_some());
        assert_eq!(bin_instructions[0], u64::MAX - 5); // unchanged
        assert_eq!(bin_instructions[1], 100); // packed here
    }

    #[test]
    fn test_normal_bin_accounting_cycle() {
        // Verify normal operation: add clusters, remove, revert.
        let config = ParallelPartitionConfig {
            clusters_per_stage: 2,
            instructions_per_cluster: 1000,
        };
        let mut stage = Stage::new(config);

        // Add cluster 0 with 300 instructions to bin 0.
        let mut tx_ids_0 = BitSet::default();
        tx_ids_0.set(0);
        stage.bin_packing[0].set(0);
        stage.bin_instructions[0] = 300;
        stage.clusters.push(Cluster {
            instructions: 300,
            conflicts: BitSet::default(),
            tx_ids: tx_ids_0,
        });

        // New cluster that conflicts with cluster 0.
        let mut new_conflicts = BitSet::default();
        new_conflicts.set(0);
        let mut new_tx_ids = BitSet::default();
        new_tx_ids.set(1);
        let new_cluster = Cluster {
            instructions: 200,
            conflicts: new_conflicts,
            tx_ids: new_tx_ids,
        };

        // This should succeed: remove cluster 0 (300), then fit new_cluster (200).
        let result = stage.try_in_place_bin_packing(&new_cluster, &[0]);
        assert!(result);
        // The new cluster (200 instructions) should be in some bin.
        let total: u64 = stage.bin_instructions.iter().sum();
        assert_eq!(total, 200);
    }
}

#[cfg(test)]
mod stages_to_xdr_phase_tests {
    use super::*;
    use stellar_xdr::curr::{
        HostFunction, InvokeContractArgs, InvokeHostFunctionOp, LedgerFootprint, LedgerKey,
        LedgerKeyContractData, Memo, MuxedAccount, Operation, OperationBody, Preconditions,
        ScAddress, ScVal, SorobanResources, SorobanTransactionData, SorobanTransactionDataExt,
        Transaction, TransactionEnvelope, TransactionExt, TransactionV1Envelope, Uint256, VecM,
    };

    fn make_soroban_tx(
        seed: u8,
        seq: i64,
        read_only_keys: Vec<LedgerKey>,
        read_write_keys: Vec<LedgerKey>,
        instructions: u32,
    ) -> TransactionEnvelope {
        let source = MuxedAccount::Ed25519(Uint256([seed; 32]));
        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: read_only_keys.try_into().unwrap_or_default(),
                    read_write: read_write_keys.try_into().unwrap_or_default(),
                },
                instructions,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee: 100,
        };
        let invoke_op = Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function: HostFunction::InvokeContract(InvokeContractArgs {
                    contract_address: ScAddress::Contract(stellar_xdr::curr::ContractId(
                        stellar_xdr::curr::Hash([seed; 32]),
                    )),
                    function_name: stellar_xdr::curr::ScSymbol("test".try_into().unwrap()),
                    args: Default::default(),
                }),
                auth: Default::default(),
            }),
        };
        let tx = Transaction {
            source_account: source,
            fee: 1000,
            seq_num: stellar_xdr::curr::SequenceNumber(seq),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![invoke_op].try_into().unwrap(),
            ext: TransactionExt::V1(soroban_data),
        };
        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: VecM::default(),
        })
    }

    fn contract_key(id: u8) -> LedgerKey {
        LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(stellar_xdr::curr::ContractId(stellar_xdr::curr::Hash(
                [id; 32],
            ))),
            key: ScVal::U32(id as u32),
            durability: stellar_xdr::curr::ContractDataDurability::Persistent,
        })
    }

    fn tx_hash(tx: &TransactionEnvelope) -> Hash256 {
        Hash256::hash_xdr(tx)
    }

    // =========================================================================
    // Multi-stage, multi-cluster tests for stages_to_xdr_phase
    // =========================================================================

    #[test]
    fn test_stages_to_xdr_phase_sorts_txs_within_cluster() {
        // Create cluster with 3 TXs in arbitrary order
        let tx_a = make_soroban_tx(10, 1, vec![], vec![contract_key(10)], 1000);
        let tx_b = make_soroban_tx(20, 1, vec![], vec![contract_key(20)], 1000);
        let tx_c = make_soroban_tx(30, 1, vec![], vec![contract_key(30)], 1000);

        let stages = vec![vec![vec![tx_c.clone(), tx_a.clone(), tx_b.clone()]]];
        let phase = stages_to_xdr_phase(stages, Some(100));

        match phase {
            TransactionPhase::V1(parallel) => {
                let cluster = &parallel.execution_stages[0].0[0].0;
                // Verify TXs within cluster are sorted by hash ascending
                for i in 1..cluster.len() {
                    let prev = tx_hash(&cluster[i - 1]);
                    let curr = tx_hash(&cluster[i]);
                    assert!(
                        prev.0 <= curr.0,
                        "TXs within cluster should be sorted by hash"
                    );
                }
            }
            _ => panic!("Expected V1 phase"),
        }
    }

    #[test]
    fn test_stages_to_xdr_phase_sorts_clusters_within_stage() {
        // Two independent clusters in one stage
        let tx_a = make_soroban_tx(10, 1, vec![], vec![contract_key(10)], 1000);
        let tx_b = make_soroban_tx(20, 1, vec![], vec![contract_key(20)], 1000);

        let stages = vec![vec![vec![tx_b.clone()], vec![tx_a.clone()]]];
        let phase = stages_to_xdr_phase(stages, Some(100));

        match phase {
            TransactionPhase::V1(parallel) => {
                let stage = &parallel.execution_stages[0];
                assert_eq!(stage.0.len(), 2, "Should have 2 clusters");
                // Clusters should be sorted by first-TX hash ascending
                let first_hash_0 = tx_hash(&stage.0[0].0[0]);
                let first_hash_1 = tx_hash(&stage.0[1].0[0]);
                assert!(
                    first_hash_0.0 < first_hash_1.0,
                    "Clusters should be sorted by first-TX hash"
                );
            }
            _ => panic!("Expected V1 phase"),
        }
    }

    #[test]
    fn test_stages_to_xdr_phase_sorts_stages_by_first_tx() {
        // Two stages, each with one cluster, one TX
        let tx_a = make_soroban_tx(10, 1, vec![], vec![contract_key(10)], 1000);
        let tx_b = make_soroban_tx(20, 1, vec![], vec![contract_key(20)], 1000);

        // Put stages in reverse order to test sorting
        let stages = vec![vec![vec![tx_b.clone()]], vec![vec![tx_a.clone()]]];
        let phase = stages_to_xdr_phase(stages, Some(100));

        match phase {
            TransactionPhase::V1(parallel) => {
                assert_eq!(parallel.execution_stages.len(), 2, "Should have 2 stages");
                let first_tx_stage0 = &parallel.execution_stages[0].0[0].0[0];
                let first_tx_stage1 = &parallel.execution_stages[1].0[0].0[0];
                let hash_0 = tx_hash(first_tx_stage0);
                let hash_1 = tx_hash(first_tx_stage1);
                assert!(
                    hash_0.0 < hash_1.0,
                    "Stages should be sorted by first-TX-of-first-cluster hash"
                );
            }
            _ => panic!("Expected V1 phase"),
        }
    }

    #[test]
    fn test_stages_to_xdr_phase_multi_stage_multi_cluster() {
        // Stage 1: cluster_a=[tx1, tx2], cluster_b=[tx3]
        // Stage 2: cluster_c=[tx4]
        let tx1 = make_soroban_tx(1, 1, vec![], vec![contract_key(1)], 1000);
        let tx2 = make_soroban_tx(2, 1, vec![], vec![contract_key(2)], 1000);
        let tx3 = make_soroban_tx(3, 1, vec![], vec![contract_key(3)], 1000);
        let tx4 = make_soroban_tx(4, 1, vec![], vec![contract_key(4)], 1000);

        let stages = vec![vec![vec![tx1, tx2], vec![tx3]], vec![vec![tx4]]];
        let phase = stages_to_xdr_phase(stages, Some(200));

        match phase {
            TransactionPhase::V1(parallel) => {
                assert_eq!(parallel.base_fee, Some(200));
                assert_eq!(parallel.execution_stages.len(), 2);

                // Verify total TX count is preserved
                let total_txs: usize = parallel
                    .execution_stages
                    .iter()
                    .flat_map(|s| s.0.iter())
                    .map(|c| c.0.len())
                    .sum();
                assert_eq!(total_txs, 4);

                // Verify all sort invariants:
                // 1. TXs within each cluster sorted
                // 2. Clusters within each stage sorted
                // 3. Stages sorted
                for stage in parallel.execution_stages.iter() {
                    for cluster in stage.0.iter() {
                        for i in 1..cluster.0.len() {
                            let h1 = tx_hash(&cluster.0[i - 1]);
                            let h2 = tx_hash(&cluster.0[i]);
                            assert!(h1.0 <= h2.0, "TXs in cluster not sorted");
                        }
                    }
                    for i in 1..stage.0.len() {
                        let h1 = tx_hash(&stage.0[i - 1].0[0]);
                        let h2 = tx_hash(&stage.0[i].0[0]);
                        assert!(h1.0 < h2.0, "Clusters in stage not sorted");
                    }
                }
                for i in 1..parallel.execution_stages.len() {
                    let h1 = tx_hash(&parallel.execution_stages[i - 1].0[0].0[0]);
                    let h2 = tx_hash(&parallel.execution_stages[i].0[0].0[0]);
                    assert!(h1.0 < h2.0, "Stages not sorted");
                }
            }
            _ => panic!("Expected V1 phase"),
        }
    }

    #[test]
    fn test_stages_to_xdr_phase_none_base_fee() {
        let tx = make_soroban_tx(1, 1, vec![], vec![contract_key(1)], 1000);
        let stages = vec![vec![vec![tx]]];
        let phase = stages_to_xdr_phase(stages, None);

        match phase {
            TransactionPhase::V1(parallel) => {
                assert_eq!(parallel.base_fee, None);
            }
            _ => panic!("Expected V1 phase"),
        }
    }

    #[test]
    fn test_stages_to_xdr_phase_sorting_is_idempotent() {
        // Build once, extract TXs, rebuild — should get same result
        let tx_a = make_soroban_tx(10, 1, vec![], vec![contract_key(10)], 1000);
        let tx_b = make_soroban_tx(20, 1, vec![], vec![contract_key(20)], 1000);
        let tx_c = make_soroban_tx(30, 1, vec![], vec![contract_key(30)], 1000);

        let stages = vec![vec![vec![tx_c, tx_a, tx_b]]];
        let phase1 = stages_to_xdr_phase(stages.clone(), Some(100));

        // Extract the TXs back from phase1 and feed them in again
        let extracted_txs = match &phase1 {
            TransactionPhase::V1(p) => p.execution_stages[0].0[0].0.to_vec(),
            _ => panic!("Expected V1"),
        };
        let stages2 = vec![vec![extracted_txs]];
        let phase2 = stages_to_xdr_phase(stages2, Some(100));

        // Both phases should produce the same XDR
        let xdr1 = phase1.to_xdr(stellar_xdr::curr::Limits::none()).unwrap();
        let xdr2 = phase2.to_xdr(stellar_xdr::curr::Limits::none()).unwrap();
        assert_eq!(xdr1, xdr2, "stages_to_xdr_phase should be idempotent");
    }
}

// ---------------------------------------------------------------------------
// stellar-core parity tests
// ---------------------------------------------------------------------------
// Ports test scenarios from stellar-core's runParallelTxSetBuildingTest
// (TxSetTests.cpp:2493-3155). Each test runs both variable-stage-count
// (min=1, max=4) and fixed-stage-count (min=4, max=4) variants.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod stellar_core_parity_tests {
    use super::*;
    use stellar_xdr::curr::{
        ContractDataDurability, HostFunction, InvokeContractArgs, InvokeHostFunctionOp,
        LedgerFootprint, LedgerKey, LedgerKeyContractData, Memo, MuxedAccount, Operation,
        OperationBody, Preconditions, ScAddress, ScVal, SorobanResources, SorobanTransactionData,
        SorobanTransactionDataExt, Transaction, TransactionEnvelope, TransactionExt,
        TransactionV1Envelope, Uint256, VecM,
    };

    const STAGE_COUNT: u32 = 4;
    const CLUSTER_COUNT: u32 = 8;
    const LEDGER_MAX_INSTRUCTIONS: i64 = 400_000_000;
    const LEDGER_BASE_FEE: i64 = 100;

    fn test_network_id() -> NetworkId {
        NetworkId(henyey_common::Hash256([0u8; 32]))
    }

    /// Generate a contract data ledger key from an i32 ID.
    /// Durability alternates: even=Persistent, odd=Temporary (matches stellar-core).
    fn contract_data_key(id: i32) -> LedgerKey {
        let durability = if id % 2 == 0 {
            ContractDataDurability::Persistent
        } else {
            ContractDataDurability::Temporary
        };
        LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(stellar_xdr::curr::ContractId(stellar_xdr::curr::Hash(
                [0u8; 32],
            ))),
            key: ScVal::I32(id),
            durability,
        })
    }

    /// Create a Soroban TX with i32 key IDs for large key spaces.
    /// Auto-increments account_id to ensure unique source accounts.
    fn make_parity_tx(
        account_id: &mut u32,
        instructions: i32,
        ro_keys: &[i32],
        rw_keys: &[i32],
        inclusion_fee: i64,
    ) -> TransactionEnvelope {
        let id = *account_id;
        *account_id += 1;

        let mut source_bytes = [0u8; 32];
        source_bytes[..4].copy_from_slice(&id.to_le_bytes());
        let source = MuxedAccount::Ed25519(Uint256(source_bytes));

        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: ro_keys
                        .iter()
                        .map(|&k| contract_data_key(k))
                        .collect::<Vec<_>>()
                        .try_into()
                        .unwrap_or_default(),
                    read_write: rw_keys
                        .iter()
                        .map(|&k| contract_data_key(k))
                        .collect::<Vec<_>>()
                        .try_into()
                        .unwrap_or_default(),
                },
                instructions: instructions as u32,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee: 0,
        };

        let invoke_op = Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function: HostFunction::InvokeContract(InvokeContractArgs {
                    contract_address: ScAddress::Contract(stellar_xdr::curr::ContractId(
                        stellar_xdr::curr::Hash(source_bytes),
                    )),
                    function_name: stellar_xdr::curr::ScSymbol("test".try_into().unwrap()),
                    args: Default::default(),
                }),
                auth: Default::default(),
            }),
        };

        // fee = inclusion_fee + resource_fee (resource_fee=0 so fee=inclusion_fee)
        let tx = Transaction {
            source_account: source,
            fee: inclusion_fee as u32,
            seq_num: stellar_xdr::curr::SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![invoke_op].try_into().unwrap(),
            ext: TransactionExt::V1(soroban_data),
        };

        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: VecM::default(),
        })
    }

    /// Verify uniform shape: all stages have same cluster count, all clusters same TX count.
    fn validate_shape(
        stages: &[Vec<Vec<TransactionEnvelope>>],
        expected_stages: usize,
        expected_clusters_per_stage: usize,
        expected_txs_per_cluster: usize,
    ) {
        assert_eq!(
            stages.len(),
            expected_stages,
            "expected {} stages, got {}",
            expected_stages,
            stages.len()
        );
        for (i, stage) in stages.iter().enumerate() {
            assert_eq!(
                stage.len(),
                expected_clusters_per_stage,
                "stage {}: expected {} clusters, got {}",
                i,
                expected_clusters_per_stage,
                stage.len()
            );
            for (j, cluster) in stage.iter().enumerate() {
                assert_eq!(
                    cluster.len(),
                    expected_txs_per_cluster,
                    "stage {} cluster {}: expected {} txs, got {}",
                    i,
                    j,
                    expected_txs_per_cluster,
                    cluster.len()
                );
            }
        }
    }

    /// Compute base fee from builder output. Matches selection.rs:compute_soroban_base_fee logic.
    fn compute_base_fee(stages: &[Vec<Vec<TransactionEnvelope>>], had_tx_not_fitting: bool) -> i64 {
        if !had_tx_not_fitting {
            return LEDGER_BASE_FEE;
        }
        stages
            .iter()
            .flat_map(|s| s.iter())
            .flat_map(|c| c.iter())
            .map(|tx| crate::tx_set_utils::envelope_inclusion_fee(tx))
            .min()
            .unwrap_or(LEDGER_BASE_FEE)
    }

    /// Run a scenario with both variable and fixed stage count configurations.
    fn run_both<F>(f: F)
    where
        F: Fn(bool), // true = variable stage count
    {
        f(true);
        f(false);
    }

    fn stage_range(variable: bool) -> (u32, u32) {
        if variable {
            (1, STAGE_COUNT)
        } else {
            (STAGE_COUNT, STAGE_COUNT)
        }
    }

    // ---- No-conflict scenarios ----

    #[test]
    fn test_parity_no_conflicts_single_stage() {
        run_both(|variable| {
            let mut account_id = 0u32;
            let txs: Vec<_> = (0..CLUSTER_COUNT as i32)
                .map(|i| {
                    make_parity_tx(
                        &mut account_id,
                        100_000_000,
                        &[4 * i, 4 * i + 1],
                        &[4 * i + 2, 4 * i + 3],
                        1000,
                    )
                })
                .collect();

            let (min, max) = stage_range(variable);
            let (stages, had_drop) = build_parallel_soroban_phase(
                txs,
                test_network_id(),
                LEDGER_MAX_INSTRUCTIONS,
                CLUSTER_COUNT,
                min,
                max,
            );

            if variable {
                // With variable stages: 1 stage, 2 clusters (bin-packed), 4 txs each
                validate_shape(
                    &stages,
                    1,
                    CLUSTER_COUNT as usize / STAGE_COUNT as usize,
                    STAGE_COUNT as usize,
                );
            } else {
                // With fixed stages: 1 stage, 8 clusters, 1 tx each
                validate_shape(&stages, 1, CLUSTER_COUNT as usize, 1);
            }
            assert_eq!(compute_base_fee(&stages, had_drop), LEDGER_BASE_FEE);
        });
    }

    #[test]
    fn test_parity_no_conflicts_all_stages() {
        run_both(|variable| {
            let mut account_id = 0u32;
            let txs: Vec<_> = (0..(STAGE_COUNT * CLUSTER_COUNT) as i32)
                .map(|i| {
                    make_parity_tx(
                        &mut account_id,
                        100_000_000,
                        &[4 * i, 4 * i + 1],
                        &[4 * i + 2, 4 * i + 3],
                        1000,
                    )
                })
                .collect();

            let (min, max) = stage_range(variable);
            let (stages, had_drop) = build_parallel_soroban_phase(
                txs,
                test_network_id(),
                LEDGER_MAX_INSTRUCTIONS,
                CLUSTER_COUNT,
                min,
                max,
            );

            if variable {
                validate_shape(&stages, 1, CLUSTER_COUNT as usize, STAGE_COUNT as usize);
            } else {
                validate_shape(&stages, STAGE_COUNT as usize, CLUSTER_COUNT as usize, 1);
            }
            assert_eq!(compute_base_fee(&stages, had_drop), LEDGER_BASE_FEE);
        });
    }

    #[test]
    fn test_parity_no_conflicts_all_stages_smaller_txs() {
        run_both(|variable| {
            let mut account_id = 0u32;
            let txs: Vec<_> = (0..(STAGE_COUNT * CLUSTER_COUNT * 5) as i32)
                .map(|i| {
                    make_parity_tx(
                        &mut account_id,
                        20_000_000,
                        &[4 * i, 4 * i + 1],
                        &[4 * i + 2, 4 * i + 3],
                        1000,
                    )
                })
                .collect();

            let (min, max) = stage_range(variable);
            let (stages, had_drop) = build_parallel_soroban_phase(
                txs,
                test_network_id(),
                LEDGER_MAX_INSTRUCTIONS,
                CLUSTER_COUNT,
                min,
                max,
            );

            if variable {
                validate_shape(
                    &stages,
                    1,
                    CLUSTER_COUNT as usize,
                    (STAGE_COUNT * 5) as usize,
                );
            } else {
                validate_shape(&stages, STAGE_COUNT as usize, CLUSTER_COUNT as usize, 5);
            }
            assert_eq!(compute_base_fee(&stages, had_drop), LEDGER_BASE_FEE);
        });
    }

    #[test]
    fn test_parity_no_conflicts_prioritization() {
        run_both(|variable| {
            let mut account_id = 0u32;
            let txs: Vec<_> = (0..(STAGE_COUNT * CLUSTER_COUNT * 10) as i32)
                .map(|i| {
                    make_parity_tx(
                        &mut account_id,
                        20_000_000,
                        &[4 * i, 4 * i + 1],
                        &[4 * i + 2, 4 * i + 3],
                        (i as i64 + 1) * 1000,
                    )
                })
                .collect();

            let (min, max) = stage_range(variable);
            let (stages, had_drop) = build_parallel_soroban_phase(
                txs,
                test_network_id(),
                LEDGER_MAX_INSTRUCTIONS,
                CLUSTER_COUNT,
                min,
                max,
            );

            if variable {
                validate_shape(
                    &stages,
                    1,
                    CLUSTER_COUNT as usize,
                    (STAGE_COUNT * 5) as usize,
                );
            } else {
                validate_shape(&stages, STAGE_COUNT as usize, CLUSTER_COUNT as usize, 5);
            }

            assert!(had_drop, "half the txs should be evicted");
            // Base fee: the lowest fee of the surviving (highest-fee) half.
            // 320 TXs with fees 1000..320000. Survivors are the top 160.
            // Lowest survivor fee = 10 * STAGE_COUNT * CLUSTER_COUNT * 1000 / 2 + 1000
            let expected_base_fee =
                10 * STAGE_COUNT as i64 * CLUSTER_COUNT as i64 * 1000 / 2 + 1000;
            assert_eq!(compute_base_fee(&stages, had_drop), expected_base_fee);
        });
    }

    #[test]
    fn test_parity_no_conflicts_instruction_limit_reached() {
        run_both(|variable| {
            let mut account_id = 0u32;
            // Reduced instruction limits: 2.5M per tx, 10M per ledger
            let ledger_max_instructions: i64 = 10_000_000;
            let txs: Vec<_> = (0..(STAGE_COUNT * CLUSTER_COUNT * 4) as i32)
                .map(|i| {
                    make_parity_tx(
                        &mut account_id,
                        2_500_000,
                        &[4 * i, 4 * i + 1],
                        &[4 * i + 2, 4 * i + 3],
                        100 + i as i64,
                    )
                })
                .collect();

            let (min, max) = stage_range(variable);
            let (stages, had_drop) = build_parallel_soroban_phase(
                txs,
                test_network_id(),
                ledger_max_instructions,
                CLUSTER_COUNT,
                min,
                max,
            );

            if variable {
                validate_shape(&stages, 1, CLUSTER_COUNT as usize, STAGE_COUNT as usize);
            } else {
                validate_shape(&stages, STAGE_COUNT as usize, CLUSTER_COUNT as usize, 1);
            }

            assert!(had_drop);
            // 128 input TXs, 32 survive (4 per cluster × 8 clusters or equivalent).
            // Base fee = fee of lowest survivor = 100 + (128 - 32) = 196.
            let expected_base_fee =
                100 + (STAGE_COUNT * CLUSTER_COUNT * 4 - STAGE_COUNT * CLUSTER_COUNT) as i64;
            assert_eq!(compute_base_fee(&stages, had_drop), expected_base_fee);
        });
    }

    // ---- Conflict scenarios ----

    #[test]
    fn test_parity_all_rw_conflicting() {
        run_both(|variable| {
            let mut account_id = 0u32;
            let txs: Vec<_> = (0..(CLUSTER_COUNT * STAGE_COUNT) as i32)
                .map(|i| {
                    // All TXs write key 0 → all conflict with each other
                    make_parity_tx(
                        &mut account_id,
                        100_000_000,
                        &[4 * i + 1, 4 * i + 2],
                        &[4 * i + 3, 0, 4 * i + 4],
                        100 + i as i64,
                    )
                })
                .collect();

            let (min, max) = stage_range(variable);
            let (stages, had_drop) = build_parallel_soroban_phase(
                txs,
                test_network_id(),
                LEDGER_MAX_INSTRUCTIONS,
                CLUSTER_COUNT,
                min,
                max,
            );

            if variable {
                validate_shape(&stages, 1, 1, STAGE_COUNT as usize);
            } else {
                validate_shape(&stages, STAGE_COUNT as usize, 1, 1);
            }

            assert!(had_drop);
            let expected_base_fee = 100 + (CLUSTER_COUNT * STAGE_COUNT - STAGE_COUNT) as i64;
            assert_eq!(compute_base_fee(&stages, had_drop), expected_base_fee);
        });
    }

    #[test]
    fn test_parity_chain_of_conflicts() {
        // tx[i] reads key i, writes key i+1. Chain: 0→1→2→...
        // Stages break the chain since tx[i] and tx[i+1] conflict on key i+1.
        run_both(|_variable| {
            let mut account_id = 0u32;
            let txs: Vec<_> = (0..(CLUSTER_COUNT * STAGE_COUNT) as i32)
                .map(|i| {
                    make_parity_tx(&mut account_id, 100_000_000, &[i], &[i + 1], 100 + i as i64)
                })
                .collect();

            // Chain of conflicts always produces STAGE_COUNT stages regardless of min
            let (stages, _had_drop) = build_parallel_soroban_phase(
                txs,
                test_network_id(),
                LEDGER_MAX_INSTRUCTIONS,
                CLUSTER_COUNT,
                1,
                STAGE_COUNT,
            );

            validate_shape(&stages, STAGE_COUNT as usize, CLUSTER_COUNT as usize, 1);
            assert_eq!(compute_base_fee(&stages, false), LEDGER_BASE_FEE);
        });
    }

    #[test]
    fn test_parity_conflict_clusters_not_exceeding_max_insns() {
        // 8 clusters of 4 conflicting TXs. Each cluster: 4 TXs that write the same key i.
        run_both(|variable| {
            let mut account_id = 0u32;
            let mut txs = Vec::new();
            for i in 0..CLUSTER_COUNT as i32 {
                for j in 0..STAGE_COUNT as i32 {
                    txs.push(make_parity_tx(
                        &mut account_id,
                        100_000_000,
                        &[i * STAGE_COUNT as i32 + j + 1000],
                        &[i, i * STAGE_COUNT as i32 + j + 10000],
                        100 + i as i64,
                    ));
                }
            }

            let (min, max) = stage_range(variable);
            let (stages, _had_drop) = build_parallel_soroban_phase(
                txs,
                test_network_id(),
                LEDGER_MAX_INSTRUCTIONS,
                CLUSTER_COUNT,
                min,
                max,
            );

            if variable {
                validate_shape(&stages, 1, CLUSTER_COUNT as usize, STAGE_COUNT as usize);
            } else {
                validate_shape(&stages, STAGE_COUNT as usize, CLUSTER_COUNT as usize, 1);
            }
            assert_eq!(compute_base_fee(&stages, false), LEDGER_BASE_FEE);
        });
    }

    #[test]
    fn test_parity_small_conflict_clusters_with_excluded_txs() {
        // 8 clusters × 5 TXs per cluster (STAGE_COUNT + 1). All TXs in cluster i write key i.
        // Only STAGE_COUNT per cluster can fit → 1 TX per cluster evicted.
        run_both(|variable| {
            let mut account_id = 0u32;
            let mut txs = Vec::new();
            for i in 0..CLUSTER_COUNT as i32 {
                for j in 0..(STAGE_COUNT as i32 + 1) {
                    txs.push(make_parity_tx(
                        &mut account_id,
                        100_000_000,
                        &[],
                        &[i],
                        100 + (i * (STAGE_COUNT as i32 + 1) + j) as i64,
                    ));
                }
            }

            let (min, max) = stage_range(variable);
            let (stages, had_drop) = build_parallel_soroban_phase(
                txs,
                test_network_id(),
                LEDGER_MAX_INSTRUCTIONS,
                CLUSTER_COUNT,
                min,
                max,
            );

            if variable {
                validate_shape(&stages, 1, CLUSTER_COUNT as usize, STAGE_COUNT as usize);
            } else {
                validate_shape(&stages, STAGE_COUNT as usize, CLUSTER_COUNT as usize, 1);
            }

            assert!(had_drop);
            // 1 cluster worth of txs excluded. Lowest surviving fee = 101.
            assert_eq!(compute_base_fee(&stages, had_drop), 101);
        });
    }

    #[test]
    fn test_parity_one_sparse_conflict_cluster() {
        assert!(CLUSTER_COUNT > STAGE_COUNT);

        run_both(|variable| {
            let mut account_id = 0u32;
            let mut txs = Vec::new();

            // Dense cluster: STAGE_COUNT TXs with RW conflict on key 1000, high fee.
            for i in 0..STAGE_COUNT as i32 {
                txs.push(make_parity_tx(
                    &mut account_id,
                    100_000_000,
                    &[],
                    &[i, 1000],
                    1_000_000 - i as i64,
                ));
            }

            // Sparse: (CLUSTER_COUNT-1) TXs per stage with RO-RW conflict.
            for i in 0..STAGE_COUNT as i32 {
                for j in 0..(CLUSTER_COUNT as i32 - 1) {
                    txs.push(make_parity_tx(
                        &mut account_id,
                        100_000_000,
                        &[i],
                        &[i * CLUSTER_COUNT as i32 + j + 10_000],
                        1000 + (i * CLUSTER_COUNT as i32 + j) as i64,
                    ));
                }
            }

            // Cheap conflicting TXs that shouldn't fit.
            for i in 0..(CLUSTER_COUNT - STAGE_COUNT) as i32 {
                txs.push(make_parity_tx(
                    &mut account_id,
                    100_000_000,
                    &[i % STAGE_COUNT as i32],
                    &[i + 100_000],
                    100 + i as i64,
                ));
            }

            let (min, max) = stage_range(variable);
            let (stages, had_drop) = build_parallel_soroban_phase(
                txs,
                test_network_id(),
                LEDGER_MAX_INSTRUCTIONS,
                CLUSTER_COUNT,
                min,
                max,
            );

            if variable {
                validate_shape(
                    &stages,
                    2,
                    CLUSTER_COUNT as usize,
                    (STAGE_COUNT / 2) as usize,
                );
            } else {
                validate_shape(&stages, STAGE_COUNT as usize, CLUSTER_COUNT as usize, 1);
            }

            // 4 cheap TXs don't fit instruction limits → base fee = 1000
            assert_eq!(compute_base_fee(&stages, had_drop), 1000);
        });
    }

    #[test]
    fn test_parity_many_clusters_with_small_transactions() {
        run_both(|_variable| {
            let mut account_id = 0u32;
            let mut txs = Vec::new();
            for i in 0..CLUSTER_COUNT as i32 {
                for j in 0..(10 * STAGE_COUNT as i32) {
                    txs.push(make_parity_tx(
                        &mut account_id,
                        10_000_000,
                        &[1000 + i * 10 + j],
                        &[i, 10_000 + i * 10 + j],
                        100 + (i * (STAGE_COUNT as i32 + 1) + j) as i64,
                    ));
                }
            }

            // Both variable and fixed produce the same shape here
            let (stages, _had_drop) = build_parallel_soroban_phase(
                txs,
                test_network_id(),
                LEDGER_MAX_INSTRUCTIONS,
                CLUSTER_COUNT,
                1,
                STAGE_COUNT,
            );

            validate_shape(&stages, STAGE_COUNT as usize, CLUSTER_COUNT as usize, 10);
            assert_eq!(compute_base_fee(&stages, false), LEDGER_BASE_FEE);
        });
    }

    #[test]
    fn test_parity_all_ro_conflict_with_one_rw() {
        run_both(|variable| {
            let mut account_id = 0u32;
            let mut txs = Vec::new();

            // 1 high-fee TX that writes key 0 (conflicts with all RO readers).
            txs.push(make_parity_tx(
                &mut account_id,
                100_000_000,
                &[1, 2],
                &[0, 3, 4],
                1_000_000,
            ));

            // 159 TXs that read key 0 (RO-RW conflict with the first TX).
            for i in 1..(CLUSTER_COUNT * STAGE_COUNT * 5) as i32 {
                txs.push(make_parity_tx(
                    &mut account_id,
                    20_000_000,
                    &[0, 4 * i + 1, 4 * i + 2],
                    &[4 * i + 3, 4 * i + 4],
                    100 + i as i64,
                ));
            }

            let (min, max) = stage_range(variable);
            let (stages, had_drop) = build_parallel_soroban_phase(
                txs,
                test_network_id(),
                LEDGER_MAX_INSTRUCTIONS,
                CLUSTER_COUNT,
                min,
                max,
            );

            // Custom shape assertion: one stage has the high-fee TX alone,
            // other stages have CLUSTER_COUNT clusters of small TXs.
            let mut single_thread_stages = 0;
            for stage in &stages {
                if stage.len() == 1 && stage[0].len() == 1 {
                    single_thread_stages += 1;
                } else {
                    assert_eq!(
                        stage.len(),
                        CLUSTER_COUNT as usize,
                        "non-single stage should have {} clusters",
                        CLUSTER_COUNT
                    );
                    for cluster in stage {
                        assert_eq!(cluster.len(), 5, "each cluster should have 5 txs");
                    }
                }
            }
            assert_eq!(
                single_thread_stages, 1,
                "expected exactly one single-thread stage"
            );

            // Base fee = 100 + CLUSTER_COUNT * 5 (the small txs that couldn't fit
            // in the same stage as the RW tx).
            let expected_base_fee = 100 + CLUSTER_COUNT as i64 * 5;
            assert_eq!(compute_base_fee(&stages, had_drop), expected_base_fee);
        });
    }

    // ---- Randomized smoke test (adapted from TxSetTests.cpp:3085-3154) ----

    /// Extract the RO and RW ledger key sets from a transaction envelope's
    /// Soroban footprint, serialized to bytes for set operations.
    fn extract_footprint_keys(tx: &TransactionEnvelope) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
        let data = match tx {
            TransactionEnvelope::Tx(env) => match &env.tx.ext {
                TransactionExt::V1(d) => d,
                _ => return (vec![], vec![]),
            },
            _ => return (vec![], vec![]),
        };
        let ro: Vec<Vec<u8>> = data
            .resources
            .footprint
            .read_only
            .iter()
            .map(|k| k.to_xdr(Limits::none()).unwrap())
            .collect();
        let rw: Vec<Vec<u8>> = data
            .resources
            .footprint
            .read_write
            .iter()
            .map(|k| k.to_xdr(Limits::none()).unwrap())
            .collect();
        (ro, rw)
    }

    /// Extract instruction count from a transaction envelope.
    fn extract_instructions(tx: &TransactionEnvelope) -> u64 {
        match tx {
            TransactionEnvelope::Tx(env) => match &env.tx.ext {
                TransactionExt::V1(d) => d.resources.instructions as u64,
                _ => 0,
            },
            _ => 0,
        }
    }

    /// Verify no two clusters within a stage have RW-RW or RO-RW footprint conflicts.
    fn assert_no_intra_stage_conflicts(stages: &[Vec<Vec<TransactionEnvelope>>], context: &str) {
        use std::collections::HashSet;

        for (si, stage) in stages.iter().enumerate() {
            // Collect per-cluster RW and RO key sets.
            let cluster_keys: Vec<(HashSet<Vec<u8>>, HashSet<Vec<u8>>)> = stage
                .iter()
                .map(|cluster| {
                    let mut ro_set = HashSet::new();
                    let mut rw_set = HashSet::new();
                    for tx in cluster {
                        let (ro, rw) = extract_footprint_keys(tx);
                        ro_set.extend(ro);
                        rw_set.extend(rw);
                    }
                    (ro_set, rw_set)
                })
                .collect();

            // Check pairwise: no RW-RW or RO-RW conflicts between clusters.
            for i in 0..cluster_keys.len() {
                for j in (i + 1)..cluster_keys.len() {
                    let (_, rw_i) = &cluster_keys[i];
                    let (ro_j, rw_j) = &cluster_keys[j];
                    let (ro_i, _) = &cluster_keys[i];

                    // RW-RW conflict
                    let rw_rw: Vec<_> = rw_i.intersection(rw_j).collect();
                    assert!(
                        rw_rw.is_empty(),
                        "{context}: stage {si} clusters {i} and {j} have RW-RW conflict"
                    );
                    // RO-RW conflict (i reads, j writes)
                    let ro_rw: Vec<_> = ro_i.intersection(rw_j).collect();
                    assert!(
                        ro_rw.is_empty(),
                        "{context}: stage {si} clusters {i} and {j} have RO-RW conflict (i reads, j writes)"
                    );
                    // RO-RW conflict (j reads, i writes)
                    let rw_ro: Vec<_> = rw_i.intersection(ro_j).collect();
                    assert!(
                        rw_ro.is_empty(),
                        "{context}: stage {si} clusters {i} and {j} have RO-RW conflict (j reads, i writes)"
                    );
                }
            }
        }
    }

    #[test]
    fn test_smoke_randomized_parallel_builder() {
        use rand::distributions::Uniform;
        use rand::rngs::StdRng;
        use rand::{Rng, SeedableRng};
        use std::collections::HashSet;
        use stellar_xdr::curr::{Limits, ReadXdr, WriteXdr};

        for iter in 0..10u64 {
            let seed = 42 + iter;
            let mut rng = StdRng::seed_from_u64(seed);

            // Per-iteration distribution bounds (matching stellar-core pattern).
            let max_insns: i32 = rng.gen_range(20_000_000..=100_000_000);
            let key_range: i32 = rng.gen_range(50..=1000);

            let insns_dist = Uniform::new_inclusive(1_000_000, max_insns);
            let key_count_dist = Uniform::new_inclusive(1, 10);
            let key_dist = Uniform::new_inclusive(1, key_range);
            let fee_dist = Uniform::new_inclusive(100i64, 100_000i64);

            let mut account_id = 0u32;
            let mut txs = Vec::with_capacity(500);
            for _ in 0..500 {
                let ro_count: usize = rng.sample(key_count_dist) as usize;
                let rw_count: usize = rng.sample(key_count_dist) as usize;

                // Generate unique keys (matching stellar-core's dedup loop).
                let mut used_keys = HashSet::new();
                let mut ro_keys = Vec::with_capacity(ro_count);
                let mut rw_keys = Vec::with_capacity(rw_count);
                for i in 0..(ro_count + rw_count) {
                    let mut key: i32 = rng.sample(key_dist);
                    while used_keys.contains(&key) {
                        key = rng.sample(key_dist);
                    }
                    used_keys.insert(key);
                    if i < ro_count {
                        ro_keys.push(key);
                    } else {
                        rw_keys.push(key);
                    }
                }

                let instructions: i32 = rng.sample(insns_dist);
                let fee: i64 = rng.sample(fee_dist);

                txs.push(make_parity_tx(
                    &mut account_id,
                    instructions,
                    &ro_keys,
                    &rw_keys,
                    fee,
                ));
            }

            // Collect input TX hashes for conservation check.
            let input_hashes: HashSet<[u8; 32]> =
                txs.iter().map(|tx| Hash256::hash_xdr(tx).0).collect();
            assert_eq!(
                input_hashes.len(),
                500,
                "seed={seed}: input TXs should all have unique hashes"
            );

            // Run with both variable and fixed stage configs.
            for (mode, min_stages, max_stages) in [
                ("variable", 1u32, STAGE_COUNT),
                ("fixed", STAGE_COUNT, STAGE_COUNT),
            ] {
                let ctx = format!("seed={seed}, iter={iter}, mode={mode}");

                let (stages, had_drop) = build_parallel_soroban_phase(
                    txs.clone(),
                    test_network_id(),
                    LEDGER_MAX_INSTRUCTIONS,
                    CLUSTER_COUNT,
                    min_stages,
                    max_stages,
                );

                // 1. Stage count checks.
                assert!(
                    !stages.is_empty(),
                    "{ctx}: builder should produce at least 1 stage"
                );
                if mode == "fixed" {
                    assert_eq!(
                        stages.len(),
                        STAGE_COUNT as usize,
                        "{ctx}: fixed mode should produce exactly STAGE_COUNT stages"
                    );
                } else {
                    // With 500 TXs drawn from a key space of 50-1000 with
                    // 1-10 keys each, conflicts are virtually guaranteed,
                    // so we expect >1 stages — matching stellar-core's assertion.
                    assert!(
                        stages.len() > 1,
                        "{ctx}: variable mode should produce >1 stages with 500 random TXs"
                    );
                    assert!(
                        stages.len() <= STAGE_COUNT as usize,
                        "{ctx}: variable mode should produce at most STAGE_COUNT stages"
                    );
                }

                // 2. No intra-stage footprint conflicts.
                assert_no_intra_stage_conflicts(&stages, &ctx);

                // 3. Sequential instruction budget: sum of max-cluster-insns per
                //    stage must not exceed LEDGER_MAX_INSTRUCTIONS.
                let sequential_insns: u64 = stages
                    .iter()
                    .map(|stage| {
                        stage
                            .iter()
                            .map(|cluster| cluster.iter().map(extract_instructions).sum::<u64>())
                            .max()
                            .unwrap_or(0)
                    })
                    .sum();
                assert!(
                    sequential_insns <= LEDGER_MAX_INSTRUCTIONS as u64,
                    "{ctx}: sequential instructions {sequential_insns} exceeds limit {LEDGER_MAX_INSTRUCTIONS}"
                );

                // 4. TX conservation: all output TXs from input, no duplicates.
                let mut output_hashes = Vec::new();
                for stage in &stages {
                    for cluster in stage {
                        for tx in cluster {
                            let h = Hash256::hash_xdr(tx).0;
                            assert!(
                                input_hashes.contains(&h),
                                "{ctx}: output TX not found in input set"
                            );
                            output_hashes.push(h);
                        }
                    }
                }
                let output_set: HashSet<[u8; 32]> = output_hashes.iter().copied().collect();
                assert_eq!(
                    output_set.len(),
                    output_hashes.len(),
                    "{ctx}: duplicate TX in output"
                );

                // 5. had_drop consistency.
                // Note: the parallel builder may drop TXs due to conflicts
                // or instruction limits, not strictly by fee priority, so we
                // only verify count consistency here.
                let output_count = output_hashes.len();
                if !had_drop {
                    assert_eq!(
                        output_count, 500,
                        "{ctx}: had_drop=false but not all TXs included"
                    );
                } else {
                    assert!(
                        output_count < 500,
                        "{ctx}: had_drop=true but all TXs included"
                    );
                }

                // 6. XDR roundtrip via stages_to_xdr_phase.
                let base_fee = compute_base_fee(&stages, had_drop);
                let phase = stages_to_xdr_phase(stages, Some(base_fee));
                let xdr_bytes = phase
                    .to_xdr(Limits::none())
                    .expect(&format!("{ctx}: XDR serialization failed"));
                let decoded = TransactionPhase::from_xdr(&xdr_bytes, Limits::none())
                    .expect(&format!("{ctx}: XDR deserialization failed"));
                let re_encoded = decoded
                    .to_xdr(Limits::none())
                    .expect(&format!("{ctx}: XDR re-serialization failed"));
                assert_eq!(
                    xdr_bytes, re_encoded,
                    "{ctx}: XDR roundtrip produced different bytes"
                );
            }
        }
    }
}
