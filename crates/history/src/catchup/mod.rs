//! Catchup manager for synchronizing from history archives.
//!
//! This module provides the [`CatchupManager`] which orchestrates the complete
//! process of synchronizing a node with the Stellar network using history archives.
//!
//! # Overview
//!
//! Catchup is the process of downloading historical data and rebuilding ledger
//! state to match the current network. This is required when:
//!
//! - Starting a new node from scratch
//! - Recovering a node that fell too far behind
//! - Rebuilding state after data corruption
//!
//! # Catchup Process
//!
//! The catchup process follows these steps:
//!
//! 1. **Find checkpoint**: Locate the latest checkpoint at or before the target ledger
//! 2. **Download HAS**: Fetch the History Archive State describing that checkpoint
//! 3. **Download buckets**: Fetch all bucket files referenced in the HAS
//! 4. **Apply buckets**: Build the initial ledger state from bucket entries
//! 5. **Download ledger data**: Fetch headers, transactions, and results
//! 6. **Verify chain**: Validate the cryptographic hash chain
//! 7. **Replay ledgers**: Re-execute transactions from checkpoint to target
//!
//! # Re-execution vs Metadata Replay
//!
//! During catchup, we **re-execute** transactions against the bucket list state
//! rather than simply applying `TransactionMeta` from archives. This approach:
//!
//! - Keeps bucket list evolution consistent with transaction effects
//! - Allows verification of transaction set and result hashes
//! - Works with traditional archives that do not include `TransactionMeta`
//!
//! The trade-off is that re-execution may produce slightly different internal
//! results than the original execution (e.g., different Soroban error codes),
//! though the final state should match. For exact verification, use CDP data
//! with `TransactionMeta`.
//!
//! # Protocol 23+ Considerations
//!
//! Starting with protocol 23, state archival introduces complexity:
//!
//! - Evicted entries move from live bucket list to hot archive
//! - Incremental eviction scan must run each ledger for correct hashes
//! - Bucket list hash = SHA256(live_hash || hot_archive_hash)

mod buckets;
mod download;
mod persist;
mod replay;

use crate::{
    archive::HistoryArchive,
    archive_state::HistoryArchiveState,
    catchup_range::{CatchupMode, CatchupRange},
    checkpoint,
    replay::ReplayConfig,
    verify, CatchupResult, HistoryError, Result,
};
use henyey_bucket::{BucketList, BucketManager, HotArchiveBucketList};
use henyey_common::{Hash256, NetworkId};
use henyey_db::Database;
use std::collections::HashMap;
use std::sync::Arc;

use henyey_ledger::{LedgerManager, TransactionSetVariant};
use stellar_xdr::curr::LedgerCloseMeta;
use stellar_xdr::curr::{
    ExtensionPoint, GeneralizedTransactionSet, Hash, LedgerCloseMetaExt, LedgerCloseMetaExtV1,
    LedgerCloseMetaV2, LedgerHeader, LedgerHeaderHistoryEntry, LedgerHeaderHistoryEntryExt,
    ScpHistoryEntry, TransactionHistoryEntry, TransactionHistoryResultEntry, TransactionResultPair,
    TransactionSetV1, WriteXdr,
};
use tracing::{debug, info};

/// Current status of a catchup operation.
///
/// This enum represents the discrete phases of the catchup process,
/// allowing callers to track progress and provide user feedback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatchupStatus {
    /// Catchup has not started yet.
    Pending,
    /// Downloading the History Archive State (HAS) file.
    DownloadingHAS,
    /// Downloading bucket files from the archive.
    DownloadingBuckets,
    /// Applying downloaded buckets to build initial ledger state.
    ApplyingBuckets,
    /// Downloading ledger headers, transactions, and results.
    DownloadingLedgers,
    /// Verifying cryptographic hashes and chain integrity.
    Verifying,
    /// Re-executing transactions to reach target ledger.
    Replaying,
    /// Catchup completed successfully.
    Completed,
    /// Catchup failed (check error for details).
    Failed,
}

/// Detailed progress information for a catchup operation.
///
/// This struct provides fine-grained progress tracking, useful for
/// displaying progress bars or status updates to users during the
/// potentially long-running catchup process.
#[derive(Debug, Clone)]
pub struct CatchupProgress {
    /// Current phase of the catchup process.
    pub status: CatchupStatus,

    /// Current step number (1-based index into the 7-step process).
    pub current_step: u32,

    /// Total number of steps in the catchup process (currently 7).
    pub total_steps: u32,

    /// Number of bucket files downloaded so far.
    ///
    /// Only meaningful when `status` is `DownloadingBuckets`.
    pub buckets_downloaded: u32,

