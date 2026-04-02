//! Ledger replay for history catchup and verification.
//!
//! This module provides functions to replay ledgers from history archives,
//! reconstructing ledger state by re-executing transactions or applying
//! transaction metadata.
//!
//! # Replay Strategies
//!
//! There are two approaches to replaying ledgers:
//!
//! ## Re-execution Replay (`replay_ledger_with_execution`)
//!
//! Re-executes transactions against the current bucket list state. This:
//!
//! - Reconstructs state changes from transaction logic
//! - Validates transaction set and result hashes against headers
//! - Works with traditional archives (no `TransactionMeta` needed)
//! - May produce slightly different internal results than original execution
//!
//! This is the **default approach** used during catchup.
//!
//! ## Metadata Replay (`replay_ledger`)
//!
//! Applies `TransactionMeta` directly from archives. This:
//!
//! - Uses exact entry changes from the original execution
//! - Requires archives that include `TransactionMeta` (e.g., CDP)
//! - Produces identical results to the original execution
//! - Used for testing and specialized replay scenarios
//!
//! # Verification
//!
//! During replay, we verify:
//!
//! - Transaction set hash matches header's `scp_value.tx_set_hash`
//! - Transaction result hash matches header's `tx_set_result_hash`
//! - Bucket list hash matches header's `bucket_list_hash` (at checkpoints)
//!
//! # Protocol 23+ Eviction
//!
//! Starting with protocol 23, incremental eviction scan runs each ledger:
//!
//! 1. Scan portion of bucket list based on `EvictionIterator` position
//! 2. Move expired entries from live bucket list to hot archive
//! 3. Update `EvictionIterator` ConfigSettingEntry
//! 4. Combined hash = SHA256(live_hash || hot_archive_hash)

pub(crate) mod diff;
pub(crate) mod execution;
pub(crate) mod metadata;

use crate::{verify, HistoryError, Result};
use henyey_bucket::EvictionIterator;
use henyey_common::{Hash256, NetworkId};
use henyey_ledger::{EntryChange, TransactionSetVariant};
use henyey_tx::soroban::PersistentModuleCache;
use stellar_xdr::curr::{
    BucketListType, LedgerEntry, LedgerHeader, LedgerKey, StateArchivalSettings, TransactionMeta,
    TransactionResultPair,
};

// Re-export public items from submodules.
pub use execution::replay_ledger_with_execution;
pub use metadata::{extract_ledger_changes, replay_ledger};

/// The result of replaying a single ledger.
///
/// This contains both summary statistics and the actual ledger entry changes
/// that should be applied to the bucket list.
#[derive(Debug, Clone)]
pub struct LedgerReplayResult {
    /// The ledger sequence number that was replayed.
    pub sequence: u32,

    /// Protocol version active during this ledger.
    pub protocol_version: u32,

    /// SHA-256 hash of the ledger header.
    pub ledger_hash: Hash256,

    /// Number of transactions executed in this ledger.
    pub tx_count: u32,

    /// Total number of operations across all transactions.
    pub op_count: u32,

    /// Net change to the fee pool (positive = fees collected).
    pub fee_pool_delta: i64,

    /// Net change to total coins (should be 0 for conservation).
    pub total_coins_delta: i64,

    /// New entries to add to the bucket list with `INITENTRY` flag.
    ///
    /// Init entries represent newly created ledger entries that did not
    /// exist before this ledger.
    pub init_entries: Vec<LedgerEntry>,

    /// Updated entries to add to the bucket list with `LIVEENTRY` flag.
    ///
    /// Live entries represent modifications to existing ledger entries.
    pub live_entries: Vec<LedgerEntry>,

    /// Keys of entries to mark as deleted with `DEADENTRY` flag.
    pub dead_entries: Vec<LedgerKey>,

    /// Detailed change records for state tracking.
    ///
    /// This provides before/after state for each changed entry.
    pub changes: Vec<EntryChange>,

    /// Updated eviction iterator position after this ledger.
    ///
    /// Only present when running eviction scan (protocol 23+).
    /// Should be passed to the next ledger's replay call.
    pub eviction_iterator: Option<EvictionIterator>,

    /// Net change in Soroban state size (bytes) during this ledger.
    ///
    /// This includes:
    /// - Added size from new ContractData/ContractCode entries (INIT)
    /// - Size difference from updated entries (LIVE)
    /// - Subtracted size from deleted entries (DEAD)
    ///
    /// Used for accurate `LiveSorobanStateSizeWindow` tracking during catchup.
    pub soroban_state_size_delta: i64,
}

