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
        for i in 0..len {
            if self.words[i] & other.words[i] != 0 {
                return true;
            }
        }
        false
    }

    fn union_with(&mut self, other: &BitSet) {
        if other.words.len() > self.words.len() {
            self.words.resize(other.words.len(), 0);
        }
        for (i, &w) in other.words.iter().enumerate() {
            self.words[i] |= w;
        }
    }

    fn difference_with(&mut self, other: &BitSet) {
        let len = self.words.len().min(other.words.len());
        for i in 0..len {
            self.words[i] &= !other.words[i];
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
        self.instructions += other.instructions;
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
        self.instructions_per_cluster * self.clusters_per_stage as u64
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
        if self.total_instructions + tx.instructions as u64 > self.config.instructions_per_stage() {
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
            self.total_instructions += tx.instructions as u64;
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
                self.total_instructions += tx.instructions as u64;
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
            // Find which bin this cluster is in.
            for (bin_id, bin) in self.bin_packing.iter().enumerate() {
                if bin.intersects(&cluster.tx_ids) {
                    removed.push((bin_id, cluster.instructions, cluster.tx_ids.clone()));
                    self.bin_instructions[bin_id] -= cluster.instructions;
                    self.bin_packing[bin_id].difference_with(&cluster.tx_ids);
                    break;
                }
            }
        }

        // Try to fit the new (merged) cluster into an existing bin.
        for bin_id in 0..self.config.clusters_per_stage as usize {
            if self.bin_instructions[bin_id] + new_cluster.instructions
                <= self.config.instructions_per_cluster
            {
                self.bin_instructions[bin_id] += new_cluster.instructions;
                self.bin_packing[bin_id].union_with(&new_cluster.tx_ids);
                return true;
            }
        }

        // Revert the removals.
        for (bin_id, insns, tx_ids) in removed {
            self.bin_instructions[bin_id] += insns;
            self.bin_packing[bin_id].union_with(&tx_ids);
        }
        false
    }

    /// Extract the transaction ID sets for each cluster.
    fn cluster_tx_ids(&self) -> Vec<&BitSet> {
        self.clusters.iter().map(|c| &c.tx_ids).collect()
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
            if bin_instructions[bin_id] + cluster.instructions <= max_instructions_per_bin {
                bin_instructions[bin_id] += cluster.instructions;
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

/// Detect footprint conflicts between Soroban transactions.
///
/// Two transactions conflict if:
/// - Both write the same key (RW-RW conflict)
/// - One reads and the other writes the same key (RO-RW conflict)
///
/// RO-RO does NOT create a conflict.
fn detect_conflicts(txs: &[TransactionEnvelope], network_id: NetworkId) -> Vec<BitSet> {
    let n = txs.len();
    let mut conflicts: Vec<BitSet> = (0..n).map(|_| BitSet::with_capacity(n)).collect();

    // Build key-to-txs maps: which transactions touch each key as RO or RW.
    let mut ro_key_txs: std::collections::HashMap<Vec<u8>, Vec<usize>> =
        std::collections::HashMap::new();
    let mut rw_key_txs: std::collections::HashMap<Vec<u8>, Vec<usize>> =
        std::collections::HashMap::new();

    for (tx_id, tx) in txs.iter().enumerate() {
        let frame = TransactionFrame::from_owned_with_network(tx.clone(), network_id);
        if let Some(data) = frame.soroban_data() {
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
) -> (Vec<Vec<Vec<TransactionEnvelope>>>, i64) {
    let conflicts = detect_conflicts(txs, network_id);
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
    // Highest-fee transactions get placed first.
    let mut sorted_ids: Vec<usize> = (0..n).collect();
    sorted_ids.sort_by(|&a, &b| tx_inclusion_fee(&txs[b]).cmp(&tx_inclusion_fee(&txs[a])));

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

    // Extract results: for each stage, for each cluster, collect the TX envelopes.
    let result_stages: Vec<Vec<Vec<TransactionEnvelope>>> = stages
        .iter()
        .map(|stage| {
            stage
                .cluster_tx_ids()
                .iter()
                .map(|tx_ids| {
                    tx_ids
                        .iter_ones()
                        .map(|id| txs[id].clone())
                        .collect::<Vec<_>>()
                })
                .filter(|cluster| !cluster.is_empty())
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
/// Stages of clusters of transaction envelopes: `stages[i][j]` = cluster j of stage i.
pub fn build_parallel_soroban_phase(
    soroban_txs: &[TransactionEnvelope],
    network_id: NetworkId,
    ledger_max_instructions: i64,
    ledger_max_dependent_tx_clusters: u32,
    min_stage_count: u32,
    max_stage_count: u32,
) -> Vec<Vec<Vec<TransactionEnvelope>>> {
    if soroban_txs.is_empty() {
        return Vec::new();
    }

    // If clusters_per_stage is 0, fall back to single cluster.
    if ledger_max_dependent_tx_clusters == 0 {
        return vec![vec![soroban_txs.to_vec()]];
    }

    // Try each stage count and collect (stages, fee) pairs.
    let mut results: Vec<(Vec<Vec<Vec<TransactionEnvelope>>>, i64)> = Vec::new();
    for sc in min_stage_count..=max_stage_count {
        let result = build_with_stage_count(
            soroban_txs,
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
    let mut best: Option<Vec<Vec<Vec<TransactionEnvelope>>>> = None;
    for (stages, fee) in results {
        if fee >= fee_threshold {
            let actual_stage_count = stages.len();
            if best.is_none() || actual_stage_count < best.as_ref().unwrap().len() {
                best = Some(stages);
            }
        }
    }

    best.unwrap_or_default()
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
                    cluster.sort_by_cached_key(|tx| Hash256::hash_xdr(tx).unwrap_or_default().0);
                    cluster
                })
                .collect();
            // 2. Sort clusters within each stage by first-TX hash
            sorted_clusters
                .sort_by_cached_key(|cluster| Hash256::hash_xdr(&cluster[0]).unwrap_or_default().0);
            sorted_clusters
        })
        .collect();

    // 3. Sort stages by first-TX-of-first-cluster hash
    sorted_stages.sort_by_cached_key(|stage| Hash256::hash_xdr(&stage[0][0]).unwrap_or_default().0);

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
        LedgerFootprint, LedgerKey, LedgerKeyContractData, Memo, MuxedAccount, Preconditions,
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
        let tx = Transaction {
            source_account: source,
            fee: 1000,
            seq_num: stellar_xdr::curr::SequenceNumber(seq),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: VecM::default(),
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
        let conflicts = detect_conflicts(&[tx_a, tx_b], test_network_id());
        assert!(!conflicts[0].get(1));
        assert!(!conflicts[1].get(0));
    }

    #[test]
    fn test_rw_rw_conflict() {
        let key = contract_key(1);
        let tx_a = make_soroban_tx(1, 1, vec![], vec![key.clone()], 1000);
        let tx_b = make_soroban_tx(2, 1, vec![], vec![key], 1000);
        let conflicts = detect_conflicts(&[tx_a, tx_b], test_network_id());
        assert!(conflicts[0].get(1));
        assert!(conflicts[1].get(0));
    }

    #[test]
    fn test_ro_rw_conflict() {
        let key = contract_key(1);
        let tx_a = make_soroban_tx(1, 1, vec![key.clone()], vec![], 1000);
        let tx_b = make_soroban_tx(2, 1, vec![], vec![key], 1000);
        let conflicts = detect_conflicts(&[tx_a, tx_b], test_network_id());
        assert!(conflicts[0].get(1));
        assert!(conflicts[1].get(0));
    }

    #[test]
    fn test_ro_ro_no_conflict() {
        let key = contract_key(1);
        let tx_a = make_soroban_tx(1, 1, vec![key.clone()], vec![], 1000);
        let tx_b = make_soroban_tx(2, 1, vec![key], vec![], 1000);
        let conflicts = detect_conflicts(&[tx_a, tx_b], test_network_id());
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
            &[tx_a, tx_b, tx_c],
            test_network_id(),
            100_000, // ledger max instructions
            8,       // clusters per stage
            1,
            1,
        );

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
            build_parallel_soroban_phase(&[tx_a, tx_b], test_network_id(), 100_000, 8, 1, 1);

        // No conflicts: should be 2 separate clusters in 1 stage
        assert_eq!(stages.len(), 1);
        assert_eq!(stages[0].len(), 2);
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
            &[tx_a.clone(), tx_b.clone()],
            test_network_id(),
            100_000,
            1, // only 1 cluster per stage
            1,
            1,
        );

        let total_txs: usize = stages.iter().flat_map(|s| s.iter()).map(|c| c.len()).sum();
        assert_eq!(total_txs, 1);

        // With 2 stages and 1 cluster each: instructions_per_cluster = 100k/2 = 50k
        // Each TX is 60k > 50k, so neither fits. Use higher ledger max instead.
        // ledger_max=120k, 2 stages, 1 cluster each: cluster limit = 60k, stage limit = 60k.
        let stages =
            build_parallel_soroban_phase(&[tx_a, tx_b], test_network_id(), 120_000, 1, 2, 2);

        let total_txs: usize = stages.iter().flat_map(|s| s.iter()).map(|c| c.len()).sum();
        assert_eq!(total_txs, 2);
    }

    #[test]
    fn test_audit_018_parallel_builder_uses_inclusion_fee_ordering() {
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
        assert_eq!(stages[0][0], vec![tx_high_inclusion]);
        assert_eq!(total_inclusion_fee, 800);
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
        let stages = build_parallel_soroban_phase(&[], test_network_id(), 100_000, 8, 1, 4);
        assert!(stages.is_empty());
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

        let stages = build_parallel_soroban_phase(&txs, test_network_id(), 1_000_000, 8, 1, 4);

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
}

#[cfg(test)]
mod stages_to_xdr_phase_tests {
    use super::*;
    use stellar_xdr::curr::{
        LedgerFootprint, LedgerKey, LedgerKeyContractData, Memo, MuxedAccount, Preconditions,
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
        let tx = Transaction {
            source_account: source,
            fee: 1000,
            seq_num: stellar_xdr::curr::SequenceNumber(seq),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: VecM::default(),
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
        Hash256::hash_xdr(tx).unwrap_or_default()
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
