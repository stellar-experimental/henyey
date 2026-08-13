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

pub(crate) mod buckets;
mod download;
mod persist;
mod replay;

use crate::{
    archive::HistoryArchive,
    archive_state::HistoryArchiveState,
    catchup_range::{CatchupConfiguration, CatchupMode, CatchupRange, CatchupRunMode},
    checkpoint,
    replay::ReplayConfig,
    verify, CatchupResult, HistoryError, Result,
};
use henyey_bucket::{canonical_bucket_filename, BucketList, BucketManager, HotArchiveBucketList};
use henyey_common::fs_utils::atomic_copy;
use henyey_common::protocol::LclContext;
use henyey_common::{Hash256, NetworkId};
use henyey_db::Database;
use std::collections::HashMap;
use std::sync::Arc;

use henyey_ledger::{LedgerManager, TransactionSetVariant};
use stellar_xdr::LedgerCloseMeta;
use stellar_xdr::{
    ExtensionPoint, GeneralizedTransactionSet, Hash, LedgerCloseMetaExt, LedgerCloseMetaExtV1,
    LedgerCloseMetaV2, LedgerHeader, LedgerHeaderHistoryEntry, LedgerHeaderHistoryEntryExt,
    ScpHistoryEntry, TransactionHistoryEntry, TransactionHistoryResultEntry, TransactionResultPair,
    TransactionSetV1, WriteXdr,
};
use tracing::{debug, info};