    /// Total number of bucket files to download.
    ///
    /// Only meaningful when `status` is `DownloadingBuckets`.
    pub buckets_total: u32,

    /// Current ledger being processed during replay.
    ///
    /// Only meaningful when `status` is `Replaying` or `DownloadingLedgers`.
    pub current_ledger: u32,

    /// Target ledger sequence for this catchup operation.
    pub target_ledger: u32,

    /// Human-readable description of current activity.
    pub message: String,
}

impl Default for CatchupProgress {
    fn default() -> Self {
        Self {
            status: CatchupStatus::Pending,
            current_step: 0,
            total_steps: 7,
            buckets_downloaded: 0,
            buckets_total: 0,
            current_ledger: 0,
            target_ledger: 0,
            message: String::new(),
        }
    }
}

/// Pre-downloaded checkpoint data for catchup.
///
/// This struct holds all the data needed for catchup when it has been
/// pre-fetched (e.g., for testing or when using an alternative data source).
/// Pass this to [`CatchupManager::catchup_to_ledger_with_checkpoint_data`]
/// to skip the download phase.
#[derive(Debug, Clone)]
pub struct CheckpointData {
    /// The History Archive State describing this checkpoint.
    pub has: HistoryArchiveState,

    /// Directory where bucket files are stored on disk (keyed by hash).
    /// Bucket files are named `<hex_hash>.bucket`.
    pub bucket_dir: std::path::PathBuf,

    /// Ledger headers for the checkpoint range.
    pub headers: Vec<LedgerHeaderHistoryEntry>,

    /// Transaction history entries for the checkpoint.
    pub transactions: Vec<TransactionHistoryEntry>,

    /// Transaction result entries for the checkpoint.
    pub tx_results: Vec<TransactionHistoryResultEntry>,

    /// SCP history entries for consensus verification.
    pub scp_history: Vec<ScpHistoryEntry>,
}

/// Manager for synchronizing ledger state from history archives.
///
/// The `CatchupManager` orchestrates the complete catchup process:
/// downloading data from archives, verifying integrity, and replaying
/// transactions to reach a target ledger.
///
/// # Usage
///
/// ```no_run
/// use henyey_history::{CatchupManager, archive::HistoryArchive};
/// use henyey_bucket::BucketManager;
/// use henyey_db::Database;
/// use henyey_ledger::{LedgerManager, LedgerManagerConfig};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let archive = HistoryArchive::new("https://history.stellar.org/prd/core-testnet/core_testnet_001")?;
/// let bucket_manager = BucketManager::new("/tmp/buckets".into())?;
/// let db = Database::open("/tmp/stellar.db")?;
/// let ledger_manager = LedgerManager::new("Test SDF Network ; September 2015".to_string(), LedgerManagerConfig::default());
///
/// let mut manager = CatchupManager::new(vec![archive], bucket_manager, db);
/// let output = manager.catchup_to_ledger(1000000, &ledger_manager).await?;
///
/// println!("Caught up to ledger {}", output.ledger_seq);
/// # Ok(())
/// # }
/// ```
///
/// Existing bucket list state for replay-only catchup (Case 1).
///
/// When the LCL is past genesis (e.g., after a prior catchup), the node already
/// has valid bucket lists in memory. Instead of re-downloading all buckets from
/// the archive, we can replay ledger transactions against the existing state.
/// This reduces catchup time from ~60s (27s bucket download + 31s cache rebuild)
/// to just a few seconds (download tx history + replay).
pub struct ExistingBucketState {
    /// The live bucket list at the current LCL.
    pub bucket_list: BucketList,
    /// The hot archive bucket list at the current LCL.
    pub hot_archive_bucket_list: HotArchiveBucketList,
    /// The ledger header at the current LCL.
    pub header: LedgerHeader,
    /// The network ID for transaction verification.
    pub network_id: NetworkId,
}

/// # Thread Safety
///
/// The `CatchupManager` is not `Send` or `Sync` due to its mutable progress
/// tracking. Create a new manager for each catchup operation.
/// Callback invoked for each replayed ledger's metadata.
///
/// When set on a [`CatchupManager`], this callback receives the
/// `LedgerCloseMeta` produced by each replayed ledger during catchup.
/// This enables streaming metadata to external consumers (e.g., the
/// meta pipe used by stellar-rpc in bounded replay mode).
pub type MetaCallback = Box<dyn Fn(LedgerCloseMeta) + Send + Sync>;

pub struct CatchupManager {
    /// History archives to download from (tried in order with failover).
    pub(super) archives: Vec<Arc<HistoryArchive>>,

    /// Manager for bucket file storage and retrieval.
    pub(super) bucket_manager: Arc<BucketManager>,

