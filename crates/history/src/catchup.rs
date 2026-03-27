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

use crate::{
    archive::HistoryArchive,
    archive_state::HistoryArchiveState,
    catchup_range::{CatchupMode, CatchupRange},
    checkpoint,
    replay::ReplayConfig,
    verify, CatchupOutput, CatchupResult, HistoryError, Result,
};
use henyey_bucket::{Bucket, BucketList, BucketManager, HasNextState, HotArchiveBucketList};
use henyey_common::protocol::{protocol_version_starts_from, ProtocolVersion};
use henyey_common::{Hash256, NetworkId};
use henyey_db::Database;
use std::collections::HashMap;
use std::sync::Arc;

use henyey_ledger::{LedgerCloseData, LedgerManager, TransactionSetVariant};
use henyey_tx::TransactionFrame;
use stellar_xdr::curr::LedgerCloseMeta;
use stellar_xdr::curr::{
    ExtensionPoint, GeneralizedTransactionSet, Hash, LedgerCloseMetaExt, LedgerCloseMetaExtV1,
    LedgerCloseMetaV2, LedgerHeader, LedgerHeaderHistoryEntry, LedgerHeaderHistoryEntryExt,
    LedgerUpgrade, Limits, ReadXdr, ScpHistoryEntry, TransactionHistoryEntry,
    TransactionHistoryEntryExt, TransactionHistoryResultEntry, TransactionHistoryResultEntryExt,
    TransactionResultPair, TransactionResultSet, TransactionSet, TransactionSetV1, WriteXdr,
};
use tracing::{debug, info, warn};

/// Maximum number of concurrent bucket downloads, mirroring stellar-core's
/// `MAX_CONCURRENT_SUBPROCESSES`.
const MAX_CONCURRENT_DOWNLOADS: usize = 16;

/// Current protocol version used for merge restarts.
const CURRENT_PROTOCOL_VERSION: u32 = 25;

/// Maximum number of retry attempts for the download-and-replay pipeline.
///
/// Matches stellar-core's `BasicWork::RETRY_A_FEW` used by
/// `DownloadApplyTxsWork` (a `BatchWork` subclass). On each retry the
/// replay start is recalculated from the current LCL, so partial
/// progress is preserved (mirroring stellar-core's `resetIter()`).
const REPLAY_RETRY_COUNT: u32 = 5;

/// Read the current process RSS (Resident Set Size) in MB from `/proc/self/status`.
/// Returns `None` on non-Linux platforms or if the file can't be read.
fn rss_mb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            // Format is "VmRSS:    123456 kB"
            let kb: u64 = rest.trim().split_whitespace().next()?.parse().ok()?;
            return Some(kb / 1024);
        }
    }
    None
}

/// Run a future to completion from a synchronous context.
///
/// Handles three cases:
/// 1. Inside a multi-threaded tokio runtime → `block_in_place` + `block_on`
/// 2. Inside a single-threaded tokio runtime → spawn a helper thread
/// 3. No runtime → create a temporary single-threaded runtime
fn block_on_async<F, T>(future: F) -> std::result::Result<T, henyey_bucket::BucketError>
where
    F: std::future::Future<Output = std::result::Result<T, henyey_bucket::BucketError>>
        + Send
        + 'static,
    T: Send + 'static,
{
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        if matches!(
            handle.runtime_flavor(),
            tokio::runtime::RuntimeFlavor::MultiThread
        ) {
            tokio::task::block_in_place(|| handle.block_on(future))
        } else {
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| {
                        henyey_bucket::BucketError::NotFound(format!(
                            "failed to build runtime: {}",
                            e
                        ))
                    })?;
                rt.block_on(future)
            })
            .join()
            .map_err(|_| {
                henyey_bucket::BucketError::NotFound("bucket download thread panicked".to_string())
            })?
        }
    } else {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| {
                henyey_bucket::BucketError::NotFound(format!("failed to build runtime: {}", e))
            })?;
        rt.block_on(future)
    }
}