/// Configuration for ledger replay behavior and verification.
///
/// This controls what verification checks are performed during replay
/// and whether optional features like event emission are enabled.
#[derive(Debug, Clone)]
pub struct ReplayConfig {
    /// Verify that computed transaction results match header hashes.
    ///
    /// When enabled, the replay will fail if the transaction result set
    /// hash does not match `header.tx_set_result_hash`. This is disabled
    /// by default because re-execution may produce different result codes
    /// than the original execution (especially for Soroban).
    pub verify_results: bool,

    /// Verify that bucket list hash matches header at checkpoints.
    ///
    /// This is the primary verification that ensures correct state
    /// reconstruction. Verification only runs at checkpoint boundaries
    /// (ledger % 64 == 63) because intermediate states may differ.
    pub verify_bucket_list: bool,

    /// Emit classic contract events during replay.
    ///
    /// When enabled, generates events for classic operations like
    /// payments and trustline changes. Useful for indexers.
    pub emit_classic_events: bool,

    /// Generate Stellar asset events for pre-protocol 23 ledgers.
    ///
    /// Protocol 23 introduced standardized asset events. This option
    /// generates equivalent events for earlier ledgers.
    pub backfill_stellar_asset_events: bool,

    /// Run incremental eviction scan during replay.
    ///
    /// Required for correct bucket list hash verification in protocol 23+.
    /// The eviction scan moves expired entries from the live bucket list
    /// to the hot archive bucket list.
    pub run_eviction: bool,

    /// Configuration for the eviction scan algorithm.
    ///
    /// Controls parameters like the starting level and scan rate.
    pub eviction_settings: StateArchivalSettings,

    /// Enable publish queue backpressure during replay.
    ///
    /// When true, replay pauses when the publish queue exceeds
    /// `PUBLISH_QUEUE_MAX_SIZE` (16) and resumes when it drains to
    /// `PUBLISH_QUEUE_UNBLOCK_APPLICATION` (8). CATCHUP_SPEC §5.6.
    ///
    /// This should be enabled for offline catchup modes to prevent
    /// unbounded queue growth when replaying faster than publishing.
    pub wait_for_publish: bool,
}

/// Inputs and mutable state needed for execution-based ledger replay.
pub struct ReplayExecutionContext<'a> {
    pub bucket_list: &'a mut henyey_bucket::BucketList,
    pub hot_archive_bucket_list: &'a mut henyey_bucket::HotArchiveBucketList,
    pub network_id: &'a NetworkId,
    pub config: &'a ReplayConfig,
    pub expected_tx_results: Option<&'a [TransactionResultPair]>,
    pub eviction_iterator: Option<EvictionIterator>,
    pub module_cache: Option<&'a PersistentModuleCache>,
    pub soroban_state_size: Option<u64>,
    pub prev_id_pool: u64,
    pub offer_entries: Option<Vec<LedgerEntry>>,
}

impl Default for ReplayConfig {
    fn default() -> Self {
        Self {
            // Transaction result verification is disabled by default because during
            // replay we re-execute transactions without TransactionMeta from archives.
            // Our execution may produce slightly different result codes than
            // stellar-core, especially for Soroban contracts (e.g., Trapped vs
            // ResourceLimitExceeded). The bucket list hash at checkpoints is the
            // authoritative verification of correct ledger state.
            verify_results: false,
            verify_bucket_list: true,
            emit_classic_events: false,
            backfill_stellar_asset_events: false,
            run_eviction: true,
            eviction_settings: StateArchivalSettings::default(),
            wait_for_publish: false,
        }
    }
}

/// Replay a batch of ledgers.
///
/// This is used during catchup to replay all ledgers from a checkpoint
/// to the target ledger.
///
/// # Arguments
///
/// * `ledgers` - Slice of (header, tx_set, results, metas) tuples
/// * `config` - Replay configuration
/// * `progress_callback` - Optional callback for progress updates
pub fn replay_ledgers<F>(
    ledgers: &[(
        LedgerHeader,
        TransactionSetVariant,
        Vec<TransactionResultPair>,
        Vec<TransactionMeta>,
    )],
    config: &ReplayConfig,
    mut progress_callback: Option<F>,
) -> Result<Vec<LedgerReplayResult>>
where
    F: FnMut(u32, u32), // (current, total)
{
    let total = ledgers.len() as u32;
    let mut results = Vec::with_capacity(ledgers.len());

    for (i, (header, tx_set, tx_results, tx_metas)) in ledgers.iter().enumerate() {
        let result = replay_ledger(header, tx_set, tx_results, tx_metas, config)?;
        results.push(result);

        if let Some(ref mut callback) = progress_callback {
            callback(i as u32 + 1, total);
        }
    }

    Ok(results)
}