    /// Database for persisting ledger state and history.
    pub(super) db: Arc<Database>,

    /// Current progress information for status reporting.
    pub(super) progress: CatchupProgress,

    /// Configuration for the replay phase.
    pub(super) replay_config: ReplayConfig,

    /// Configured network passphrase for HAS validation.
    ///
    /// When set, the HAS `networkPassphrase` field is validated against this
    /// value during catchup (per stellar-core §8.2 step 2). If `None`,
    /// passphrase validation is skipped.
    pub(super) network_passphrase: Option<String>,

    /// Optional callback for streaming metadata from replayed ledgers.
    ///
    /// Called once per ledger during the replay phase with the `LedgerCloseMeta`
    /// produced by `close_ledger`. Used by stellar-rpc's bounded replay mode
    /// (`catchup --metadata-output-stream fd:3`).
    pub(super) meta_callback: Option<MetaCallback>,
    /// When true, synthetic bucket-apply meta uses `LedgerCloseMetaExtV1`
    /// (matching the live mode setting `EMIT_LEDGER_CLOSE_META_EXT_V1`).
    pub(super) emit_meta_ext_v1: bool,
}

impl CatchupManager {
    /// Create a new catchup manager.
    ///
    /// # Arguments
    ///
    /// * `archives` - List of history archives to use (will try in order)
    /// * `bucket_manager` - Manager for bucket file operations
    /// * `db` - Database for persisting ledger state
    pub fn new(archives: Vec<HistoryArchive>, bucket_manager: BucketManager, db: Database) -> Self {
        Self {
            archives: archives.into_iter().map(Arc::new).collect(),
            bucket_manager: Arc::new(bucket_manager),
            db: Arc::new(db),
            progress: CatchupProgress::default(),
            replay_config: ReplayConfig::default(),
            network_passphrase: None,
            meta_callback: None,
            emit_meta_ext_v1: false,
        }
    }

    /// Create a new catchup manager from Arc references.
    pub fn new_with_arcs(
        archives: Vec<Arc<HistoryArchive>>,
        bucket_manager: Arc<BucketManager>,
        db: Arc<Database>,
    ) -> Self {
        Self {
            archives,
            bucket_manager,
            db,
            progress: CatchupProgress::default(),
            replay_config: ReplayConfig::default(),
            network_passphrase: None,
            meta_callback: None,
            emit_meta_ext_v1: false,
        }
    }

    /// Get the current catchup progress.
    pub fn progress(&self) -> &CatchupProgress {
        &self.progress
    }

    /// Set the replay configuration.
    pub fn set_replay_config(&mut self, config: ReplayConfig) {
        self.replay_config = config;
    }

    /// Set the expected network passphrase for HAS validation.
    ///
    /// When set, the `networkPassphrase` field of downloaded HAS files is
    /// validated against this value. A mismatch causes catchup to fail
    /// immediately (per stellar-core §8.2 step 2).
    pub fn set_network_passphrase(&mut self, passphrase: String) {
        self.network_passphrase = Some(passphrase);
    }

    /// Set a callback for streaming metadata from replayed ledgers.
    ///
    /// The callback receives `LedgerCloseMeta` for each ledger replayed
    /// during catchup. This is used by stellar-rpc's bounded replay mode
    /// to stream meta over the `--metadata-output-stream` pipe.
    pub fn set_meta_callback(&mut self, callback: MetaCallback) {
        self.meta_callback = Some(callback);
    }

    /// Configure whether synthetic bucket-apply meta should use ExtV1.
    ///
    /// Must match the `emit_ledger_close_meta_ext_v1` setting used by the
    /// live ledger close path so captive core consumers see a consistent
    /// meta extension version.
    pub fn set_emit_meta_ext_v1(&mut self, enabled: bool) {
        self.emit_meta_ext_v1 = enabled;
    }

    /// Select an archive for a download attempt, rotating through available archives.
    ///
    /// Uses `attempt % archives.len()` to distribute retries across different archives,
    /// providing failover when one archive is down or slow. This matches stellar-core's
    /// archive selection strategy for retry resilience.
    pub(super) fn select_archive(&self, attempt: u32) -> &Arc<HistoryArchive> {
        let index = (attempt as usize) % self.archives.len();
        &self.archives[index]
    }

    /// Update the progress status.
    pub(super) fn update_progress(&mut self, status: CatchupStatus, step: u32, message: &str) {
        self.progress.status = status;
        self.progress.current_step = step;
        self.progress.message = message.to_string();
        debug!(
            "Catchup progress: step {}/{} - {}",
            step, self.progress.total_steps, message
        );
    }