/// Download a bucket from archives, trying each archive in order.
async fn download_bucket_from_archives(
    archives: Vec<Arc<HistoryArchive>>,
    hash: Hash256,
) -> std::result::Result<Vec<u8>, henyey_bucket::BucketError> {
    for archive in &archives {
        match archive.get_bucket(&hash).await {
            Ok(data) => return Ok(data),
            Err(e) => {
                warn!("Failed to download bucket {} from archive: {}", hash, e);
                continue;
            }
        }
    }
    Err(henyey_bucket::BucketError::NotFound(format!(
        "Bucket {} not found in any archive",
        hash
    )))
}

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
/// println!("Caught up to ledger {}", output.result.ledger_seq);
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
    archives: Vec<Arc<HistoryArchive>>,

    /// Manager for bucket file storage and retrieval.
    bucket_manager: Arc<BucketManager>,

    /// Database for persisting ledger state and history.
    db: Arc<Database>,

    /// Current progress information for status reporting.
    progress: CatchupProgress,

    /// Configuration for the replay phase.
    replay_config: ReplayConfig,

    /// Configured network passphrase for HAS validation.
    ///
    /// When set, the HAS `networkPassphrase` field is validated against this
    /// value during catchup (per stellar-core §8.2 step 2). If `None`,
    /// passphrase validation is skipped.
    network_passphrase: Option<String>,

    /// Optional callback for streaming metadata from replayed ledgers.
    ///
    /// Called once per ledger during the replay phase with the `LedgerCloseMeta`
    /// produced by `close_ledger`. Used by stellar-rpc's bounded replay mode
    /// (`catchup --metadata-output-stream fd:3`).
    meta_callback: Option<MetaCallback>,
    /// When true, synthetic bucket-apply meta uses `LedgerCloseMetaExtV1`
    /// (matching the live mode setting `EMIT_LEDGER_CLOSE_META_EXT_V1`).
    emit_meta_ext_v1: bool,
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

    /// Emit `LedgerCloseMeta` to both the streaming callback and SQLite.
    ///
    /// During catchup replay, meta must be persisted to the `ledger_close_meta`
    /// table so that consumers querying SQLite (e.g., RPC `getTransactions`,
    /// `getLedgers`) see entries for the replayed range. The streaming callback
    /// (if configured) writes the meta to the fd:3 pipe for captive core
    /// consumers like stellar-rpc and horizon.
    fn emit_meta(&self, ledger_seq: u32, meta: LedgerCloseMeta) {
        // Persist to SQLite first (to_xdr borrows &self on meta, no clone needed).
        match meta.to_xdr(Limits::none()) {
            Ok(meta_xdr) => {
                if let Err(err) = self.db.store_ledger_close_meta(ledger_seq, &meta_xdr) {
                    warn!(
                        error = %err,
                        ledger_seq,
                        "Failed to persist LedgerCloseMeta during catchup"
                    );
                }
            }
            Err(err) => {
                warn!(
                    error = %err,
                    ledger_seq,
                    "Failed to serialize LedgerCloseMeta during catchup"
                );
            }
        }

        // Stream to external consumers (fd:3 pipe).
        if let Some(ref callback) = self.meta_callback {
            callback(meta);
        }
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
    /// A `CatchupOutput` containing the bucket list, header, and summary information.
    pub async fn catchup_to_ledger(
        &mut self,
        target: u32,
        ledger_manager: &LedgerManager,
    ) -> Result<CatchupOutput> {
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

        // Step 4: Apply buckets to build initial state
        self.update_progress(
            CatchupStatus::ApplyingBuckets,
            3,
            "Applying buckets to build initial state",
        );
        let (mut bucket_list, mut hot_archive_bucket_list, live_next_states, hot_next_states) =
            self.apply_buckets(&has, &buckets).await?;

        // Restart merges.
        self.restart_merges(
            &mut bucket_list,
            &mut hot_archive_bucket_list,
            checkpoint_seq,
            &live_next_states,
            &hot_next_states,
        )
        .await?;

        self.persist_bucket_list_snapshot(checkpoint_seq, &bucket_list)?;

        // Initialize the LedgerManager at the checkpoint state.
        let (checkpoint_header, checkpoint_hash) =
            self.download_checkpoint_header(checkpoint_seq).await?;

        if ledger_manager.is_initialized() {
            ledger_manager.reset();
        }
        ledger_manager
            .initialize(
                bucket_list,
                hot_archive_bucket_list,
                checkpoint_header.clone(),
                checkpoint_hash,
            )
            .map_err(|e| {
                HistoryError::CatchupFailed(format!("Failed to initialize ledger manager: {}", e))
            })?;

        // Steps 5-7: Download, verify, and replay ledgers with retry.
        // Matches stellar-core's DownloadApplyTxsWork(RETRY_A_FEW).
        let (final_header, final_hash, ledgers_applied) = if checkpoint_seq >= target {
            // No replay needed — target is exactly at checkpoint.
            self.persist_header_only(&checkpoint_header)?;

            // Emit synthetic LedgerCloseMeta for the bucket-applied ledger.
            // Captive core consumers (stellar-rpc, horizon) need at least one
            // frame on fd:3 to know core is initialized.
            let meta =
                build_bucket_apply_meta(&checkpoint_header, checkpoint_hash, self.emit_meta_ext_v1);
            info!(
                "Emitting synthetic LedgerCloseMeta for bucket-applied ledger {}",
                checkpoint_header.ledger_seq
            );
            self.emit_meta(checkpoint_header.ledger_seq, meta);

            (checkpoint_header, checkpoint_hash, 0)
        } else {
            let (header, hash, applied) = self
                .download_verify_and_replay_with_retry(target, ledger_manager)
                .await?;
            (header, hash, applied)
        };

        // Complete!
        self.update_progress(CatchupStatus::Completed, 7, "Catchup completed");

        info!(
            "Catchup completed: ledger {}, hash {}",
            final_header.ledger_seq, final_hash
        );

        Ok(CatchupOutput {
            result: CatchupResult {
                ledger_seq: final_header.ledger_seq,
                ledger_hash: final_hash,
                ledgers_applied,
                buckets_downloaded: buckets_total,
            },
        })
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
    /// A `CatchupOutput` containing the bucket list, header, and summary information.
    pub async fn catchup_to_ledger_with_mode(
        &mut self,
        target: u32,
        mode: CatchupMode,
        lcl: u32,
        existing_state: Option<ExistingBucketState>,
        ledger_manager: &LedgerManager,
    ) -> Result<CatchupOutput> {
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

            // Apply buckets
            self.update_progress(
                CatchupStatus::ApplyingBuckets,
                3,
                "Applying buckets to build initial state",
            );
            let (mut bucket_list, mut hot_archive_bucket_list, live_next_states, hot_next_states) =
                self.apply_buckets(&has, &buckets).await?;

            // Restart merges.
            self.restart_merges(
                &mut bucket_list,
                &mut hot_archive_bucket_list,
                bucket_apply_at,
                &live_next_states,
                &hot_next_states,
            )
            .await?;

            self.persist_bucket_list_snapshot(bucket_apply_at, &bucket_list)?;

            // Initialize the LedgerManager at the checkpoint state.
            let (checkpoint_header, checkpoint_hash) =
                self.download_checkpoint_header(bucket_apply_at).await?;

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
                    HistoryError::CatchupFailed(format!(
                        "Failed to initialize ledger manager: {}",
                        e
                    ))
                })?;

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
        let replay_count = range.replay_count();

        let (final_header, final_hash, ledgers_applied) = if replay_count == 0 {
            // No replay needed - target is exactly at checkpoint
            info!(
                "Catching up to checkpoint {} (no ledgers to replay)",
                checkpoint_seq
            );
            let (header, hash) = self.download_checkpoint_header(checkpoint_seq).await?;
            self.persist_header_only(&header)?;

            // Emit synthetic LedgerCloseMeta for the bucket-applied ledger.
            // Captive core consumers (stellar-rpc, horizon) read metadata from
            // fd:3 and need at least one frame to know the core is initialized.
            // Without this, the Go SDK times out and kills the process.
            let meta = build_bucket_apply_meta(&header, hash, self.emit_meta_ext_v1);
            info!(
                "Emitting synthetic LedgerCloseMeta for bucket-applied ledger {}",
                header.ledger_seq
            );
            self.emit_meta(header.ledger_seq, meta);

            (header, hash, 0)
        } else {
            let (header, hash, applied) = self
                .download_verify_and_replay_with_retry(target, ledger_manager)
                .await?;
            (header, hash, applied)
        };

        // Complete!
        self.update_progress(CatchupStatus::Completed, 7, "Catchup completed");

        info!(
            "Catchup completed: ledger {}, hash {}, {} ledgers replayed",
            final_header.ledger_seq, final_hash, ledgers_applied
        );

        Ok(CatchupOutput {
            result: CatchupResult {
                ledger_seq: final_header.ledger_seq,
                ledger_hash: final_hash,
                ledgers_applied,
                buckets_downloaded: self.progress.buckets_total,
            },
        })
    }

    /// Catch up to a target ledger using pre-downloaded checkpoint data.
    pub async fn catchup_to_ledger_with_checkpoint_data(
        &mut self,
        target: u32,
        data: CheckpointData,
        ledger_manager: &LedgerManager,
    ) -> Result<CatchupOutput> {
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
        self.progress.buckets_total = bucket_hashes.len() as u32;
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

        // Step 5: Apply buckets to build initial state
        // Buckets are loaded lazily from disk — no in-memory bucket data needed.
        self.update_progress(
            CatchupStatus::ApplyingBuckets,
            3,
            "Applying buckets to build initial state",
        );
        let (mut bucket_list, mut hot_archive_bucket_list, live_next_states, hot_next_states) =
            self.apply_buckets(&data.has, &[]).await?;

        // Restart merges.
        self.restart_merges(
            &mut bucket_list,
            &mut hot_archive_bucket_list,
            checkpoint_seq,
            &live_next_states,
            &hot_next_states,
        )
        .await?;

        self.persist_bucket_list_snapshot(checkpoint_seq, &bucket_list)?;

        // Initialize the LedgerManager at the checkpoint state.
        let (checkpoint_header, checkpoint_hash) =
            checkpoint_header_from_headers(checkpoint_seq, &data.headers)?;

        if ledger_manager.is_initialized() {
            ledger_manager.reset();
        }
        ledger_manager
            .initialize(
                bucket_list,
                hot_archive_bucket_list,
                checkpoint_header.clone(),
                checkpoint_hash,
            )
            .map_err(|e| {
                HistoryError::CatchupFailed(format!("Failed to initialize ledger manager: {}", e))
            })?;

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
        // Steps 6-7: Download, verify, and replay ledgers with retry.
        // Matches stellar-core's DownloadApplyTxsWork(RETRY_A_FEW).
        let (final_header, final_hash, ledgers_applied) = if target == checkpoint_seq {
            self.persist_header_only(&checkpoint_header)?;

            // Emit synthetic LedgerCloseMeta for the bucket-applied ledger.
            // Captive core consumers (stellar-rpc, horizon) need at least one
            // frame on fd:3 to know core is initialized.
            let meta =
                build_bucket_apply_meta(&checkpoint_header, checkpoint_hash, self.emit_meta_ext_v1);
            info!(
                "Emitting synthetic LedgerCloseMeta for bucket-applied ledger {}",
                checkpoint_header.ledger_seq
            );
            self.emit_meta(checkpoint_header.ledger_seq, meta);

            (checkpoint_header, checkpoint_hash, 0)
        } else {
            let (header, hash, applied) = self
                .download_verify_and_replay_with_retry(target, ledger_manager)
                .await?;
            (header, hash, applied)
        };

        self.update_progress(CatchupStatus::Completed, 7, "Catchup completed");

        Ok(CatchupOutput {
            result: CatchupResult {
                ledger_seq: final_header.ledger_seq,
                ledger_hash: final_hash,
                ledgers_applied,
                buckets_downloaded: bucket_hashes.len() as u32,
            },
        })
    }

    /// Restart pending bucket merges from the HAS (without cache scanning).
    ///
    /// Cache initialization is handled by `LedgerManager::initialize()`.
    async fn restart_merges(
        &self,
        bucket_list: &mut BucketList,
        hot_archive_bucket_list: &mut HotArchiveBucketList,
        checkpoint_seq: u32,
        live_next_states: &[HasNextState],
        hot_next_states: &[HasNextState],
    ) -> Result<()> {
        let protocol_version = CURRENT_PROTOCOL_VERSION;

        // Run live bucket list merge restarts in parallel (all levels concurrently).
        let bucket_dir = self.bucket_manager.bucket_dir().to_path_buf();
        let load_bucket_for_merge = load_disk_backed_bucket_closure(bucket_dir.clone());

        bucket_list
            .restart_merges_from_has(
                checkpoint_seq,
                protocol_version,
                live_next_states,
                load_bucket_for_merge,
                true,
            )
            .await
            .map_err(|e| {
                HistoryError::CatchupFailed(format!("Failed to restart bucket merges: {}", e))
            })?;

        // Hot archive merges are small — run synchronously.
        {
            let load_hot_bucket_for_merge = load_disk_backed_hot_archive_bucket_closure(bucket_dir);
            hot_archive_bucket_list
                .restart_merges_from_has(
                    checkpoint_seq,
                    protocol_version,
                    hot_next_states,
                    load_hot_bucket_for_merge,
                    true,
                )
                .map_err(|e| {
                    HistoryError::CatchupFailed(format!(
                        "Failed to restart hot archive merges: {}",
                        e
                    ))
                })?;
        }

        info!(
            "Bucket list hash after restart_merges_from_has: {}",
            bucket_list.hash()
        );

        Ok(())
    }

    /// Select an archive for a download attempt, rotating through available archives.
    ///
    /// Uses `attempt % archives.len()` to distribute retries across different archives,
    /// providing failover when one archive is down or slow. This matches stellar-core's
    /// archive selection strategy for retry resilience.
    fn select_archive(&self, attempt: u32) -> &Arc<HistoryArchive> {
        let index = (attempt as usize) % self.archives.len();
        &self.archives[index]
    }

    /// Update the progress status.
    fn update_progress(&mut self, status: CatchupStatus, step: u32, message: &str) {
        self.progress.status = status;
        self.progress.current_step = step;
        self.progress.message = message.to_string();
        debug!(
            "Catchup progress: step {}/{} - {}",
            step, self.progress.total_steps, message
        );
    }

    /// Download the History Archive State for a checkpoint.
    ///
    /// Uses archive rotation: each attempt tries a different archive, cycling
    /// through them to provide failover when one archive is unavailable.
    async fn download_has(&self, checkpoint_seq: u32) -> Result<HistoryArchiveState> {
        let num_archives = self.archives.len() as u32;
        for attempt in 0..num_archives {
            let archive = self.select_archive(attempt);
            match archive.get_checkpoint_has(checkpoint_seq).await {
                Ok(has) => return Ok(has),
                Err(e) => {
                    warn!(
                        "Failed to download HAS from archive {}: {}",
                        archive.base_url(),
                        e
                    );
                    continue;
                }
            }
        }

        Err(HistoryError::CatchupFailed(format!(
            "failed to download HAS for checkpoint {} from any archive",
            checkpoint_seq
        )))
    }

    async fn download_scp_history(&self, checkpoint_seq: u32) -> Result<Vec<ScpHistoryEntry>> {
        for archive in &self.archives {
            match archive.get_scp_history(checkpoint_seq).await {
                Ok(entries) => return Ok(entries),
                Err(HistoryError::NotFound(_)) => {
                    debug!(
                        archive = %archive.base_url(),
                        checkpoint = checkpoint_seq,
                        "SCP history not found"
                    );
                }
                Err(e) => {
                    warn!(
                        archive = %archive.base_url(),
                        checkpoint = checkpoint_seq,
                        error = %e,
                        "Failed to download SCP history"
                    );
                }
            }
        }

        Ok(Vec::new())
    }

    /// Download all buckets referenced in the HAS to disk in parallel.
    ///
    /// This pre-downloads buckets to disk (not memory) so apply_buckets can
    /// load them quickly. Uses parallel downloads for speed while keeping
    /// memory usage low by saving directly to disk.
    async fn download_buckets(&mut self, hashes: &[Hash256]) -> Result<Vec<(Hash256, Vec<u8>)>> {
        use futures::stream::{self, StreamExt};

        let bucket_dir = self.bucket_manager.bucket_dir().to_path_buf();
        let empty_bucket_hash = Hash256::hash(&[]);

        // Filter out zero/empty hashes and already-downloaded buckets
        let to_download: Vec<_> = hashes
            .iter()
            .filter(|hash| {
                if hash.is_zero() || **hash == empty_bucket_hash {
                    return false;
                }
                let bucket_path = bucket_dir.join(format!("{}.bucket.xdr", hash.to_hex()));
                !bucket_path.exists()
            })
            .cloned()
            .collect();

        self.progress.buckets_total = hashes.len() as u32;

        if to_download.is_empty() {
            info!("All {} buckets already cached on disk", hashes.len());
            return Ok(Vec::new());
        }

        info!(
            "Pre-downloading {} buckets to disk ({} already cached) with {} parallel downloads",
            to_download.len(),
            hashes.len() - to_download.len(),
            MAX_CONCURRENT_DOWNLOADS
        );

        let archives = self.archives.clone();
        let bucket_dir = bucket_dir.clone();
        let total_to_download = to_download.len();
        let downloaded = std::sync::atomic::AtomicU32::new(0);

        // Download buckets in parallel, saving directly to disk
        let results: Vec<Result<()>> = stream::iter(to_download.into_iter())
            .map(|hash| {
                let archives = archives.clone();
                let bucket_dir = bucket_dir.clone();
                let downloaded = &downloaded;

                async move {
                    let bucket_path = bucket_dir.join(format!("{}.bucket.xdr", hash.to_hex()));

                    // Try each archive until one succeeds
                    for archive in &archives {
                        match archive.get_bucket(&hash).await {
                            Ok(data) => {
                                // Reject oversized buckets
                                if data.len() as u64
                                    > crate::archive_state::MAX_HISTORY_ARCHIVE_BUCKET_SIZE
                                {
                                    warn!(
                                        "Bucket {} exceeds MAX_HISTORY_ARCHIVE_BUCKET_SIZE ({} > {})",
                                        hash,
                                        data.len(),
                                        crate::archive_state::MAX_HISTORY_ARCHIVE_BUCKET_SIZE
                                    );
                                    continue;
                                }
                                // Save to disk
                                if let Err(e) = std::fs::write(&bucket_path, &data) {
                                    warn!("Failed to save bucket {} to disk: {}", hash, e);
                                    continue;
                                }
                                let count = downloaded
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                                    + 1;
                                if count % 5 == 0 || count == total_to_download as u32 {
                                    info!("Downloaded {}/{} buckets", count, total_to_download);
                                }
                                debug!("Pre-downloaded bucket {} ({} bytes)", hash, data.len());
                                return Ok(());
                            }
                            Err(e) => {
                                debug!(
                                    "Failed to download bucket {} from {}: {}",
                                    hash,
                                    archive.base_url(),
                                    e
                                );
                                continue;
                            }
                        }
                    }

                    Err(HistoryError::BucketNotFound(hash))
                }
            })
            .buffer_unordered(MAX_CONCURRENT_DOWNLOADS)
            .collect()
            .await;

        // Check for any failures
        for result in results {
            result?;
        }

        self.progress.buckets_downloaded = hashes.len() as u32;
        info!("Pre-downloaded all {} buckets to disk", total_to_download);

        // Return empty - buckets are on disk, not in memory
        Ok(Vec::new())
    }

    /// Apply downloaded buckets to build the initial bucket list state.
    /// Returns (live_bucket_list, hot_archive_bucket_list).
    ///
    /// This method uses disk-backed bucket storage to handle mainnet's large buckets
    /// efficiently. Instead of loading all entries into memory, each bucket is:
    /// 1. Downloaded and saved to disk
    /// 2. Indexed with a compact key-to-offset mapping
    /// 3. Entries are loaded on-demand when accessed
    ///
    /// This reduces memory usage from O(entries) to O(unique_keys) for the index.
    /// Return type for apply_buckets, including next_states for restart_merges_from_has
    async fn apply_buckets(
        &self,
        has: &HistoryArchiveState,
        buckets: &[(Hash256, Vec<u8>)],
    ) -> Result<(
        BucketList,
        HotArchiveBucketList,
        Vec<HasNextState>,
        Vec<HasNextState>,
    )> {
        use std::sync::Mutex;

        if let Some(mb) = rss_mb() {
            info!("apply_buckets START — RSS {} MB", mb);
        }
        info!(
            "Applying buckets to build state at ledger {} (disk-backed mode)",
            has.current_ledger
        );

        // Get bucket storage directory from the bucket manager
        let bucket_dir = self.bucket_manager.bucket_dir();

        // Cache for buckets we've already loaded (to avoid re-downloading).
        let bucket_cache: Mutex<HashMap<Hash256, Bucket>> = Mutex::new(HashMap::new());
        let preloaded_buckets: Mutex<HashMap<Hash256, Vec<u8>>> =
            Mutex::new(buckets.iter().cloned().collect());

        // Clone archives and bucket_dir for use in closure
        let archives = self.archives.clone();
        let bucket_dir = bucket_dir.to_path_buf();

        let empty_bucket_hash = Hash256::hash(&[]);

        // Helper to load a bucket - downloads on-demand, saves to disk, and caches
        let load_bucket = |hash: &Hash256| -> henyey_bucket::Result<Bucket> {
            // Zero hash means empty bucket
            if hash.is_zero() {
                return Ok(Bucket::empty());
            }
            if *hash == empty_bucket_hash {
                return Ok(Bucket::empty());
            }

            // Check cache first
            {
                let cache = bucket_cache.lock().unwrap();
                if let Some(bucket) = cache.get(hash) {
                    return Ok(bucket.clone());
                }
            }

            // Construct path for this bucket
            let bucket_path = bucket_dir.join(format!("{}.bucket.xdr", hash.to_hex()));

            // Check if bucket already exists on disk as an XDR file.
            // Build the index eagerly so it's ready for lookups during live
            // ledger closing — deferring index construction to the first get()
            // would cause multi-second stalls when closing the first few ledgers.
            if bucket_path.exists() {
                debug!("Loading existing bucket {} from disk", hash);
                let bucket = Bucket::from_xdr_file_disk_backed(&bucket_path)?;
                let mut cache = bucket_cache.lock().unwrap();
                cache.insert(*hash, bucket.clone());
                return Ok(bucket);
            }

            // Use preloaded bucket data if available, otherwise download.
            let xdr_data = if let Some(data) = {
                let mut preloaded = preloaded_buckets.lock().unwrap();
                preloaded.remove(hash)
            } {
                data
            } else {
                // Download the bucket (blocking - we're in a sync context)
                block_on_async(download_bucket_from_archives(archives.clone(), *hash))?
            };

            info!(
                "Downloaded bucket {}: {} bytes, saving to disk",
                hash,
                xdr_data.len()
            );

            // Save XDR data to disk first, then build the disk-backed bucket by
            // streaming through the file. This avoids holding the full file in memory
            // while also building the index — critical for multi-GB buckets on mainnet.
            std::fs::write(&bucket_path, &xdr_data).map_err(|e| {
                henyey_bucket::BucketError::NotFound(format!(
                    "failed to write bucket to disk: {}",
                    e
                ))
            })?;
            // Drop the in-memory XDR data before building the index to free memory
            drop(xdr_data);

            let bucket = Bucket::from_xdr_file_disk_backed(&bucket_path)?;

            // Verify hash matches
            if bucket.hash() != *hash {
                // Clean up the bad file
                let _ = std::fs::remove_file(&bucket_path);
                return Err(henyey_bucket::BucketError::HashMismatch {
                    expected: hash.to_hex(),
                    actual: bucket.hash().to_hex(),
                });
            }

            info!(
                "Created disk-backed bucket {} with {} entries",
                hash,
                bucket.len()
            );

            // Cache the bucket (it might be referenced multiple times in the bucket list)
            {
                let mut cache = bucket_cache.lock().unwrap();
                cache.insert(*hash, bucket.clone());
            }

            Ok(bucket)
        };

        // Build live bucket list hashes as (curr, snap) pairs with next states
        // This is required for proper FutureBucket restoration
        let live_hash_pairs = has.bucket_hash_pairs();
        let live_next_states: Vec<HasNextState> = has
            .live_next_states()
            .into_iter()
            .map(HasNextState::from)
            .collect();

        for (level_idx, (curr, snap)) in live_hash_pairs.iter().enumerate() {
            info!(
                "HAS level {} hashes: curr={}, snap={}",
                level_idx, curr, snap
            );
        }

        // Restore the live bucket list with FutureBucket states
        let mut bucket_list =
            BucketList::restore_from_has(&live_hash_pairs, &live_next_states, load_bucket)
                .map_err(|e| {
                    HistoryError::CatchupFailed(format!(
                        "Failed to restore live bucket list: {}",
                        e
                    ))
                })?;
        bucket_list.set_bucket_dir(bucket_dir.to_path_buf());

        // Log the restored bucket list hash
        info!("Live bucket list restored hash: {}", bucket_list.hash());
        info!(
            "Live bucket list restored: {} total entries",
            bucket_list.stats().total_entries
        );
        if let Some(mb) = rss_mb() {
            info!(
                "apply_buckets AFTER live bucket list restore — RSS {} MB",
                mb
            );
        }

        // Build hot archive next states (even if no hot archive buckets, for return value)
        let hot_next_states: Vec<HasNextState> = has
            .hot_archive_next_states()
            .unwrap_or_default()
            .into_iter()
            .map(HasNextState::from)
            .collect();

        // Build hot archive bucket list if present (protocol 23+)
        // Hot archive uses HotArchiveBucketEntry (Metaentry/Archived/Live), not BucketEntry
        let hot_archive_bucket_list = if has.has_hot_archive_buckets() {
            use henyey_bucket::HotArchiveBucket;

            // Build hot archive bucket list hashes as (curr, snap) pairs
            let hot_hash_pairs = has.hot_archive_bucket_hash_pairs().unwrap_or_default();

            // Log the HAS hashes before restoration
            for (level_idx, (curr, snap)) in hot_hash_pairs.iter().enumerate().take(5) {
                info!(
                    "Hot archive HAS level {} hashes: curr={}, snap={}",
                    level_idx,
                    curr.to_hex(),
                    snap.to_hex()
                );
            }

            // Create a loader for HotArchiveBucket (different from live Bucket)
            // Hot archive buckets contain HotArchiveBucketEntry, not BucketEntry
            let bucket_dir_clone = bucket_dir.clone();
            let archives_clone = archives.clone();

            // Cache for hot archive buckets (same hash can appear at multiple levels)
            let hot_archive_bucket_cache: Mutex<HashMap<Hash256, HotArchiveBucket>> =
                Mutex::new(HashMap::new());

            let load_hot_archive_bucket =
                |hash: &Hash256| -> henyey_bucket::Result<HotArchiveBucket> {
                    // Zero hash means empty bucket
                    if hash.is_zero() {
                        return Ok(HotArchiveBucket::empty());
                    }

                    // Check cache first (same hash can appear at multiple levels)
                    {
                        let cache = hot_archive_bucket_cache.lock().unwrap();
                        if let Some(bucket) = cache.get(hash) {
                            return Ok(bucket.clone());
                        }
                    }

                    // Check if we have the XDR data in the pre-downloaded cache
                    let bucket_path =
                        bucket_dir_clone.join(format!("{}.bucket.xdr", hash.to_hex()));

                    let xdr_data: Option<Vec<u8>> = if let Some(data) = {
                        let mut preloaded = preloaded_buckets.lock().unwrap();
                        preloaded.remove(hash)
                    } {
                        // Save preloaded data to disk, then load via streaming
                        std::fs::write(&bucket_path, &data).map_err(|e| {
                            henyey_bucket::BucketError::NotFound(format!(
                                "failed to write hot archive bucket to disk: {}",
                                e
                            ))
                        })?;
                        None
                    } else if bucket_path.exists() {
                        // Already on disk, load via streaming
                        None
                    } else {
                        // Download if needed (shouldn't happen if download_buckets was called)
                        warn!(
                            "Hot archive bucket {} not found in cache, downloading",
                            hash
                        );
                        Some(block_on_async(download_bucket_from_archives(
                            archives_clone.clone(),
                            *hash,
                        ))?)
                    };

                    // If we downloaded data, save it to disk first
                    if let Some(downloaded_data) = xdr_data {
                        std::fs::write(&bucket_path, &downloaded_data).map_err(|e| {
                            henyey_bucket::BucketError::NotFound(format!(
                                "failed to write hot archive bucket to disk: {}",
                                e
                            ))
                        })?;
                    }

                    // Load hot archive bucket from disk eagerly — builds the index
                    // immediately so it's ready for lookups during live operation.
                    let bucket = HotArchiveBucket::from_xdr_file_disk_backed(&bucket_path)?;

                    // Cache for reuse (same hash can appear at multiple levels)
                    {
                        let mut cache = hot_archive_bucket_cache.lock().unwrap();
                        cache.insert(*hash, bucket.clone());
                    }

                    Ok(bucket)
                };

            let hot_bucket_list = HotArchiveBucketList::restore_from_has(
                &hot_hash_pairs,
                &hot_next_states,
                load_hot_archive_bucket,
            )
            .map_err(|e| {
                HistoryError::CatchupFailed(format!(
                    "Failed to restore hot archive bucket list: {}",
                    e
                ))
            })?;

            info!(
                "Hot archive bucket list restored: {} total entries",
                hot_bucket_list.stats().total_entries
            );
            if let Some(mb) = rss_mb() {
                info!("apply_buckets AFTER hot archive restore — RSS {} MB", mb);
            }

            // Log the restored bucket list state
            for (level_idx, level) in hot_bucket_list.levels().iter().enumerate().take(5) {
                info!(
                    "Hot archive restored level {}: curr={}, snap={}",
                    level_idx,
                    level.curr.hash().to_hex(),
                    level.snap.hash().to_hex()
                );
            }

            hot_bucket_list
        } else {
            HotArchiveBucketList::new()
        };

        if let Some(mb) = rss_mb() {
            info!("apply_buckets END — RSS {} MB", mb);
        }

        Ok((
            bucket_list,
            hot_archive_bucket_list,
            live_next_states,
            hot_next_states,
        ))
    }

    /// Download ledger headers, transactions, and results for a range.
    async fn download_ledger_data(
        &mut self,
        from_checkpoint: u32,
        to_ledger: u32,
    ) -> Result<Vec<LedgerData>> {
        let mut data = Vec::new();
        let mut checkpoint_cache: HashMap<u32, CheckpointLedgerData> = HashMap::new();

        // We need to download data for ledgers (from_checkpoint+1) to to_ledger
        // The checkpoint ledger's state is already in the bucket list
        let start = from_checkpoint + 1;

        if start > to_ledger {
            // No ledgers to replay, we're at the checkpoint
            return Ok(data);
        }

        for seq in start..=to_ledger {
            self.progress.current_ledger = seq;
            let checkpoint = checkpoint::checkpoint_containing(seq);

            if let std::collections::hash_map::Entry::Vacant(e) = checkpoint_cache.entry(checkpoint)
            {
                let downloaded = self.download_checkpoint_ledger_data(checkpoint).await?;
                e.insert(downloaded);
            }

            let cache = checkpoint_cache.get(&checkpoint).ok_or_else(|| {
                HistoryError::CatchupFailed(format!("missing checkpoint cache for {}", checkpoint))
            })?;

            let header = cache
                .headers
                .iter()
                .find(|h| h.header.ledger_seq == seq)
                .ok_or_else(|| {
                    HistoryError::CatchupFailed(format!(
                        "ledger {} not found in checkpoint headers",
                        seq
                    ))
                })?
                .header
                .clone();

            let tx_history_entry = cache
                .tx_entries
                .iter()
                .find(|entry| entry.ledger_seq == seq)
                .cloned();
            let tx_set = tx_history_entry
                .as_ref()
                .map(|entry| match &entry.ext {
                    TransactionHistoryEntryExt::V0 => {
                        TransactionSetVariant::Classic(entry.tx_set.clone())
                    }
                    TransactionHistoryEntryExt::V1(set) => {
                        TransactionSetVariant::Generalized(set.clone())
                    }
                })
                .unwrap_or_else(|| {
                    // For protocol 20+, use GeneralizedTransactionSet format
                    // For earlier protocols, use Classic TransactionSet
                    if protocol_version_starts_from(header.ledger_version, ProtocolVersion::V20) {
                        // Create empty GeneralizedTransactionSet with proper phases
                        // Phase 0: empty classic phase (V0 with no components)
                        // Phase 1: empty soroban phase (V1 with no stages)
                        use stellar_xdr::curr::{ParallelTxsComponent, TransactionPhase, VecM};

                        // Empty classic phase (no components)
                        let classic_phase = TransactionPhase::V0(VecM::default());
                        // Empty soroban phase (no execution stages)
                        let soroban_phase = TransactionPhase::V1(ParallelTxsComponent {
                            base_fee: None,
                            execution_stages: VecM::default(),
                        });

                        TransactionSetVariant::Generalized(GeneralizedTransactionSet::V1(
                            TransactionSetV1 {
                                previous_ledger_hash: header.previous_ledger_hash.clone(),
                                phases: vec![classic_phase, soroban_phase]
                                    .try_into()
                                    .unwrap_or_default(),
                            },
                        ))
                    } else {
                        TransactionSetVariant::Classic(TransactionSet {
                            previous_ledger_hash: header.previous_ledger_hash.clone(),
                            txs: Default::default(),
                        })
                    }
                });

            let tx_result_entry = cache
                .result_entries
                .iter()
                .find(|entry| entry.ledger_seq == seq)
                .cloned();
            let tx_results = tx_result_entry
                .as_ref()
                .map(|entry| entry.tx_result_set.results.iter().cloned().collect())
                .unwrap_or_else(Vec::new);

            data.push(LedgerData {
                header,
                tx_set,
                tx_results,
                tx_history_entry,
                tx_result_entry,
            });
        }

        Ok(data)
    }

    /// Download ledger headers, transactions, and results for a checkpoint.
    async fn download_checkpoint_ledger_data(
        &self,
        checkpoint: u32,
    ) -> Result<CheckpointLedgerData> {
        // Try each archive until one succeeds
        for archive in &self.archives {
            match self.try_download_checkpoint(archive, checkpoint).await {
                Ok(data) => return Ok(data),
                Err(e) => {
                    warn!(
                        "Failed to download checkpoint {} from archive {}: {}",
                        checkpoint,
                        archive.base_url(),
                        e
                    );
                    continue;
                }
            }
        }

        Err(HistoryError::CatchupFailed(format!(
            "failed to download checkpoint {} from any archive",
            checkpoint
        )))
    }

    /// Try to download checkpoint data from a specific archive.
    async fn try_download_checkpoint(
        &self,
        archive: &HistoryArchive,
        checkpoint: u32,
    ) -> Result<CheckpointLedgerData> {
        let headers = archive.get_ledger_headers(checkpoint).await?;
        let tx_entries = archive.get_transactions(checkpoint).await?;
        let result_entries = archive.get_results(checkpoint).await?;
        Ok(CheckpointLedgerData {
            headers,
            tx_entries,
            result_entries,
        })
    }

    /// Verify the downloaded ledger data.
    fn verify_downloaded_data(
        &self,
        ledger_data: &[LedgerData],
        anchors: &verify::ChainTrustAnchors,
    ) -> Result<()> {
        if ledger_data.is_empty() {
            return Ok(());
        }

        // Extract headers for chain verification
        let headers: Vec<_> = ledger_data.iter().map(|d| d.header.clone()).collect();

        // Skip header chain and trust anchor verification when verify_results
        // is false (maps from CatchupOptions::verify_headers). This allows
        // synthetic tests to bypass chain integrity checks.
        if self.replay_config.verify_results {
            verify::verify_header_chain(&headers)?;

            // Verify trust anchors (spec §9.2–§9.5): ensures the chain connects
            // to externally trusted state (checkpoint header hash, LCL hash).
            verify::verify_chain_anchors(&headers, anchors)?;
        }

        // Verify transaction sets match header hashes
        for data in ledger_data {
            if let Some(entry) = data.tx_history_entry.as_ref() {
                let tx_set = match &entry.ext {
                    TransactionHistoryEntryExt::V0 => {
                        TransactionSetVariant::Classic(entry.tx_set.clone())
                    }
                    TransactionHistoryEntryExt::V1(set) => {
                        TransactionSetVariant::Generalized(set.clone())
                    }
                };
                if let Err(e) = verify::verify_tx_set(&data.header, &tx_set) {
                    warn!(
                        "Transaction set verification failed for ledger {}: {}",
                        data.header.ledger_seq, e
                    );
                    // Continue - tx sets may be empty for some ledgers
                }
            }
            if let Some(entry) = data.tx_result_entry.as_ref() {
                if let Ok(xdr) = entry
                    .tx_result_set
                    .to_xdr(stellar_xdr::curr::Limits::none())
                {
                    if let Err(e) = verify::verify_tx_result_set(&data.header, &xdr) {
                        warn!(
                            "Transaction result set verification failed for ledger {}: {}",
                            data.header.ledger_seq, e
                        );
                    }
                }
            }
        }

        info!("Verified header chain for {} ledgers", headers.len());
        Ok(())
    }

    fn persist_ledger_history(
        &self,
        ledger_data: &[LedgerData],
        network_id: &NetworkId,
    ) -> Result<()> {
        if ledger_data.is_empty() {
            return Ok(());
        }

        self.db
            .transaction(|conn| {
                use henyey_db::error::DbError;
                use henyey_db::queries::{HistoryQueries, LedgerQueries};

                for data in ledger_data {
                    let header_xdr = data.header.to_xdr(stellar_xdr::curr::Limits::none())?;
                    conn.store_ledger_header(&data.header, &header_xdr)?;

                    let tx_history_entry =
                        data.tx_history_entry
                            .clone()
                            .unwrap_or_else(|| match &data.tx_set {
                                TransactionSetVariant::Classic(set) => TransactionHistoryEntry {
                                    ledger_seq: data.header.ledger_seq,
                                    tx_set: set.clone(),
                                    ext: TransactionHistoryEntryExt::V0,
                                },
                                TransactionSetVariant::Generalized(set) => {
                                    let stellar_xdr::curr::GeneralizedTransactionSet::V1(set_v1) =
                                        set;
                                    TransactionHistoryEntry {
                                        ledger_seq: data.header.ledger_seq,
                                        tx_set: TransactionSet {
                                            previous_ledger_hash: set_v1
                                                .previous_ledger_hash
                                                .clone(),
                                            txs: Default::default(),
                                        },
                                        ext: TransactionHistoryEntryExt::V1(set.clone()),
                                    }
                                }
                            });
                    conn.store_tx_history_entry(data.header.ledger_seq, &tx_history_entry)?;

                    let tx_result_entry = data.tx_result_entry.clone().unwrap_or_else(|| {
                        let results = data.tx_results.clone().try_into().unwrap_or_default();
                        TransactionHistoryResultEntry {
                            ledger_seq: data.header.ledger_seq,
                            tx_result_set: TransactionResultSet { results },
                            ext: TransactionHistoryResultEntryExt::default(),
                        }
                    });
                    conn.store_tx_result_entry(data.header.ledger_seq, &tx_result_entry)?;

                    let tx_results: Vec<TransactionResultPair> = tx_result_entry
                        .tx_result_set
                        .results
                        .iter()
                        .cloned()
                        .collect();
                    let transactions = data
                        .tx_set
                        .transactions_with_base_fee()
                        .into_iter()
                        .map(|(tx, _)| tx)
                        .collect::<Vec<_>>();
                    let tx_count = transactions.len().min(tx_results.len());

                    for (idx, tx) in transactions.iter().take(tx_count).enumerate() {
                        let tx_result = &tx_results[idx];

                        let frame = TransactionFrame::with_network(tx.clone(), *network_id);
                        let tx_hash = frame
                            .hash(network_id)
                            .map_err(|e| DbError::Integrity(e.to_string()))?;
                        let tx_id = tx_hash.to_hex();

                        let tx_body = tx.to_xdr(stellar_xdr::curr::Limits::none())?;
                        let tx_result_xdr = tx_result.to_xdr(stellar_xdr::curr::Limits::none())?;

                        // Compute status from result code
                        let status = {
                            use stellar_xdr::curr::TransactionResultCode;
                            let code = tx_result.result.result.discriminant();
                            if code == TransactionResultCode::TxSuccess
                                || code == TransactionResultCode::TxFeeBumpInnerSuccess
                            {
                                henyey_db::TX_STATUS_SUCCESS
                            } else {
                                henyey_db::TX_STATUS_FAILED
                            }
                        };

                        conn.store_transaction(&henyey_db::StoreTxParams {
                            ledger_seq: data.header.ledger_seq,
                            tx_index: idx as u32,
                            tx_id: &tx_id,
                            body: &tx_body,
                            result: &tx_result_xdr,
                            meta: None,
                            status,
                        })?;
                    }
                }

                Ok(())
            })
            .map_err(|err| {
                HistoryError::CatchupFailed(format!("failed to persist history: {}", err))
            })?;

        Ok(())
    }

    fn persist_scp_history_entries(&self, entries: &[ScpHistoryEntry]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }

        self.db
            .transaction(|conn| {
                use henyey_db::queries::ScpQueries;

                for entry in entries {
                    let ScpHistoryEntry::V0(v0) = entry;
                    let ledger_seq = v0.ledger_messages.ledger_seq;
                    let envelopes: Vec<_> = v0.ledger_messages.messages.iter().cloned().collect();

                    conn.store_scp_history(ledger_seq, &envelopes)?;

                    for qset in v0.quorum_sets.iter() {
                        let hash = Hash256::hash_xdr(qset)?;
                        conn.store_scp_quorum_set(&hash, ledger_seq, qset)?;
                    }
                }

                Ok(())
            })
            .map_err(|err| {
                HistoryError::CatchupFailed(format!("failed to persist scp history: {}", err))
            })?;

        Ok(())
    }

    fn persist_bucket_list_snapshot(
        &self,
        ledger_seq: u32,
        bucket_list: &BucketList,
    ) -> Result<()> {
        let levels = bucket_list
            .levels()
            .iter()
            .map(|level| (level.curr.hash(), level.snap.hash()))
            .collect::<Vec<_>>();
        self.db
            .with_connection(|conn| {
                use henyey_db::queries::BucketListQueries;
                conn.store_bucket_list(ledger_seq, &levels)?;
                Ok(())
            })
            .map_err(|err| {
                HistoryError::CatchupFailed(format!(
                    "failed to persist bucket list for ledger {}: {}",
                    ledger_seq, err
                ))
            })?;
        Ok(())
    }

    fn persist_header_only(&self, header: &LedgerHeader) -> Result<()> {
        self.db
            .with_connection(|conn| {
                use henyey_db::queries::LedgerQueries;
                let header_xdr = header.to_xdr(stellar_xdr::curr::Limits::none())?;
                conn.store_ledger_header(header, &header_xdr)?;
                Ok(())
            })
            .map_err(|err| {
                HistoryError::CatchupFailed(format!("failed to persist header: {}", err))
            })?;
        Ok(())
    }

    /// Load the local History Archive State from the database, if available.
    ///
    /// This is used by `differing_bucket_hashes()` to compute the differential
    /// bucket download set — only downloading buckets that differ between the
    /// remote HAS and local state. Returns `None` if no local HAS exists (fresh node).
    fn load_local_has(&self) -> Option<HistoryArchiveState> {
        self.db
            .with_connection(|conn| {
                use henyey_db::queries::StateQueries;
                conn.get_state(henyey_db::schema::state_keys::HISTORY_ARCHIVE_STATE)
            })
            .ok()
            .flatten()
            .and_then(|json| HistoryArchiveState::from_json(&json).ok())
    }

    /// Compute the bucket hashes to download from a remote HAS.
    ///
    /// If a local HAS is available in the database, computes the differential
    /// set (only buckets we don't already have). Otherwise falls back to all
    /// unique bucket hashes from the remote HAS.
    ///
    /// This mirrors stellar-core's use of `differingBuckets(mLocalState)` in
    /// `CatchupWork.cpp`.
    fn compute_bucket_download_set(&self, remote_has: &HistoryArchiveState) -> Vec<Hash256> {
        match self.load_local_has() {
            Some(local_has) => {
                let hashes = remote_has.all_differing_bucket_hashes(&local_has);
                info!(
                    "Computed differential bucket set: {} buckets to download \
                     (remote has {} unique, local has {} unique)",
                    hashes.len(),
                    remote_has.unique_bucket_hashes().len(),
                    local_has.unique_bucket_hashes().len(),
                );
                hashes
            }
            None => {
                info!("No local HAS found, downloading all unique buckets");
                remote_has.unique_bucket_hashes()
            }
        }
    }

    /// Download the header for a specific ledger with its pre-computed hash.
    ///
    /// Returns the header and its hash as recorded in the history archive.
    /// The hash from the archive is authoritative - it's what the network used.
    async fn download_checkpoint_header(&self, ledger_seq: u32) -> Result<(LedgerHeader, Hash256)> {
        for archive in &self.archives {
            match archive.get_ledger_header_with_hash(ledger_seq).await {
                Ok((header, hash)) => {
                    debug!(
                        "Downloaded header for ledger {}: bucket_list_hash={}, ledger_seq={}, hash={}",
                        ledger_seq,
                        hex::encode(header.bucket_list_hash.0),
                        header.ledger_seq,
                        hash.to_hex()
                    );
                    return Ok((header, hash));
                }
                Err(e) => {
                    warn!(
                        "Failed to download header {} from archive {}: {}",
                        ledger_seq,
                        archive.base_url(),
                        e
                    );
                    continue;
                }
            }
        }

        Err(HistoryError::CatchupFailed(format!(
            "failed to download header for ledger {} from any archive",
            ledger_seq
        )))
    }

    /// Download, verify, and replay ledgers from `replay_start` to `target`
    /// with bounded retry on transient failures.
    ///
    /// Matches stellar-core's `DownloadApplyTxsWork` which uses
    /// `BatchWork(RETRY_A_FEW)`. On each retry, the replay start is
    /// recalculated from the ledger manager's current LCL (mirroring
    /// `DownloadApplyTxsWork::resetIter()` which resets
    /// `mCheckpointToQueue` to `checkpointContainingLedger(LCL + 1)`).
    ///
    /// Fatal errors (verification/integrity failures) are NOT retried —
    /// only transient errors (network, download, etc.) trigger a retry.
    async fn download_verify_and_replay_with_retry(
        &mut self,
        target: u32,
        ledger_manager: &LedgerManager,
    ) -> Result<(LedgerHeader, Hash256, u32)> {
        let mut last_error: Option<HistoryError> = None;

        for attempt in 0..=REPLAY_RETRY_COUNT {
            if attempt > 0 {
                let delay_ms = 200 * (1 << (attempt - 1).min(4));
                warn!(
                    attempt,
                    max_attempts = REPLAY_RETRY_COUNT + 1,
                    delay_ms,
                    error = %last_error.as_ref().unwrap(),
                    "Retrying download-and-replay pipeline"
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }

            // Recalculate replay start from current LCL (matching resetIter()).
            let current_lcl = ledger_manager.current_header().ledger_seq;
            let replay_first = current_lcl + 1;

            if replay_first > target {
                // Already past target — previous partial replay succeeded fully.
                let final_header = ledger_manager.current_header();
                let final_hash = ledger_manager.current_header_hash();
                let ledgers_applied = target.saturating_sub(current_lcl);
                return Ok((final_header, final_hash, ledgers_applied));
            }

            // Download from the checkpoint containing replay_first - 1 (the LCL).
            let download_from = current_lcl;

            match self
                .download_verify_and_replay_once(download_from, target, ledger_manager)
                .await
            {
                Ok(result) => return Ok(result),
                Err(e) => {
                    if e.is_fatal_catchup_failure() {
                        warn!(
                            attempt,
                            error = %e,
                            "Fatal error during replay — not retrying"
                        );
                        return Err(e);
                    }
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            HistoryError::CatchupFailed(
                "download-and-replay exhausted all retry attempts".to_string(),
            )
        }))
    }

    /// Single attempt at download + verify + replay from `download_from` to `target`.
    async fn download_verify_and_replay_once(
        &mut self,
        download_from: u32,
        target: u32,
        ledger_manager: &LedgerManager,
    ) -> Result<(LedgerHeader, Hash256, u32)> {
        // Download ledger data for replay
        self.update_progress(
            CatchupStatus::DownloadingLedgers,
            4,
            "Downloading ledger data",
        );
        let ledger_data = self.download_ledger_data(download_from, target).await?;

        // Verify the header chain
        self.update_progress(CatchupStatus::Verifying, 5, "Verifying header chain");
        let anchor_hash = ledger_manager.current_header_hash();
        let anchors = verify::ChainTrustAnchors {
            previous_ledger_hash: Some(anchor_hash),
            ..Default::default()
        };
        self.verify_downloaded_data(&ledger_data, &anchors)?;

        // Replay ledgers via close_ledger
        self.update_progress(CatchupStatus::Replaying, 6, "Replaying ledgers");
        self.replay_via_close_ledger(ledger_manager, &ledger_data)
            .await?;

        let network_id = NetworkId(ledger_manager.network_id().0);
        self.persist_ledger_history(&ledger_data, &network_id)?;

        let final_header = ledger_manager.current_header();
        let final_hash = ledger_manager.current_header_hash();
        let ledgers_applied = final_header.ledger_seq.saturating_sub(download_from);

        Ok((final_header, final_hash, ledgers_applied))
    }

    /// Replay ledgers by calling `LedgerManager::close_ledger()` for each one.
    ///
    /// This eliminates the duplicate replay implementation and uses the same
    /// code path as live ledger close, ensuring consistent behavior for:
    /// - Offer store maintenance (populated by `initialize()`, updated by `close_ledger()`)
    /// - Soroban state size tracking
    /// - Eviction scanning
    /// - Bucket list updates
    async fn replay_via_close_ledger(
        &mut self,
        ledger_manager: &LedgerManager,
        ledger_data: &[LedgerData],
    ) -> Result<()> {
        if ledger_data.is_empty() {
            return Err(HistoryError::CatchupFailed(
                "no ledger data to replay".to_string(),
            ));
        }

        let total = ledger_data.len();
        // CATCHUP_SPEC §5.6: Publish queue backpressure state.
        // When the queue exceeds PUBLISH_QUEUE_MAX_SIZE, replay pauses until
        // it drains to PUBLISH_QUEUE_UNBLOCK_APPLICATION.
        let mut pq_fell_behind = false;

        for (i, data) in ledger_data.iter().enumerate() {
            self.progress.current_ledger = data.header.ledger_seq;

            // Apply publish queue backpressure if enabled (offline catchup).
            if self.replay_config.wait_for_publish {
                self.wait_for_publish_queue(&mut pq_fell_behind).await?;
            }

            // Decode upgrades from the header's scp_value.upgrades
            let upgrades = decode_upgrades_from_header(&data.header);

            let close_data = LedgerCloseData::new(
                data.header.ledger_seq,
                data.tx_set.clone(),
                data.header.scp_value.close_time.0,
                ledger_manager.current_header_hash(),
            )
            .with_stellar_value_ext(data.header.scp_value.ext.clone())
            .with_upgrades(upgrades);

            let result = ledger_manager.close_ledger(close_data, None).map_err(|e| {
                HistoryError::CatchupFailed(format!(
                    "close_ledger failed at ledger {}: {}",
                    data.header.ledger_seq, e
                ))
            })?;

            // Verify computed header hash matches archive
            if self.replay_config.verify_bucket_list {
                let expected_hash =
                    henyey_ledger::compute_header_hash(&data.header).map_err(|e| {
                        HistoryError::CatchupFailed(format!(
                            "Failed to compute header hash for ledger {}: {}",
                            data.header.ledger_seq, e
                        ))
                    })?;
                if result.header_hash != expected_hash {
                    return Err(HistoryError::CatchupFailed(format!(
                        "Header hash mismatch at ledger {}: computed={}, expected={}",
                        data.header.ledger_seq,
                        result.header_hash.to_hex(),
                        expected_hash.to_hex()
                    )));
                }
            }

            // Emit metadata to SQLite and external consumers (e.g., stellar-rpc's
            // meta pipe in bounded replay mode: `catchup --metadata-output-stream fd:3`).
            if let Some(meta) = result.meta {
                self.emit_meta(data.header.ledger_seq, meta);
            }

            debug!(
                "Replayed ledger {}/{} via close_ledger: seq={}",
                i + 1,
                total,
                data.header.ledger_seq
            );
        }

        Ok(())
    }

    /// Wait until the publish queue is below the backpressure threshold.
    ///
    /// CATCHUP_SPEC §5.6 / §11.4: Uses hysteresis (high/low water marks) to
    /// avoid oscillation. Sets `pq_fell_behind` when queue > 16, clears it
    /// when queue <= 8.
    async fn wait_for_publish_queue(&self, pq_fell_behind: &mut bool) -> Result<()> {
        use crate::publish_queue::{
            PublishQueue, PUBLISH_QUEUE_MAX_SIZE, PUBLISH_QUEUE_UNBLOCK_APPLICATION,
        };

        let pq = PublishQueue::new(Arc::clone(&self.db));
        loop {
            let queue_len = pq.len()?;

            if queue_len <= PUBLISH_QUEUE_UNBLOCK_APPLICATION {
                *pq_fell_behind = false;
            }
            if queue_len > PUBLISH_QUEUE_MAX_SIZE {
                *pq_fell_behind = true;
            }

            if !*pq_fell_behind {
                return Ok(());
            }

            debug!(
                queue_len,
                "Publish queue backpressure: waiting for queue to drain"
            );
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
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

#[derive(Debug, Clone)]
struct CheckpointLedgerData {
    headers: Vec<LedgerHeaderHistoryEntry>,
    tx_entries: Vec<TransactionHistoryEntry>,
    result_entries: Vec<TransactionHistoryResultEntry>,
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

/// Decode ledger upgrades from a header's SCP value.
///
/// Each `upgrade` in `header.scp_value.upgrades` is an XDR-encoded `LedgerUpgrade`.
/// Invalid entries are skipped with a warning.
fn decode_upgrades_from_header(header: &LedgerHeader) -> Vec<LedgerUpgrade> {
    header
        .scp_value
        .upgrades
        .iter()
        .filter_map(|upgrade| {
            let bytes = upgrade.0.as_slice();
            match LedgerUpgrade::from_xdr(bytes, Limits::none()) {
                Ok(decoded) => Some(decoded),
                Err(err) => {
                    warn!(error = %err, "Failed to decode ledger upgrade during replay");
                    None
                }
            }
        })
        .collect()
}

/// Create a closure that loads live buckets from disk using streaming I/O.
///
/// This uses `Bucket::from_xdr_file_disk_backed()` which streams through the file
/// in two passes (hash computation + index building) without loading the entire file
/// into memory. Memory usage is O(index_size) instead of O(file_size).
///
/// This is critical for mainnet where higher-level buckets can be tens of GB.
/// Loading a 30GB bucket file fully into memory would require 30GB+ RAM, while
/// the streaming approach uses only ~150MB for the index.
fn load_disk_backed_bucket_closure(
    bucket_dir: std::path::PathBuf,
) -> impl FnMut(&Hash256) -> henyey_bucket::Result<Bucket> {
    move |hash: &Hash256| {
        if hash.is_zero() {
            return Ok(Bucket::empty());
        }
        let bucket_path = bucket_dir.join(format!("{}.bucket.xdr", hash.to_hex()));
        if bucket_path.exists() {
            Bucket::from_xdr_file_disk_backed(&bucket_path)
        } else {
            Err(henyey_bucket::BucketError::NotFound(format!(
                "bucket {} not found on disk at {}",
                hash,
                bucket_path.display()
            )))
        }
    }
}

/// Create a closure that loads hot archive buckets from disk using streaming I/O.
///
/// Same memory optimization as `load_disk_backed_bucket_closure` but for hot archive
/// buckets which use `HotArchiveBucketEntry` format instead of `BucketEntry`.
fn load_disk_backed_hot_archive_bucket_closure(
    bucket_dir: std::path::PathBuf,
) -> impl FnMut(&Hash256) -> henyey_bucket::Result<henyey_bucket::HotArchiveBucket> {
    use henyey_bucket::HotArchiveBucket;
    move |hash: &Hash256| {
        if hash.is_zero() {
            return Ok(HotArchiveBucket::empty());
        }
        let bucket_path = bucket_dir.join(format!("{}.bucket.xdr", hash.to_hex()));
        if bucket_path.exists() {
            HotArchiveBucket::from_xdr_file_disk_backed(&bucket_path)
        } else {
            Err(henyey_bucket::BucketError::NotFound(format!(
                "hot archive bucket {} not found on disk at {}",
                hash,
                bucket_path.display()
            )))
        }
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

    /// Verify REPLAY_RETRY_COUNT matches stellar-core's RETRY_A_FEW = 5.
    #[test]
    fn test_replay_retry_count_matches_stellar_core() {
        assert_eq!(REPLAY_RETRY_COUNT, 5, "must match BasicWork::RETRY_A_FEW");
    }

    /// Fatal errors must not be retried — verify the classification.
    #[test]
    fn test_fatal_errors_not_retriable() {
        let fatal_errors: Vec<HistoryError> = vec![
            HistoryError::VerificationFailed("hash mismatch".to_string()),
            HistoryError::InvalidPreviousHash { ledger: 100 },
            HistoryError::InvalidTxSetHash { ledger: 100 },
            HistoryError::InvalidSequence {
                expected: 100,
                got: 200,
            },
            HistoryError::CorruptHeader {
                ledger: 100,
                detail: "bad encoding".to_string(),
            },
        ];
        for err in &fatal_errors {
            assert!(err.is_fatal_catchup_failure(), "expected fatal: {}", err);
        }
    }

    /// Transient errors should be retriable (i.e., NOT fatal).
    #[test]
    fn test_transient_errors_are_retriable() {
        let transient_errors: Vec<HistoryError> = vec![
            HistoryError::ArchiveUnreachable("timeout".to_string()),
            HistoryError::DownloadFailed("connection reset".to_string()),
            HistoryError::CatchupFailed("close_ledger failed at ledger 100".to_string()),
            HistoryError::NotFound("missing file".to_string()),
            HistoryError::HttpStatus {
                url: "http://archive.example.com".to_string(),
                status: 503,
            },
        ];
        for err in &transient_errors {
            assert!(
                !err.is_fatal_catchup_failure(),
                "expected transient (retriable): {}",
                err
            );
        }
    }

    /// Verify the exponential backoff formula used in retry.
    #[test]
    fn test_retry_backoff_formula() {
        // delay_ms = 200 * 2^(attempt-1), capped at 2^4 = 16
        let delays: Vec<u64> = (1..=REPLAY_RETRY_COUNT)
            .map(|attempt| 200 * (1u64 << (attempt - 1).min(4)))
            .collect();
        assert_eq!(delays, vec![200, 400, 800, 1600, 3200]);
    }
}