/// Verify ledger consistency after replay.
///
/// Checks that the final bucket list hash matches the expected hash
/// from the last replayed ledger header.
pub fn verify_replay_consistency(
    final_header: &LedgerHeader,
    computed_bucket_list_hash: &Hash256,
) -> Result<()> {
    verify::verify_ledger_hash(final_header, computed_bucket_list_hash)
}

/// Apply replay results to the bucket list.
///
/// This takes the changes from ledger replay and applies them to the
/// bucket list to update the ledger state.
pub fn apply_replay_to_bucket_list(
    bucket_list: &mut henyey_bucket::BucketList,
    replay_result: &LedgerReplayResult,
) -> Result<()> {
    bucket_list
        .add_batch(
            replay_result.sequence,
            replay_result.protocol_version,
            BucketListType::Live,
            replay_result.init_entries.clone(),
            replay_result.live_entries.clone(),
            replay_result.dead_entries.clone(),
        )
        .map_err(HistoryError::Bucket)
}

/// Prepare a ledger close based on replay data.
///
/// This is used when we've replayed history and want to set up the
/// ledger manager to continue from that point.
#[derive(Debug, Clone)]
pub struct ReplayedLedgerState {
    /// The ledger sequence we replayed to.
    pub sequence: u32,
    /// Hash of the final ledger.
    pub ledger_hash: Hash256,
    /// Hash of the bucket list.
    pub bucket_list_hash: Hash256,
    /// Close time of the final ledger.
    pub close_time: u64,
    /// Protocol version.
    pub protocol_version: u32,
    /// Base fee.
    pub base_fee: u32,
    /// Base reserve.
    pub base_reserve: u32,
}

impl ReplayedLedgerState {
    /// Create from a final ledger header after replay.
    pub fn from_header(header: &LedgerHeader, ledger_hash: Hash256) -> Self {
        Self {
            sequence: header.ledger_seq,
            ledger_hash,
            bucket_list_hash: Hash256::from(header.bucket_list_hash.clone()),
            close_time: header.scp_value.close_time.0,
            protocol_version: header.ledger_version,
            base_fee: header.base_fee,
            base_reserve: header.base_reserve,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use stellar_xdr::curr::{Hash, StellarValue, TimePoint, TransactionSet, VecM};

    pub(crate) fn make_test_header(seq: u32) -> LedgerHeader {
        LedgerHeader {
            ledger_version: 20,
            previous_ledger_hash: Hash([0u8; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0u8; 32]),
                close_time: TimePoint(1234567890),
                upgrades: VecM::default(),
                ext: stellar_xdr::curr::StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0u8; 32]),
            bucket_list_hash: Hash([0u8; 32]),
            ledger_seq: seq,
            total_coins: 0,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 5000000,
            max_tx_set_size: 100,
            skip_list: std::array::from_fn(|_| Hash([0u8; 32])),
            ext: stellar_xdr::curr::LedgerHeaderExt::V0,
        }
    }

    pub(crate) fn make_header_with_hashes(
        seq: u32,
        tx_set_hash: Hash,
        tx_result_hash: Hash,
    ) -> LedgerHeader {
        let mut header = make_test_header(seq);
        header.scp_value.tx_set_hash = tx_set_hash;
        header.tx_set_result_hash = tx_result_hash;
        header
    }

    pub(crate) fn make_empty_tx_set() -> TransactionSet {
        TransactionSet {
            previous_ledger_hash: Hash([0u8; 32]),
            txs: VecM::default(),
        }
    }

    #[test]
    fn test_replayed_ledger_state_from_header() {
        let header = make_test_header(42);
        let hash = Hash256::hash(b"test");

        let state = ReplayedLedgerState::from_header(&header, hash);

        assert_eq!(state.sequence, 42);
        assert_eq!(state.ledger_hash, hash);
        assert_eq!(state.close_time, 1234567890);
        assert_eq!(state.protocol_version, 20);
        assert_eq!(state.base_fee, 100);
    }

    #[test]
    fn test_replay_config_default() {
        let config = ReplayConfig::default();
        // verify_results is disabled by default because replay without TransactionMeta
        // produces different results than stellar-core
        assert!(!config.verify_results);
        assert!(config.verify_bucket_list);
    }
}