    /// Apply buckets from a HAS, restart merges, persist the bucket list snapshot,
    /// and initialize the LedgerManager at the checkpoint state.
    ///
    /// This is the shared "bucket apply → init ledger manager" pipeline used by
    /// all catchup variants that need to rebuild state from buckets.
    async fn apply_buckets_and_init_ledger_manager(
        &mut self,
        has: &HistoryArchiveState,
        buckets: &[(Hash256, Vec<u8>)],
        checkpoint_seq: u32,
        checkpoint_header: LedgerHeader,
        checkpoint_hash: Hash256,
        ledger_manager: &LedgerManager,
    ) -> Result<()> {
        self.update_progress(
            CatchupStatus::ApplyingBuckets,
            3,
            "Applying buckets to build initial state",
        );
        let (mut bucket_list, mut hot_archive_bucket_list, live_next_states, hot_next_states) =
            self.apply_buckets(has, buckets).await?;

        self.restart_merges(
            &mut bucket_list,
            &mut hot_archive_bucket_list,
            checkpoint_seq,
            &live_next_states,
            &hot_next_states,
        )
        .await?;

        self.persist_bucket_list_snapshot(checkpoint_seq, &bucket_list)?;

        if ledger_manager.is_initialized() {
            ledger_manager.reset();
        }
        ledger_manager
            .initialize(
                bucket_list,
                hot_archive_bucket_list,
                checkpoint_header,
                checkpoint_hash,
            )
            .map_err(|e| {
                HistoryError::CatchupFailed(format!("Failed to initialize ledger manager: {}", e))
            })?;

        Ok(())
    }

    /// Shared tail of every catchup variant: either replay ledgers or emit
    /// synthetic bucket-apply meta, then build the final [`CatchupResult`].
    ///
    /// # Arguments
    ///
    /// * `target` — target ledger the caller wants to reach.
    /// * `checkpoint_seq` — the checkpoint at which buckets were applied.
    /// * `checkpoint_header` / `checkpoint_hash` — header at `checkpoint_seq`.
    /// * `buckets_downloaded` — count for the output struct.
    /// * `ledger_manager` — used by the replay path.
    async fn replay_and_finish(
        &mut self,
        target: u32,
        checkpoint_seq: u32,
        checkpoint_header: &LedgerHeader,
        checkpoint_hash: Hash256,
        buckets_downloaded: u32,
        ledger_manager: &LedgerManager,
    ) -> Result<CatchupResult> {
        let (final_header, final_hash, ledgers_applied) = if checkpoint_seq >= target {
            // No replay needed — target is exactly at checkpoint.
            self.persist_header_only(checkpoint_header)?;

            // Emit synthetic LedgerCloseMeta for the bucket-applied ledger.
            // Captive core consumers (stellar-rpc, horizon) need at least one
            // frame on fd:3 to know core is initialized.
            let meta =
                build_bucket_apply_meta(checkpoint_header, checkpoint_hash, self.emit_meta_ext_v1);
            info!(
                "Emitting synthetic LedgerCloseMeta for bucket-applied ledger {}",
                checkpoint_header.ledger_seq
            );
            self.emit_meta(checkpoint_header.ledger_seq, meta);

            (checkpoint_header.clone(), checkpoint_hash, 0)
        } else {
            let (header, hash, applied) = self
                .download_verify_and_replay_with_retry(target, ledger_manager)
                .await?;
            (header, hash, applied)
        };

        self.update_progress(CatchupStatus::Completed, 7, "Catchup completed");

        info!(
            "Catchup completed: ledger {}, hash {}, {} ledgers replayed",
            final_header.ledger_seq, final_hash, ledgers_applied
        );

        Ok(CatchupResult {
            ledger_seq: final_header.ledger_seq,
            ledger_hash: final_hash,
            ledgers_applied,
            buckets_downloaded,
        })
    }