/// Extract a `TransactionSetVariant` from a `TransactionHistoryEntry`.
pub(crate) fn tx_set_from_history_entry(entry: &TransactionHistoryEntry) -> TransactionSetVariant {
    use stellar_xdr::TransactionHistoryEntryExt;
    match &entry.ext {
        TransactionHistoryEntryExt::V0 => TransactionSetVariant::Classic(entry.tx_set.clone()),
        TransactionHistoryEntryExt::V1(set) => TransactionSetVariant::Generalized(set.clone()),
    }
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
/// Pass this to [`CatchupManager::catchup_to_ledger_with_checkpoint_data_config`]
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

    /// Optional SCP-derived trusted anchor for reverse-walk verification (§9.1 + INV-C5).
    ///
    /// When set, the reverse-walk verifier uses `TrustSource::Scp` instead of
    /// `TrustSource::None`, making local-state disagreement fatal (non-retriable).
    /// Derived from `Herder::check_ledger_close(target + 1)` during online catchup:
    /// `(target_ledger, tx_set.previous_ledger_hash())`.
    pub(super) trusted_scp_anchor: Option<(u32, Hash256)>,

    /// Optional cap on concurrent bucket-materialization during the cold-catchup
    /// `apply_buckets` restore (lever D, #3249/#3268).
    ///
    /// Threaded into `BucketList::restore_from_has_parallel` /
    /// `HotArchiveBucketList::restore_from_has_parallel` via the `FanOutLimiter`
    /// semaphore. `None` (the default) means unbounded — every level materializes
    /// concurrently, identical to the historical behavior. The restored
    /// `bucketListHash` is independent of this value (levels are assembled by
    /// index, not completion order), so capping is byte-for-byte state-preserving.
    pub(super) restore_apply_fan_out: Option<usize>,
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
            trusted_scp_anchor: None,
            restore_apply_fan_out: None,
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
            trusted_scp_anchor: None,
            restore_apply_fan_out: None,
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

    /// Set the cap on concurrent bucket-materialization during cold-catchup restore.
    ///
    /// Mirrors the warm-restart wiring (lever D, #3249): the cap is threaded into
    /// `BucketList::restore_from_has_parallel` / `HotArchiveBucketList::restore_from_has_parallel`
    /// in `apply_buckets`, bounding how many bucket levels materialize at once via
    /// the `FanOutLimiter` semaphore. `None` means unbounded (the default), which is
    /// byte-for-byte identical to the historical behavior. The restored
    /// `bucketListHash` is independent of this value.
    pub fn set_restore_apply_fan_out(&mut self, cap: Option<usize>) {
        self.restore_apply_fan_out = cap;
    }

    /// Set an SCP-derived trusted anchor for reverse-walk verification (§9.1 + INV-C5).
    ///
    /// When set, `verify_header_chain` builds `ReverseWalkConfig` with
    /// `TrustSource::Scp { seq, hash }` instead of the default `TrustSource::None`.
    /// This makes local-state disagreement during header-chain verification a
    /// fatal (non-retriable) error, matching stellar-core's online catchup behavior
    /// (`LedgerApplyManagerImpl::startOnlineCatchup` → `VerifyLedgerChainWork`).
    ///
    /// The anchor is derived from `Herder::check_ledger_close(target + 1)`:
    /// `(target_ledger, tx_set.previous_ledger_hash())`.
    pub fn set_trusted_scp_anchor(&mut self, seq: u32, hash: Hash256) {
        self.trusted_scp_anchor = Some((seq, hash));
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
    ///
    /// **Stage E instrumentation:** wraps the full pipeline body and emits
    /// `stellar_history_bucket_apply_{success,failure}_total` exactly once per
    /// invocation, so all three callers (`catchup_to_ledger`,
    /// `catchup_to_ledger_with_mode`, `catchup_to_ledger_with_checkpoint_data`)
    /// share the same boundary.
    #[allow(clippy::too_many_arguments)]
    async fn apply_buckets_and_init_ledger_manager(
        &mut self,
        has: &HistoryArchiveState,
        buckets: &[(Hash256, Vec<u8>)],
        checkpoint_seq: u32,
        checkpoint_header: LedgerHeader,
        checkpoint_hash: Hash256,
        ledger_manager: &LedgerManager,
        archive_name: &str,
    ) -> Result<()> {
        let result = self
            .apply_buckets_and_init_ledger_manager_inner(
                has,
                buckets,
                checkpoint_seq,
                checkpoint_header,
                checkpoint_hash,
                ledger_manager,
            )
            .await;
        match &result {
            Ok(()) => {
                metrics::counter!(
                    "stellar_history_bucket_apply_success_total",
                    "archive" => archive_name.to_owned(),
                )
                .increment(1);
            }
            Err(_) => {
                metrics::counter!(
                    "stellar_history_bucket_apply_failure_total",
                    "archive" => archive_name.to_owned(),
                )
                .increment(1);
            }
        }
        result
    }

    async fn apply_buckets_and_init_ledger_manager_inner(
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
            checkpoint_header.ledger_version,
        )
        .await?;

        // Spec: CATCHUP_SPEC §8.5 — verify HAS/header seq agreement before any
        // persistent side effects. Guards `catchup_to_ledger_with_checkpoint_data`
        // where HAS and header are supplied externally.
        if has.current_ledger != checkpoint_header.ledger_seq {
            return Err(HistoryError::VerificationFailed(format!(
                "HAS current_ledger {} != checkpoint header ledger_seq {}",
                has.current_ledger, checkpoint_header.ledger_seq
            )));
        }

        // INV-C15: bucket-apply point must not be older than current LCL.
        // Mirrors stellar-core CatchupWork::assertBucketState() (CatchupWork.cpp:265-270).
        // When ledger_manager is not yet initialized (fresh node from genesis),
        // bucket-apply is always valid — skip the check.
        if ledger_manager.is_initialized() {
            let lcl_seq = ledger_manager.current_ledger_seq();
            if checkpoint_header.ledger_seq < lcl_seq {
                return Err(HistoryError::VerificationFailed(format!(
                    "INV-C15: bucket-apply at ledger {} is older than LCL {}; \
                     refusing to clobber newer state",
                    checkpoint_header.ledger_seq, lcl_seq
                )));
            }
        }

        // §14.5 parity (Window 1): this writes a checkpoint-keyed `bucketlist`
        // row to SQLite. It is durable but NON-AUTHORITATIVE for the startup
        // restore path — `load_last_known_ledger` reads from
        // `last_closed_ledger` + `HISTORY_ARCHIVE_STATE`, not from future
        // `bucketlist` rows. A crash here leaves orphaned rows overwritten by
        // the next successful catchup.
        //
        // Other pre-final-persist writes in this window include:
        // - `persist_header_only` (below, when checkpoint_seq >= target)
        // - `persist_ledger_history` (replay.rs — headers, txs, results)
        // - `emit_meta` (replay.rs — LedgerCloseMeta rows)
        // All share the same non-authoritative property.
        //
        // AHEAD-OF-LCL SWEEP: after a Window-1 crash these rows can lead the
        // durable LCL by up to one checkpoint. `Database::cleanup_ahead_of_lcl()`
        // (#3812, ports stellar-core `CheckpointBuilder::cleanup(lcl)`) truncates
        // every `MAX(ledgerseq)`-anchored table strictly above the durable LCL at
        // node startup (before `verify_on_disk_integrity()`) and in the offline
        // CLI readers (`publish_history`, `self_check`) right after they open the
        // DB, so no reader observes ahead-of-LCL state. `verify_on_disk_integrity`
        // was harmless even before the sweep (hash chain stays valid because
        // headers are written sequentially).
        // (see crates/history/SPEC_ADHERENCE.md §14.5).
        //
        // This is the first of two crash windows; the second (deferred-persist)
        // is guarded by `CATCHUP_PERSIST_PENDING` in
        // `crates/app/src/app/catchup_impl.rs`.
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
            .map_err(|e| map_initialize_error(e, ""))?;

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
        let (final_seq, final_hash, ledgers_applied) = if checkpoint_seq >= target {
            // No replay needed — target is exactly at checkpoint.
            // §14.5 parity: like `persist_bucket_list_snapshot` above,
            // this header row is non-authoritative until `last_closed_ledger`
            // advances via the final persist transaction. Safe to leave
            // on crash for the same reason.
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

            (checkpoint_header.ledger_seq, checkpoint_hash, 0)
        } else {
            let (snap, applied) = self
                .download_verify_and_replay_with_retry(target, ledger_manager)
                .await?;
            (snap.header.ledger_seq, snap.hash, applied)
        };

        self.update_progress(CatchupStatus::Completed, 7, "Catchup completed");

        info!(
            "Catchup completed: ledger {}, hash {}, {} ledgers replayed",
            final_seq, final_hash, ledgers_applied
        );

        Ok(CatchupResult {
            ledger_seq: final_seq,
            ledger_hash: final_hash,
            ledgers_applied,
            buckets_downloaded,
        })
    }

    /// Verify the structure, checkpoint alignment, and (optionally) network
    /// passphrase of a downloaded History Archive State.
    fn verify_has(&self, has: &HistoryArchiveState, checkpoint_seq: u32) -> Result<()> {
        verify::verify_has_structure(has)?;
        verify::verify_has_checkpoint(has, checkpoint_seq)?;
        if let Some(ref expected) = self.network_passphrase {
            verify::verify_has_passphrase(has, expected)?;
        }
        Ok(())
    }

    /// Download and verify the HAS for a checkpoint, emitting the
    /// `stellar_history_download_history_archive_state_{success,failure}_total`
    /// counters on terminal outcome. The combined download+verify is a single
    /// "HAS acquired" event from a dashboard perspective, so any failure in
    /// either step counts as one failure here.
    async fn download_and_verify_has(
        &self,
        checkpoint_seq: u32,
    ) -> Result<(HistoryArchiveState, String)> {
        match self.download_has(checkpoint_seq).await {
            Ok((has, archive)) => match self.verify_has(&has, checkpoint_seq) {
                Ok(()) => {
                    let archive_name = archive.name().to_owned();
                    metrics::counter!(
                        "stellar_history_download_history_archive_state_success_total",
                        "archive" => archive_name.clone(),
                    )
                    .increment(1);
                    Ok((has, archive_name))
                }
                Err(e) => {
                    metrics::counter!(
                        "stellar_history_download_history_archive_state_failure_total",
                        "archive" => archive.name().to_owned(),
                    )
                    .increment(1);
                    Err(e)
                }
            },
            Err(e) => {
                // All archives exhausted — label with the last attempted archive.
                let last_name = self
                    .archives
                    .last()
                    .map(|a| a.name().to_owned())
                    .unwrap_or_default();
                metrics::counter!(
                    "stellar_history_download_history_archive_state_failure_total",
                    "archive" => last_name,
                )
                .increment(1);
                Err(e)
            }
        }
    }

    /// Verify and persist SCP history entries.
    ///
    /// No-op when `entries` is empty.
    fn verify_and_persist_scp_history(&self, entries: &[ScpHistoryEntry]) -> Result<()> {
        if !entries.is_empty() {
            verify::verify_scp_history_entries(entries)?;
            self.persist_scp_history_entries(entries)?;
        }
        Ok(())
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
        let (has, has_archive_name) = self.download_and_verify_has(checkpoint_seq).await?;

        let scp_history = self.download_scp_history(checkpoint_seq).await?;
        self.verify_and_persist_scp_history(&scp_history)?;

        // Step 3: Download all buckets referenced in the HAS
        self.update_progress(
            CatchupStatus::DownloadingBuckets,
            2,
            "Downloading bucket files",
        );
        let bucket_hashes = self.compute_bucket_download_set(&has)?;
        let buckets_total = bucket_hashes.len() as u32;
        self.progress.buckets_total = buckets_total;
        let buckets = self.download_buckets(&bucket_hashes).await?;

        // Steps 4-5: Apply buckets and initialize LedgerManager
        let (checkpoint_header, checkpoint_hash) = self
            .download_checkpoint_header(checkpoint_seq, None)
            .await?;

        self.apply_buckets_and_init_ledger_manager(
            &has,
            &buckets,
            checkpoint_seq,
            checkpoint_header.clone(),
            checkpoint_hash,
            ledger_manager,
            &has_archive_name,
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
    /// Compatibility shim: delegates to [`Self::catchup_to_ledger_with_config`]
    /// with `CatchupRunMode::OfflineBasic`.
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
        self.catchup_to_ledger_with_config(
            target,
            CatchupConfiguration::offline_basic(mode),
            lcl,
            existing_state,
            ledger_manager,
        )
        .await
    }

    /// Catch up to a target ledger with a full catchup configuration.
    ///
    /// This is the config-aware entry point that carries both replay depth and
    /// the spec §6.1 run-mode discriminator.
    ///
    /// # Arguments
    ///
    /// * `target` - The target ledger sequence to catch up to
    /// * `config` - The catchup configuration (depth + run mode)
    /// * `lcl` - The current Last Closed Ledger (use GENESIS_LEDGER_SEQ if starting fresh)
    /// * `existing_state` - If provided, contains the existing bucket lists and header
    ///   for replay-only catchup (Case 1: LCL > genesis). When `None`, Case 1 will
    ///   return an error (bucket lists are required for replay without re-downloading).
    ///
    /// # Returns
    ///
    /// A `CatchupResult` containing the bucket list, header, and summary information.
    pub async fn catchup_to_ledger_with_config(
        &mut self,
        target: u32,
        config: CatchupConfiguration,
        lcl: u32,
        existing_state: Option<ExistingBucketState>,
        ledger_manager: &LedgerManager,
    ) -> Result<CatchupResult> {
        let mode = config.depth;
        let run_mode = config.run_mode;

        // OfflineComplete is not yet implemented (tx-result verification tracked
        // by #2831). Reject loudly so callers cannot silently receive weaker
        // validation than they requested.
        if run_mode == CatchupRunMode::OfflineComplete {
            return Err(HistoryError::CatchupFailed(
                "CatchupRunMode::OfflineComplete is not yet implemented \
                 (tx-result verification tracked by #2831)"
                    .to_string(),
            ));
        }

        info!(
            "Starting catchup to ledger {} with mode {:?}, run_mode={}, lcl={}",
            target, mode, run_mode, lcl
        );
        self.progress.target_ledger = target;

        // Calculate the catchup range based on mode. Bad/derived input
        // (lcl < genesis, target <= lcl, target <= genesis) surfaces as a
        // graceful error rather than aborting the process (#3567), matching
        // stellar-core's checkCatchupPreconditions.
        let range = CatchupRange::calculate(lcl, target, mode)
            .map_err(|e| HistoryError::CatchupFailed(e.to_string()))?;
        info!("Catchup range: {:?}", range);

        let checkpoint_seq = match &range {
            CatchupRange::BucketApplyAndReplay {
                checkpoint: bucket_apply_at,
                ..
            }
            | CatchupRange::BucketsOnly {
                checkpoint: bucket_apply_at,
            } => {
                let bucket_apply_at = *bucket_apply_at;
                info!("Applying buckets at checkpoint {}", bucket_apply_at);

                // Download HAS
                self.update_progress(
                    CatchupStatus::DownloadingHAS,
                    1,
                    "Downloading History Archive State",
                );
                let (has, has_archive_name) = self.download_and_verify_has(bucket_apply_at).await?;

                let scp_history = self.download_scp_history(bucket_apply_at).await?;
                self.verify_and_persist_scp_history(&scp_history)?;

                // Download buckets
                self.update_progress(
                    CatchupStatus::DownloadingBuckets,
                    2,
                    "Downloading bucket files",
                );
                let bucket_hashes = self.compute_bucket_download_set(&has)?;
                self.progress.buckets_total = bucket_hashes.len() as u32;
                let buckets = self.download_buckets(&bucket_hashes).await?;

                // Apply buckets and initialize LedgerManager
                let (checkpoint_header, checkpoint_hash) = self
                    .download_checkpoint_header(bucket_apply_at, None)
                    .await?;

                self.apply_buckets_and_init_ledger_manager(
                    &has,
                    &buckets,
                    bucket_apply_at,
                    checkpoint_header,
                    checkpoint_hash,
                    ledger_manager,
                    &has_archive_name,
                )
                .await?;

                bucket_apply_at
            }
            CatchupRange::ReplayOnly { .. } => {
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
                            let (header, hash) = self.download_checkpoint_header(lcl, None).await?;
                            ledger_manager
                                .initialize(
                                    state.bucket_list,
                                    state.hot_archive_bucket_list,
                                    header,
                                    hash,
                                )
                                .map_err(|e| map_initialize_error(e, " from existing state"))?;
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
            }
        };

        // Download, verify, and replay ledgers with retry.
        // Matches stellar-core's DownloadApplyTxsWork(RETRY_A_FEW).
        if matches!(range, CatchupRange::BucketsOnly { .. }) {
            info!(
                "Catching up to checkpoint {} (no ledgers to replay)",
                checkpoint_seq
            );
        }
        let (header, hash) = self
            .download_checkpoint_header(checkpoint_seq, None)
            .await?;
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

    /// Catch up to a target ledger using pre-downloaded checkpoint data with
    /// an explicit catchup configuration.
    ///
    /// The `config` parameter carries the spec §6.1 run-mode discriminator so
    /// that online minimal catchup going through the historywork fast path
    /// preserves `CatchupRunMode::Online` end-to-end.
    pub async fn catchup_to_ledger_with_checkpoint_data_config(
        &mut self,
        target: u32,
        data: CheckpointData,
        config: CatchupConfiguration,
        ledger_manager: &LedgerManager,
    ) -> Result<CatchupResult> {
        // OfflineComplete is not yet implemented (tx-result verification tracked
        // by #2831). Reject loudly so callers cannot silently receive weaker
        // validation than they requested.
        if config.run_mode == CatchupRunMode::OfflineComplete {
            return Err(HistoryError::CatchupFailed(
                "CatchupRunMode::OfflineComplete is not yet implemented \
                 (tx-result verification tracked by #2831)"
                    .to_string(),
            ));
        }

        // The checkpoint-data fast path only supports minimal replay (replay
        // from the checkpoint boundary to target). Reject non-Minimal depths
        // so callers cannot silently receive fewer replayed ledgers than they
        // requested.
        if !matches!(config.depth, CatchupMode::Minimal) {
            return Err(HistoryError::CatchupFailed(format!(
                "catchup_to_ledger_with_checkpoint_data_config only supports \
                 CatchupMode::Minimal depth, got {:?}. Use \
                 catchup_to_ledger_with_config for non-minimal replay depths.",
                config.depth
            )));
        }

        info!(
            "Starting catchup to ledger {} with checkpoint data (run_mode={})",
            target, config.run_mode
        );
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

        // Step 2: Verify HAS. The HAS was supplied by the caller (no remote
        // download in this path), but we still emit
        // `stellar_history_download_history_archive_state_*` so dashboards see
        // a consistent count of HAS-acquisition events across all catchup
        // variants.
        self.update_progress(
            CatchupStatus::DownloadingHAS,
            1,
            "Using provided History Archive State",
        );
        match self.verify_has(&data.has, checkpoint_seq) {
            Ok(()) => {
                metrics::counter!(
                    "stellar_history_download_history_archive_state_success_total",
                    "archive" => "provided",
                )
                .increment(1);
            }
            Err(e) => {
                metrics::counter!(
                    "stellar_history_download_history_archive_state_failure_total",
                    "archive" => "provided",
                )
                .increment(1);
                return Err(e);
            }
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
        let buckets_downloaded = bucket_hashes.len() as u32;
        self.progress.buckets_total = buckets_downloaded;
        for (idx, hash) in bucket_hashes.iter().enumerate() {
            if !hash.is_empty_bucket_sentinel() {
                let bucket_path = data.bucket_dir.join(canonical_bucket_filename(hash));
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
                if hash.is_empty_bucket_sentinel() {
                    continue;
                }
                let src = data.bucket_dir.join(canonical_bucket_filename(hash));
                let dst = bucket_mgr_dir.join(canonical_bucket_filename(hash));
                if src.exists() && !dst.exists() {
                    atomic_copy(&src, &dst).map_err(|e| {
                        HistoryError::CatchupFailed(format!(
                            "failed to copy bucket {} to bucket manager dir: {}",
                            hash, e
                        ))
                    })?;
                }
            }
        }

        // Step 4: Verify SCP history entries (if present)
        self.verify_and_persist_scp_history(&data.scp_history)?;

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
            "provided",
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
                    .to_xdr(stellar_xdr::Limits::none())
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

/// Maps a [`LedgerError`](henyey_ledger::LedgerError) from
/// `LedgerManager::initialize()` to the appropriate [`HistoryError`],
/// preserving the fatal hash-mismatch classification.
///
/// [`LedgerError::HashMismatch`](henyey_ledger::LedgerError::HashMismatch) is
/// mapped to [`HistoryError::Ledger`] so that
/// [`is_fatal_catchup_failure()`](HistoryError::is_fatal_catchup_failure) and
/// [`is_hash_mismatch()`](HistoryError::is_hash_mismatch) correctly classify it
/// as a fatal integrity failure. All other variants are wrapped in
/// [`HistoryError::CatchupFailed`] with contextual information.
fn map_initialize_error(e: henyey_ledger::LedgerError, context: &str) -> HistoryError {
    match e {
        e @ henyey_ledger::LedgerError::HashMismatch { .. } => HistoryError::Ledger(e),
        other => HistoryError::CatchupFailed(format!(
            "Failed to initialize ledger manager{}: {}",
            context, other
        )),
    }
}

fn checkpoint_header_from_headers(
    checkpoint_seq: u32,
    headers: &[LedgerHeaderHistoryEntry],
) -> Result<(LedgerHeader, Hash256)> {
    for entry in headers {
        if entry.header.ledger_seq == checkpoint_seq {
            let hash = verify::verify_ledger_header_history_entry(entry)?;
            return Ok((entry.header.clone(), hash));
        }
    }

    Err(HistoryError::CatchupFailed(format!(
        "checkpoint header {} not found in headers",
        checkpoint_seq
    )))
}

/// Transaction data for a ledger — either present in the archive or absent
/// (empty-tx ledger where the archive correctly omitted entries per
/// stellar-core `CheckpointBuilder.cpp:140`).
///
/// This enum enforces the both-or-none invariant: a ledger either has both
/// tx history and result entries, or neither. The asymmetric case (one present,
/// one absent) is rejected at construction time.
#[derive(Debug, Clone)]
pub enum LedgerTxData {
    /// Both entries were present in the archive.
    Present {
        tx_history_entry: TransactionHistoryEntry,
        tx_result_entry: TransactionHistoryResultEntry,
    },
    /// Archive omitted both entries (zero-transaction ledger).
    /// The `tx_set` field is synthesized protocol-correctly for
    /// verification and replay, matching stellar-core's
    /// `TxSetXDRFrame::makeEmpty()` (`TxSetFrame.cpp:958-979`).
    Absent { tx_set: TransactionSetVariant },
}

/// Data downloaded for a single ledger.
///
/// Fields are private and construction goes through [`LedgerData::new`], which
/// validates that header/entry ledger sequence numbers match and that the
/// transaction count equals the result count. This eliminates the class of
/// silent-divergence bugs that arose from the previous redundant field pairs
/// (#1760, #1802).
#[derive(Debug, Clone)]
pub struct LedgerData {
    header: LedgerHeader,
    tx_data: LedgerTxData,
}

impl LedgerData {
    /// Construct a new `LedgerData`, validating invariants.
    ///
    /// Accepts optional tx/result entries:
    /// - `(Some, Some)` → validates seq numbers and tx/result count match
    /// - `(None, None)` → synthesizes a protocol-correct empty tx set using
    ///   the [`LclContext`] (the previous ledger's protocol version and hash)
    /// - `(Some, None)` or `(None, Some)` → returns error (asymmetric is invalid)
    ///
    /// # Arguments
    ///
    /// * `lcl` — context from the Last Closed Ledger, used to select the correct
    ///   empty tx set format when no tx entries are present. Mirrors
    ///   stellar-core's `TxSetXDRFrame::makeEmpty(lclHeader)` semantics.
    pub fn new(
        header: LedgerHeader,
        tx_history_entry: Option<TransactionHistoryEntry>,
        tx_result_entry: Option<TransactionHistoryResultEntry>,
        lcl: &LclContext,
    ) -> Result<Self> {
        let tx_data = match (tx_history_entry, tx_result_entry) {
            (Some(tx_entry), Some(result_entry)) => {
                if header.ledger_seq != tx_entry.ledger_seq {
                    return Err(HistoryError::VerificationFailed(format!(
                        "ledger_seq mismatch: header={} tx_history_entry={}",
                        header.ledger_seq, tx_entry.ledger_seq
                    )));
                }
                if header.ledger_seq != result_entry.ledger_seq {
                    return Err(HistoryError::VerificationFailed(format!(
                        "ledger_seq mismatch: header={} tx_result_entry={}",
                        header.ledger_seq, result_entry.ledger_seq
                    )));
                }
                let tx_set = tx_set_from_history_entry(&tx_entry);
                let tx_count = tx_set.transactions_with_base_fee().len();
                let result_count = result_entry.tx_result_set.results.len();
                if tx_count != result_count {
                    return Err(HistoryError::VerificationFailed(format!(
                        "tx/result count mismatch for ledger {}: {} txs vs {} results",
                        header.ledger_seq, tx_count, result_count
                    )));
                }
                LedgerTxData::Present {
                    tx_history_entry: tx_entry,
                    tx_result_entry: result_entry,
                }
            }
            (None, None) => {
                if header.previous_ledger_hash != *lcl.lcl_hash() {
                    return Err(HistoryError::VerificationFailed(format!(
                        "LCL stitching failed at ledger {}: previous_hash {} != expected lcl_hash {}",
                        header.ledger_seq,
                        header.previous_ledger_hash,
                        lcl.lcl_hash(),
                    )));
                }
                let tx_set = make_empty_tx_set(lcl);
                LedgerTxData::Absent { tx_set }
            }
            (Some(_), None) => {
                return Err(HistoryError::VerificationFailed(format!(
                    "ledger {}: tx history entry present but result entry missing (asymmetric)",
                    header.ledger_seq
                )));
            }
            (None, Some(_)) => {
                return Err(HistoryError::VerificationFailed(format!(
                    "ledger {}: result entry present but tx history entry missing (asymmetric)",
                    header.ledger_seq
                )));
            }
        };
        Ok(Self { header, tx_data })
    }

    /// The ledger header.
    pub fn header(&self) -> &LedgerHeader {
        &self.header
    }

    /// Extract the transaction set.
    ///
    /// Always available: for `Present` data, extracted from the history entry;
    /// for `Absent` data, a protocol-correct synthesized empty set.
    pub fn tx_set(&self) -> TransactionSetVariant {
        match &self.tx_data {
            LedgerTxData::Present {
                tx_history_entry, ..
            } => tx_set_from_history_entry(tx_history_entry),
            LedgerTxData::Absent { tx_set } => tx_set.clone(),
        }
    }

    /// The transaction history entry, if present in the archive.
    pub fn tx_history_entry(&self) -> Option<&TransactionHistoryEntry> {
        match &self.tx_data {
            LedgerTxData::Present {
                tx_history_entry, ..
            } => Some(tx_history_entry),
            LedgerTxData::Absent { .. } => None,
        }
    }

    /// The transaction result history entry, if present in the archive.
    pub fn tx_result_entry(&self) -> Option<&TransactionHistoryResultEntry> {
        match &self.tx_data {
            LedgerTxData::Present {
                tx_result_entry, ..
            } => Some(tx_result_entry),
            LedgerTxData::Absent { .. } => None,
        }
    }

    /// Reference to the transaction result pairs.
    /// Returns an empty slice for absent (empty-tx) ledgers.
    pub fn tx_results(&self) -> &[TransactionResultPair] {
        match &self.tx_data {
            LedgerTxData::Present {
                tx_result_entry, ..
            } => &tx_result_entry.tx_result_set.results,
            LedgerTxData::Absent { .. } => &[],
        }
    }

    /// Whether this ledger has transactions (entries present in archive).
    pub fn has_transactions(&self) -> bool {
        matches!(self.tx_data, LedgerTxData::Present { .. })
    }
}

/// Construct the canonical empty [`TransactionSetVariant`] for a ledger,
/// matching stellar-core's `TxSetXDRFrame::makeEmpty` (`TxSetFrame.cpp:958-979`).
///
/// Accepts an [`LclContext`] that bundles the LCL hash and protocol version,
/// ensuring callers cannot accidentally mix values from different ledgers.
///
/// Protocol ranges (based on LCL protocol):
/// - `< 20`: Classic `TransactionSet` with empty txs
/// - `20-22`: `GeneralizedTransactionSet` with two V0 (sequential) phases
/// - `23+`: `GeneralizedTransactionSet` with V0 classic + V1 (parallel) soroban
pub(crate) fn make_empty_tx_set(lcl: &LclContext) -> TransactionSetVariant {
    use henyey_common::protocol::{
        protocol_version_starts_from, ProtocolVersion, PARALLEL_SOROBAN_PHASE_PROTOCOL_VERSION,
    };
    use stellar_xdr::{TransactionPhase, TransactionSet, VecM};

    let previous_ledger_hash = lcl.lcl_hash().clone();
    let lcl_protocol_version = lcl.protocol_version();

    if protocol_version_starts_from(lcl_protocol_version, ProtocolVersion::V20) {
        let classic_phase = TransactionPhase::V0(VecM::default());
        let soroban_phase = if protocol_version_starts_from(
            lcl_protocol_version,
            PARALLEL_SOROBAN_PHASE_PROTOCOL_VERSION,
        ) {
            // Protocol 23+: parallel soroban phase
            henyey_tx::tx_set_xdr::empty_soroban_phase()
        } else {
            // Protocol 20-22: sequential soroban phase
            TransactionPhase::V0(VecM::default())
        };
        TransactionSetVariant::Generalized(GeneralizedTransactionSet::V1(TransactionSetV1 {
            previous_ledger_hash,
            phases: vec![classic_phase, soroban_phase]
                .try_into()
                .unwrap_or_default(),
        }))
    } else {
        // Protocol < 20: classic tx set
        TransactionSetVariant::Classic(TransactionSet {
            previous_ledger_hash,
            txs: Default::default(),
        })
    }
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
            verify_header_chain: self.options.verify_headers,
            verify_tx_set: true,
            verify_tx_results: false,
            verify_bucket_list: self.options.verify_buckets,
            verify_header_hash: self.options.verify_headers,
            emit_classic_events: false,
            backfill_stellar_asset_events: false,
            run_eviction: true,
            eviction_settings: stellar_xdr::StateArchivalSettings::default(),
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

    /// Stage E: source code in this module emits Prometheus metrics by raw
    /// string literal (not by constant — `henyey-history` cannot depend on
    /// `henyey-app` without a circular workspace dep). This test pins the
    /// literals so a typo or rename detaches *both* sides from the catalog.
    ///
    /// The matching tests on the catalog side
    /// (`crates/app/src/metrics.rs::test_stage_e_history_metrics_in_catalog`
    /// and `test_stage_e_history_constant_string_stability`) verify the same
    /// strings exist there. Together, the two tests guarantee end-to-end
    /// wire-name stability without coupling the crates.
    #[test]
    fn test_stage_e_history_metric_literals_present() {
        // Read this source file at test time and assert each emit-site
        // literal is exactly as expected. Catches typos like
        // `stellar_history_publish_succes_total` (missing 's') that would
        // produce silent zero-rate panels in production.
        let src = include_str!("mod.rs");
        for literal in &[
            "\"stellar_history_bucket_apply_success_total\"",
            "\"stellar_history_bucket_apply_failure_total\"",
            "\"stellar_history_download_history_archive_state_success_total\"",
            "\"stellar_history_download_history_archive_state_failure_total\"",
        ] {
            assert!(
                src.contains(literal),
                "expected metric literal {literal} in catchup/mod.rs",
            );
        }
    }

    /// Stage E: bucket_apply counters must carry the `"archive"` label.
    #[test]
    fn test_stage_e_mod_archive_label_present() {
        let src = include_str!("mod.rs");
        let main_code = src.split("#[cfg(test)]").next().unwrap_or(src);
        for metric in &[
            "stellar_history_bucket_apply_success_total",
            "stellar_history_bucket_apply_failure_total",
        ] {
            let mut search_from = 0;
            let mut found_any = false;
            while let Some(rel_idx) = main_code[search_from..].find(metric) {
                found_any = true;
                let idx = search_from + rel_idx;
                let window = &main_code[idx..std::cmp::min(idx + 200, main_code.len())];
                assert!(
                    window.contains("\"archive\""),
                    "metric {metric} missing \"archive\" label at byte offset {idx} \
                     in catchup/mod.rs",
                );
                search_from = idx + metric.len();
            }
            assert!(found_any, "metric {metric} not found in catchup/mod.rs",);
        }
    }

    #[test]
    fn test_checkpoint_header_from_headers_verifies_entry_hash() {
        let header = LedgerHeader {
            ledger_version: 20,
            previous_ledger_hash: Hash([0u8; 32]),
            scp_value: stellar_xdr::StellarValue {
                tx_set_hash: Hash([0u8; 32]),
                close_time: stellar_xdr::TimePoint(0),
                upgrades: stellar_xdr::VecM::default(),
                ext: stellar_xdr::StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0u8; 32]),
            bucket_list_hash: Hash([0u8; 32]),
            ledger_seq: 63,
            total_coins: 0,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 5000000,
            max_tx_set_size: 100,
            skip_list: std::array::from_fn(|_| Hash([0u8; 32])),
            ext: stellar_xdr::LedgerHeaderExt::V0,
        };
        let entry = LedgerHeaderHistoryEntry {
            hash: Hash([1u8; 32]),
            header,
            ext: LedgerHeaderHistoryEntryExt::default(),
        };

        assert!(checkpoint_header_from_headers(63, &[entry]).is_err());
    }

    #[test]
    fn test_map_initialize_error_hash_mismatch_is_fatal() {
        let err = henyey_ledger::LedgerError::HashMismatch {
            expected: "aabb".to_string(),
            actual: "ccdd".to_string(),
        };
        let mapped = map_initialize_error(err, "");
        assert!(
            mapped.is_fatal_catchup_failure(),
            "HashMismatch from initialize should be classified as a fatal catchup failure"
        );
        assert!(
            mapped.is_hash_mismatch(),
            "HashMismatch from initialize should be classified as a hash mismatch"
        );
        assert!(
            matches!(
                mapped,
                HistoryError::Ledger(henyey_ledger::LedgerError::HashMismatch { .. })
            ),
            "HashMismatch should be preserved as HistoryError::Ledger variant"
        );
    }

    #[test]
    fn test_map_initialize_error_other_variant_not_fatal() {
        let err = henyey_ledger::LedgerError::AlreadyInitialized;
        let mapped = map_initialize_error(err, " from existing state");
        assert!(
            !mapped.is_fatal_catchup_failure(),
            "AlreadyInitialized should NOT be classified as a fatal catchup failure"
        );
        assert!(
            !mapped.is_hash_mismatch(),
            "AlreadyInitialized should NOT be classified as a hash mismatch"
        );
        assert!(
            matches!(mapped, HistoryError::CatchupFailed(ref msg) if msg.contains("from existing state")),
            "Non-HashMismatch errors should be wrapped in CatchupFailed with context"
        );
    }
}