    /// Catch up to a specific target ledger.
    ///
    /// This is the main entry point for the catchup process. It will:
    /// 1. Find the latest checkpoint before or at the target
    /// 2. Download and apply the state at that checkpoint
    /// 3. Replay ledgers from the checkpoint to the target (if any)
    ///
    /// # Arguments
    ///
    /// * `target` - The target ledger sequence to catch up to
    ///
    /// # Returns
    ///
    /// A `CatchupResult` containing the bucket list, header, and summary information.
    pub async fn catchup_to_ledger(
        &mut self,
        target: u32,
        ledger_manager: &LedgerManager,
    ) -> Result<CatchupResult> {
        info!("Starting catchup to ledger {}", target);
        self.progress.target_ledger = target;

        // Step 1: Find the latest checkpoint <= target
        let checkpoint_seq =
            checkpoint::latest_checkpoint_before_or_at(target).ok_or_else(|| {
                HistoryError::CatchupFailed(format!(
                    "target ledger {} is before first checkpoint",
                    target
                ))
            })?;

        info!(
            "Using checkpoint {} for catchup to {}",
            checkpoint_seq, target
        );

        // Step 2: Download the History Archive State
        self.update_progress(
            CatchupStatus::DownloadingHAS,
            1,
            "Downloading History Archive State",
        );
        let has = self.download_has(checkpoint_seq).await?;
        verify::verify_has_structure(&has)?;
        verify::verify_has_checkpoint(&has, checkpoint_seq)?;
        if let Some(ref expected) = self.network_passphrase {
            verify::verify_has_passphrase(&has, expected)?;
        }

        let scp_history = self.download_scp_history(checkpoint_seq).await?;
        if !scp_history.is_empty() {
            verify::verify_scp_history_entries(&scp_history)?;
            self.persist_scp_history_entries(&scp_history)?;
        }

        // Step 3: Download all buckets referenced in the HAS
        self.update_progress(
            CatchupStatus::DownloadingBuckets,
            2,
            "Downloading bucket files",
        );
        let bucket_hashes = self.compute_bucket_download_set(&has);
        let buckets_total = bucket_hashes.len() as u32;
        self.progress.buckets_total = buckets_total;
        let buckets = self.download_buckets(&bucket_hashes).await?;

        // Steps 4-5: Apply buckets and initialize LedgerManager
        let (checkpoint_header, checkpoint_hash) =
            self.download_checkpoint_header(checkpoint_seq).await?;

        self.apply_buckets_and_init_ledger_manager(
            &has,
            &buckets,
            checkpoint_seq,
            checkpoint_header.clone(),
            checkpoint_hash,
            ledger_manager,
        )
        .await?;

        // Steps 6-7: Replay ledgers (or emit synthetic meta) and finish.
        self.replay_and_finish(
            target,
            checkpoint_seq,
            &checkpoint_header,
            checkpoint_hash,
            buckets_total,
            ledger_manager,
        )
        .await
    }

    /// Catch up to a target ledger with a specific catchup mode.
    ///
    /// This method calculates the appropriate checkpoint and replay range based on
    /// the mode (Minimal, Complete, or Recent(N)).
    ///
    /// # Arguments
    ///
    /// * `target` - The target ledger sequence to catch up to
    /// * `mode` - The catchup mode determining history depth
    /// * `lcl` - The current Last Closed Ledger (use GENESIS_LEDGER_SEQ if starting fresh)
    /// * `existing_state` - If provided, contains the existing bucket lists and header
    ///   for replay-only catchup (Case 1: LCL > genesis). When `None`, Case 1 will
    ///   return an error (bucket lists are required for replay without re-downloading).
    ///
    /// # Returns
    ///
    /// A `CatchupResult` containing the bucket list, header, and summary information.
    pub async fn catchup_to_ledger_with_mode(
        &mut self,
        target: u32,
        mode: CatchupMode,
        lcl: u32,
        existing_state: Option<ExistingBucketState>,
        ledger_manager: &LedgerManager,
    ) -> Result<CatchupResult> {
        info!(
            "Starting catchup to ledger {} with mode {:?}, lcl={}",
            target, mode, lcl
        );
        self.progress.target_ledger = target;

        // Calculate the catchup range based on mode
        let range = CatchupRange::calculate(lcl, target, mode);
        info!(
            "Catchup range: apply_buckets={}, bucket_apply_ledger={}, replay_first={}, replay_count={}",
            range.apply_buckets(),
            if range.apply_buckets() { range.bucket_apply_ledger() } else { 0 },
            range.replay_first(),
            range.replay_count()
        );

        let checkpoint_seq = if range.apply_buckets() {
            // Apply buckets at the calculated checkpoint
            let bucket_apply_at = range.bucket_apply_ledger();
            info!("Applying buckets at checkpoint {}", bucket_apply_at);

            // Download HAS
            self.update_progress(
                CatchupStatus::DownloadingHAS,
                1,
                "Downloading History Archive State",
            );
            let has = self.download_has(bucket_apply_at).await?;
            verify::verify_has_structure(&has)?;
            verify::verify_has_checkpoint(&has, bucket_apply_at)?;
            if let Some(ref expected) = self.network_passphrase {
                verify::verify_has_passphrase(&has, expected)?;
            }

            let scp_history = self.download_scp_history(bucket_apply_at).await?;
            if !scp_history.is_empty() {
                verify::verify_scp_history_entries(&scp_history)?;
                self.persist_scp_history_entries(&scp_history)?;
            }

            // Download buckets
            self.update_progress(
                CatchupStatus::DownloadingBuckets,
                2,
                "Downloading bucket files",
            );
            let bucket_hashes = self.compute_bucket_download_set(&has);
            self.progress.buckets_total = bucket_hashes.len() as u32;
            let buckets = self.download_buckets(&bucket_hashes).await?;

            // Apply buckets and initialize LedgerManager
            let (checkpoint_header, checkpoint_hash) =
                self.download_checkpoint_header(bucket_apply_at).await?;

            self.apply_buckets_and_init_ledger_manager(
                &has,
                &buckets,
                bucket_apply_at,
                checkpoint_header,
                checkpoint_hash,
                ledger_manager,
            )
            .await?;

            bucket_apply_at
        } else {
            // No bucket application - Case 1: replay from current state.
            // The LedgerManager is already initialized at the current LCL.
            if !ledger_manager.is_initialized() {
                // If we have existing state but ledger manager is not initialized,
                // initialize it with the existing bucket state.
                match existing_state {
                    Some(state) => {
                        info!(
                            "Case 1 replay: initializing ledger manager from existing state at LCL {}",
                            lcl
                        );
                        let (header, hash) = self.download_checkpoint_header(lcl).await?;
                        ledger_manager
                            .initialize(
                                state.bucket_list,
                                state.hot_archive_bucket_list,
                                header,
                                hash,
                            )
                            .map_err(|e| {
                                HistoryError::CatchupFailed(format!(
                                    "Failed to initialize ledger manager from existing state: {}",
                                    e
                                ))
                            })?;
                    }
                    None => {
                        return Err(HistoryError::CatchupFailed(
                            "Catchup from LCL > genesis requires existing bucket lists or an initialized ledger manager"
                                .to_string(),
                        ));
                    }
                }
            }
            lcl
        };

        // Download, verify, and replay ledgers with retry.
        // Matches stellar-core's DownloadApplyTxsWork(RETRY_A_FEW).
        if range.replay_count() == 0 {
            info!(
                "Catching up to checkpoint {} (no ledgers to replay)",
                checkpoint_seq
            );
        }
        let (header, hash) = self.download_checkpoint_header(checkpoint_seq).await?;
        self.replay_and_finish(
            target,
            checkpoint_seq,
            &header,
            hash,
            self.progress.buckets_total,
            ledger_manager,
        )
        .await
    }

    /// Catch up to a target ledger using pre-downloaded checkpoint data.
    pub async fn catchup_to_ledger_with_checkpoint_data(
        &mut self,
        target: u32,
        data: CheckpointData,
        ledger_manager: &LedgerManager,
    ) -> Result<CatchupResult> {
        info!("Starting catchup to ledger {} with checkpoint data", target);
        self.progress.target_ledger = target;

        let checkpoint_seq =
            checkpoint::latest_checkpoint_before_or_at(target).ok_or_else(|| {
                HistoryError::CatchupFailed(format!(
                    "target ledger {} is before first checkpoint",
                    target
                ))
            })?;

        if data.has.current_ledger != checkpoint_seq {
            return Err(HistoryError::CatchupFailed(format!(
                "checkpoint data ledger {} does not match target checkpoint {}",
                data.has.current_ledger, checkpoint_seq
            )));
        }

        // Step 2: Verify HAS
        self.update_progress(
            CatchupStatus::DownloadingHAS,
            1,
            "Using provided History Archive State",
        );
        verify::verify_has_structure(&data.has)?;
        verify::verify_has_checkpoint(&data.has, checkpoint_seq)?;
        if let Some(ref expected) = self.network_passphrase {
            verify::verify_has_passphrase(&data.has, expected)?;
        }

        // Step 3: Verify bucket files exist on disk
        // Hash verification was already performed at download time by DownloadBucketsWork.
        // The apply_buckets step will re-verify hashes when loading each bucket lazily.
        self.update_progress(
            CatchupStatus::DownloadingBuckets,
            2,
            "Verifying bucket files on disk",
        );
        let bucket_hashes = data.has.unique_bucket_hashes();
        let empty_bucket_hash = Hash256::hash(&[]);
        let buckets_downloaded = bucket_hashes.len() as u32;
        self.progress.buckets_total = buckets_downloaded;
        for (idx, hash) in bucket_hashes.iter().enumerate() {
            if !hash.is_zero() && *hash != empty_bucket_hash {
                let bucket_path = data
                    .bucket_dir
                    .join(format!("{}.bucket.xdr", hash.to_hex()));
                if !bucket_path.exists() {
                    return Err(HistoryError::BucketNotFound(*hash));
                }
            }
            self.progress.buckets_downloaded = (idx + 1) as u32;
        }

        // Copy bucket files to the bucket manager directory if they're in a different location
        let bucket_mgr_dir = self.bucket_manager.bucket_dir().to_path_buf();
        if data.bucket_dir != bucket_mgr_dir {
            for hash in &bucket_hashes {
                if hash.is_zero() || *hash == empty_bucket_hash {
                    continue;
                }
                let src = data
                    .bucket_dir
                    .join(format!("{}.bucket.xdr", hash.to_hex()));
                let dst = bucket_mgr_dir.join(format!("{}.bucket.xdr", hash.to_hex()));
                if src.exists() && !dst.exists() {
                    std::fs::copy(&src, &dst).map_err(|e| {
                        HistoryError::CatchupFailed(format!(
                            "failed to copy bucket {} to bucket manager dir: {}",
                            hash, e
                        ))
                    })?;
                }
            }
        }

        // Step 4: Verify SCP history entries (if present)
        if !data.scp_history.is_empty() {
            verify::verify_scp_history_entries(&data.scp_history)?;
            self.persist_scp_history_entries(&data.scp_history)?;
        }

        // Step 5: Apply buckets and initialize LedgerManager
        let (checkpoint_header, checkpoint_hash) =
            checkpoint_header_from_headers(checkpoint_seq, &data.headers)?;

        self.apply_buckets_and_init_ledger_manager(
            &data.has,
            &[],
            checkpoint_seq,
            checkpoint_header.clone(),
            checkpoint_hash,
            ledger_manager,
        )
        .await?;

        // Step 6: Build ledger data from checkpoint files
        self.update_progress(
            CatchupStatus::DownloadingLedgers,
            4,
            "Using provided ledger data",
        );
        let mut header_map = HashMap::new();
        for entry in &data.headers {
            header_map.insert(entry.header.ledger_seq, entry.header.clone());
        }
        for entry in &data.tx_results {
            if entry.ledger_seq <= checkpoint_seq || entry.ledger_seq > target {
                continue;
            }
            if let Some(header) = header_map.get(&entry.ledger_seq) {
                let xdr = entry
                    .tx_result_set
                    .to_xdr(stellar_xdr::curr::Limits::none())
                    .map_err(|e| {
                        HistoryError::CatchupFailed(format!(
                            "failed to encode tx result set: {}",
                            e
                        ))
                    })?;
                verify::verify_tx_result_set(header, &xdr)?;
            }
        }

        // Steps 6-7: Replay ledgers (or emit synthetic meta) and finish.
        self.replay_and_finish(
            target,
            checkpoint_seq,
            &checkpoint_header,
            checkpoint_hash,
            buckets_downloaded,
            ledger_manager,
        )
        .await
    }
}

fn checkpoint_header_from_headers(
    checkpoint_seq: u32,
    headers: &[LedgerHeaderHistoryEntry],
) -> Result<(LedgerHeader, Hash256)> {
    for entry in headers {
        if entry.header.ledger_seq == checkpoint_seq {
            return Ok((entry.header.clone(), Hash256::from(entry.hash.0)));
        }
    }

    Err(HistoryError::CatchupFailed(format!(
        "checkpoint header {} not found in headers",
        checkpoint_seq
    )))
}

/// Data downloaded for a single ledger.
#[derive(Debug, Clone)]
pub struct LedgerData {
    /// The ledger header.
    pub header: LedgerHeader,
    /// The transaction set.
    pub tx_set: TransactionSetVariant,
    /// Transaction results.
    pub tx_results: Vec<TransactionResultPair>,
    /// Transaction history entry (tx set) when available.
    pub tx_history_entry: Option<TransactionHistoryEntry>,
    /// Transaction result history entry when available.
    pub tx_result_entry: Option<TransactionHistoryResultEntry>,
}

/// Options for catchup operations.
#[derive(Debug, Clone)]
pub struct CatchupOptions {
    /// Whether to verify bucket hashes.
    pub verify_buckets: bool,
    /// Whether to verify header chain.
    pub verify_headers: bool,
}

impl Default for CatchupOptions {
    fn default() -> Self {
        Self {
            verify_buckets: true,
            verify_headers: true,
        }
    }
}

/// Builder for creating a CatchupManager with custom options.
pub struct CatchupManagerBuilder {
    archives: Vec<HistoryArchive>,
    bucket_manager: Option<BucketManager>,
    db: Option<Database>,
    options: CatchupOptions,
    network_passphrase: Option<String>,
}

impl CatchupManagerBuilder {
    /// Create a new builder.
    pub fn new() -> Self {
        Self {
            archives: Vec::new(),
            bucket_manager: None,
            db: None,
            options: CatchupOptions::default(),
            network_passphrase: None,
        }
    }

    /// Add a history archive.
    pub fn add_archive(mut self, archive: HistoryArchive) -> Self {
        self.archives.push(archive);
        self
    }

    /// Set the bucket manager.
    pub fn bucket_manager(mut self, manager: BucketManager) -> Self {
        self.bucket_manager = Some(manager);
        self
    }

    /// Set the database.
    pub fn database(mut self, db: Database) -> Self {
        self.db = Some(db);
        self
    }

    /// Set catchup options.
    pub fn options(mut self, options: CatchupOptions) -> Self {
        self.options = options;
        self
    }

    /// Set the expected network passphrase for HAS validation.
    pub fn network_passphrase(mut self, passphrase: String) -> Self {
        self.network_passphrase = Some(passphrase);
        self
    }

    /// Build the CatchupManager.
    pub fn build(self) -> Result<CatchupManager> {
        let bucket_manager = self
            .bucket_manager
            .ok_or_else(|| HistoryError::CatchupFailed("bucket manager required".to_string()))?;

        let db = self
            .db
            .ok_or_else(|| HistoryError::CatchupFailed("database required".to_string()))?;

        if self.archives.is_empty() {
            return Err(HistoryError::CatchupFailed(
                "at least one archive required".to_string(),
            ));
        }

        let mut manager = CatchupManager::new(self.archives, bucket_manager, db);

        if let Some(passphrase) = self.network_passphrase {
            manager.set_network_passphrase(passphrase);
        }

        manager.replay_config = ReplayConfig {
            verify_results: self.options.verify_headers,
            verify_bucket_list: self.options.verify_buckets,
            emit_classic_events: false,
            backfill_stellar_asset_events: false,
            run_eviction: true,
            eviction_settings: stellar_xdr::curr::StateArchivalSettings::default(),
            wait_for_publish: false,
        };

        Ok(manager)
    }
}

impl Default for CatchupManagerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a minimal `LedgerCloseMeta` for a bucket-applied ledger (no replay).
///
/// When catchup applies buckets directly to a checkpoint (replay_count == 0),
/// captive core consumers (stellar-rpc, horizon) still need at least one
/// metadata frame on fd:3 to know the core is initialized. This constructs
/// a V2 meta with the checkpoint header and empty transaction/upgrade data.
fn build_bucket_apply_meta(
    header: &LedgerHeader,
    hash: Hash256,
    emit_ext_v1: bool,
) -> LedgerCloseMeta {
    let ext = if emit_ext_v1 {
        LedgerCloseMetaExt::V1(LedgerCloseMetaExtV1 {
            ext: ExtensionPoint::V0,
            soroban_fee_write1_kb: 0,
        })
    } else {
        LedgerCloseMetaExt::V0
    };
    LedgerCloseMeta::V2(LedgerCloseMetaV2 {
        ext,
        ledger_header: LedgerHeaderHistoryEntry {
            hash: Hash(hash.0),
            header: header.clone(),
            ext: LedgerHeaderHistoryEntryExt::V0,
        },
        tx_set: GeneralizedTransactionSet::V1(TransactionSetV1 {
            previous_ledger_hash: header.previous_ledger_hash.clone(),
            phases: Default::default(),
        }),
        tx_processing: Default::default(),
        upgrades_processing: Default::default(),
        scp_info: Default::default(),
        total_byte_size_of_live_soroban_state: 0,
        evicted_keys: Default::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_catchup_options_default() {
        let options = CatchupOptions::default();
        assert!(options.verify_buckets);
        assert!(options.verify_headers);
    }

    #[test]
    fn test_catchup_progress_default() {
        let progress = CatchupProgress::default();
        assert_eq!(progress.status, CatchupStatus::Pending);
        assert_eq!(progress.current_step, 0);
        assert_eq!(progress.total_steps, 7);
    }

    #[test]
    fn test_catchup_status() {
        assert_eq!(CatchupStatus::Pending, CatchupStatus::Pending);
        assert_ne!(CatchupStatus::Pending, CatchupStatus::Completed);
    }

    #[test]
    fn test_select_archive_rotation_logic() {
        // Verify the rotation logic: attempt % len gives round-robin
        let num_archives = 3usize;

        let select = |attempt: u32| -> usize { (attempt as usize) % num_archives };

        assert_eq!(select(0), 0);
        assert_eq!(select(1), 1);
        assert_eq!(select(2), 2);
        // Wraps around
        assert_eq!(select(3), 0);
        assert_eq!(select(4), 1);
        assert_eq!(select(5), 2);
        assert_eq!(select(6), 0);

        // Single archive always returns 0
        let num_archives = 1usize;
        let select = |attempt: u32| -> usize { (attempt as usize) % num_archives };
        assert_eq!(select(0), 0);
        assert_eq!(select(1), 0);
        assert_eq!(select(100), 0);
    }
}
