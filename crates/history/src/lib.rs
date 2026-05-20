//! History archive access and catchup for rs-stellar-core.
//!
//! This crate provides the infrastructure for interacting with Stellar history archives,
//! enabling nodes to synchronize with the network by downloading and verifying historical
//! ledger data.
//!
//! # Overview
//!
//! Stellar validators publish their ledger history to "history archives" - HTTP-accessible
//! repositories containing checkpoints of network state. This crate handles:
//!
//! - **Archive Access**: Downloading ledger headers, transactions, results, and bucket files
//! - **Catchup**: Synchronizing a node from genesis or a recent checkpoint
//! - **Replay**: Re-executing transactions to verify state transitions
//! - **Publishing**: Writing new checkpoints to history archives (for validators)
//! - **Verification**: Ensuring data integrity via cryptographic hash chains
//!
//! # History Archive Structure
//!
//! Archives are organized around **checkpoints** - snapshots taken every 64 ledgers.
//! Each checkpoint includes:
//!
//! - **Ledger headers**: Metadata for each ledger in the checkpoint
//! - **Transaction sets**: All transactions executed in the checkpoint
//! - **Transaction results**: Outcomes of each transaction
//! - **Bucket files**: Serialized ledger state (accounts, trustlines, offers, etc.)
//! - **SCP messages**: Consensus protocol messages for verification
//!
//! Files are organized hierarchically by their hex-encoded sequence number:
//! ```text
//! history/00/00/00/history-0000003f.json  # Checkpoint 63
//! ledger/00/00/00/ledger-0000003f.xdr.gz  # Ledger headers
//! bucket/e1/13/f8/bucket-e113f8...fd.xdr.gz  # Bucket by hash
//! ```
//!
//! # Quick Start
//!
//! ```no_run
//! use henyey_history::archive::HistoryArchive;
//!
//! # async fn example() -> Result<(), henyey_history::HistoryError> {
//! // Connect to a testnet archive
//! let archive = HistoryArchive::new(
//!     "https://history.stellar.org/prd/core-testnet/core_testnet_001"
//! )?;
//!
//! // Get the current archive state (History Archive State / HAS)
//! let has = archive.fetch_root_has().await?;
//! println!("Current ledger: {}", has.current_ledger());
//!
//! // Get all bucket hashes needed for catchup
//! let buckets = has.all_bucket_hashes();
//! println!("Need {} buckets", buckets.len());
//!
//! // Download ledger headers for a checkpoint
//! let headers = archive.fetch_ledger_headers(63).await?;
//! println!("Got {} ledger headers", headers.len());
//! # Ok(())
//! # }
//! ```
//!
//! # Catchup Modes
//!
//! The catchup process supports different synchronization strategies:
//!
//! - **Minimal**: Download only the latest checkpoint state to start validating new ledgers
//!   immediately. Best for validators that do not need historical data.
//!
//! - **Complete**: Download full history from genesis. Required for archival nodes that
//!   need to serve historical queries.
//!
//! - **Recent**: Download the last N ledgers of history. Useful for nodes that need
//!   some historical context but not the complete chain.
//!
//! # Key Types
//!
//! - [`HistoryArchive`]: Client for accessing a single history archive
//! - [`HistoryManager`]: Manages multiple archives with failover support
//! - [`HistoryArchiveState`]: Parsed checkpoint metadata (HAS file)
//! - [`CatchupManager`]: Orchestrates the full catchup process
//! - [`ReplayConfig`]: Configuration for ledger replay and verification
//!
//! # Verification
//!
//! All downloaded data is cryptographically verified:
//!
//! 1. Bucket hashes are verified against their content
//! 2. Ledger headers form a hash chain (each references the previous)
//! 3. Transaction sets and results match the hashes in headers
//! 4. The final bucket list hash matches the target ledger header
//!
//! [`HistoryArchive`]: archive::HistoryArchive
//! [`HistoryArchiveState`]: archive_state::HistoryArchiveState
//! [`CatchupManager`]: catchup::CatchupManager
//! [`ReplayConfig`]: replay::ReplayConfig

// Module declarations with brief descriptions
//
// Archive access and data retrieval
pub mod archive;
pub mod archive_state;
pub mod download;
pub mod paths;

// Catchup and synchronization
pub mod catchup;
pub mod catchup_range;
pub mod checkpoint;
pub mod replay;
pub mod verify;

// Comparison and validation
pub mod compare;
pub(crate) mod ordering;

// Publishing and external data sources
pub mod cdp;
pub mod checkpoint_builder;
pub mod publish;
pub mod publish_queue;
pub mod remote_archive;
pub mod upload;

// Error handling
pub mod error;

/// Reusable in-process history-archive fixtures for integration tests.
/// Gated behind the `test-utils` feature so production builds don't pull
/// in the axum dependency.
#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

#[cfg(test)]
pub(crate) mod tracing_test_support;

// Re-export main types at crate root
pub use archive::HistoryArchive;
pub use archive_state::HistoryArchiveState;
pub use archive_state::MAX_HISTORY_ARCHIVE_BUCKET_SIZE;
pub use archive_state::{validate_bucket_list_structure, BucketLevelVersionInfo};
pub use catchup::{
    CatchupManager, CatchupOptions, CatchupProgress, CatchupStatus, CheckpointData,
    ExistingBucketState, LedgerData, LedgerTxData,
};
pub use catchup_range::{
    CatchupConfiguration, CatchupMode, CatchupRunMode, LedgerRange, GENESIS_LEDGER_SEQ,
};
pub use cdp::{
    extract_ledger_header, extract_transaction_envelopes, extract_transaction_metas, CacheStats,
    CachedCdpDataLake, CdpDataLake,
};
pub use checkpoint::{
    checkpoint_containing, checkpoint_frequency, checkpoint_ledger, checkpoint_start,
    first_ledger_after_checkpoint_containing, first_ledger_in_checkpoint_containing,
    is_checkpoint_ledger, is_checkpoint_start, last_ledger_before_checkpoint_containing,
    latest_checkpoint_before_or_at, ledger_to_trigger_catchup, set_checkpoint_frequency,
    size_of_checkpoint_containing, ACCELERATED_CHECKPOINT_FREQUENCY, DEFAULT_CHECKPOINT_FREQUENCY,
};
pub use checkpoint_builder::write_record_marked_xdr;
pub use compare::{compare_checkpoint, Category, CheckpointComparison, Mismatch};
pub use download::DownloadConfig;
pub use error::{HistoryError, TxSetHashMismatchInfo, VerifyHashKind, VerifyHashMismatchInfo};
pub use paths::{
    bucket_path, checkpoint_path, checkpoint_path_dirty, dirty_to_final_path, final_to_dirty_path,
    is_dirty_path,
};
pub use publish::build_history_archive_state;
pub use publish_queue::{
    PublishQueue, PublishQueueStats, PUBLISH_QUEUE_MAX_SIZE, PUBLISH_QUEUE_UNBLOCK_APPLICATION,
};
pub use remote_archive::{RemoteArchive, RemoteArchiveConfig};
pub use replay::{ReplayConfig, ReplayedLedgerState};
pub use verify::{
    compute_header_hash, verify_bucket_hash, verify_chain_anchors, verify_has_passphrase,
    verify_header_chain, verify_header_chain_from_entries, verify_ledger_header_history_entry,
    verify_reverse_walk, verify_tx_result_ordering, ChainTrustAnchors, ReverseWalkConfig,
    ReverseWalkResult, TrustSource,
};

/// Result type for history operations.
pub type Result<T> = std::result::Result<T, HistoryError>;

/// Construct an empty `TransactionSet` with a zeroed previous-ledger hash.
pub fn make_empty_tx_set() -> stellar_xdr::curr::TransactionSet {
    stellar_xdr::curr::TransactionSet {
        previous_ledger_hash: stellar_xdr::curr::Hash([0u8; 32]),
        txs: stellar_xdr::curr::VecM::default(),
    }
}

// Re-export archive manager types (added at end of file)

/// Configuration for a single history archive.
///
/// Archives can be configured for reading (get), writing (put), or both.
/// Validators typically configure multiple archives with different capabilities:
/// - Public archives for reading during catchup
/// - Private archives for publishing their own history
#[derive(Debug, Clone)]
pub struct ArchiveConfig {
    /// Base URL of the archive (e.g., `https://history.stellar.org/prd/core-live/core_live_001`).
    pub url: url::Url,
    /// Whether this archive can be used for downloading history data.
    pub get_enabled: bool,
    /// Whether this archive can be used for publishing history data.
    pub put_enabled: bool,
}

/// Manager for multiple history archives with failover support.
///
/// The `HistoryManager` wraps multiple [`HistoryArchive`] instances and provides
/// automatic failover: if one archive fails to respond, the manager tries the next
/// archive in the list until one succeeds or all archives have been exhausted.
///
/// This is essential for reliable catchup since individual archives may be temporarily
/// unavailable or have incomplete data.
///
/// # Example
///
/// ```no_run
/// use henyey_history::{HistoryManager, archive::testnet};
///
/// # async fn example() -> Result<(), henyey_history::HistoryError> {
/// let manager = HistoryManager::from_urls(testnet::ARCHIVE_URLS)?;
///
/// // Automatically tries each archive until one succeeds
/// let has = manager.fetch_root_has().await?;
/// println!("Network at ledger {}", has.current_ledger());
/// # Ok(())
/// # }
/// ```
pub struct HistoryManager {
    archives: Vec<HistoryArchive>,
}

impl HistoryManager {
    /// Create a new history manager with the given archives.
    pub fn new(archives: Vec<HistoryArchive>) -> Self {
        Self { archives }
    }

    /// Create a history manager from a list of URLs.
    pub fn from_urls(urls: &[&str]) -> Result<Self> {
        let archives: Result<Vec<_>> = urls.iter().map(|url| HistoryArchive::new(url)).collect();
        Ok(Self::new(archives?))
    }

    /// Add an archive to the manager.
    pub fn add_archive(&mut self, archive: HistoryArchive) {
        self.archives.push(archive);
    }

    /// Get the number of archives.
    pub fn archive_count(&self) -> usize {
        self.archives.len()
    }

    /// Try an operation against each archive until one succeeds.
    ///
    /// On failure, logs a warning with the archive URL and the error,
    /// then moves on to the next archive. Returns `fallback_err` if all
    /// archives fail.
    async fn try_archives<'a, F, T>(
        &'a self,
        op: F,
        context: &str,
        fallback_err: HistoryError,
    ) -> Result<T>
    where
        F: Fn(
            &'a HistoryArchive,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T>> + Send + 'a>>,
    {
        for archive in &self.archives {
            match op(archive).await {
                Ok(value) => return Ok(value),
                Err(e) => {
                    tracing::warn!(
                        url = %archive.base_url(),
                        error = %e,
                        "Failed to {} from archive", context,
                    );
                }
            }
        }
        Err(fallback_err)
    }

    /// Get the root HAS from any available archive.
    ///
    /// Tries each archive in sequence until one succeeds.
    pub async fn fetch_root_has(&self) -> Result<HistoryArchiveState> {
        self.try_archives(
            |a| Box::pin(a.fetch_root_has()),
            "get HAS",
            HistoryError::NoArchiveAvailable,
        )
        .await
    }

    /// Get the checkpoint HAS from any available archive.
    pub async fn fetch_checkpoint_has(&self, ledger: u32) -> Result<HistoryArchiveState> {
        self.try_archives(
            |a| Box::pin(a.fetch_checkpoint_has(ledger)),
            "get checkpoint HAS",
            HistoryError::CheckpointNotFound(ledger),
        )
        .await
    }

    /// Download a bucket from any available archive.
    pub async fn fetch_bucket(&self, hash: &henyey_common::Hash256) -> Result<Vec<u8>> {
        let hash = *hash;
        self.try_archives(
            |a| Box::pin(a.fetch_bucket(&hash)),
            "get bucket",
            HistoryError::BucketNotFound(hash),
        )
        .await
    }

    /// Get ledger headers for a checkpoint from any available archive.
    pub async fn fetch_ledger_headers(
        &self,
        checkpoint: u32,
    ) -> Result<Vec<stellar_xdr::curr::LedgerHeaderHistoryEntry>> {
        self.try_archives(
            |a| Box::pin(a.fetch_ledger_headers(checkpoint)),
            "get ledger headers",
            HistoryError::CheckpointNotFound(checkpoint),
        )
        .await
    }

    /// Get transactions for a checkpoint from any available archive.
    pub async fn fetch_transactions(
        &self,
        checkpoint: u32,
    ) -> Result<Vec<stellar_xdr::curr::TransactionHistoryEntry>> {
        self.try_archives(
            |a| Box::pin(a.fetch_transactions(checkpoint)),
            "get transactions",
            HistoryError::CheckpointNotFound(checkpoint),
        )
        .await
    }

    /// Get transaction results for a checkpoint from any available archive.
    pub async fn fetch_results(
        &self,
        checkpoint: u32,
    ) -> Result<Vec<stellar_xdr::curr::TransactionHistoryResultEntry>> {
        self.try_archives(
            |a| Box::pin(a.fetch_results(checkpoint)),
            "get transaction results",
            HistoryError::CheckpointNotFound(checkpoint),
        )
        .await
    }
}

/// Summary result of a successful catchup operation.
///
/// Provides high-level statistics about the catchup process. The caller
/// can query the `LedgerManager` for the current header, bucket lists, etc.
#[derive(Debug)]
pub struct CatchupResult {
    /// The ledger sequence that was caught up to.
    pub ledger_seq: u32,

    /// The SHA-256 hash of the final ledger header.
    pub ledger_hash: henyey_common::Hash256,

    /// Number of ledgers replayed from checkpoint to target.
    ///
    /// Zero if the target was exactly at a checkpoint boundary.
    pub ledgers_applied: u32,

    /// Number of bucket files downloaded during catchup.
    pub buckets_downloaded: u32,
}

impl std::fmt::Display for CatchupResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Caught up to ledger {} (hash: {}, {} buckets, {} ledgers applied)",
            self.ledger_seq,
            &self.ledger_hash.to_hex()[..16],
            self.buckets_downloaded,
            self.ledgers_applied
        )
    }
}

/// Testnet archive configuration.
pub mod testnet {
    pub use crate::archive::testnet::*;
}

/// Mainnet archive configuration.
pub mod mainnet {
    pub use crate::archive::mainnet::*;
}

// ============================================================================
// History Archive Manager (Parity with stellar-core HistoryArchiveManager)
// ============================================================================

/// An entry in the archive manager combining read and write capabilities.
///
/// Each archive entry can have:
/// - An HTTP archive for reading (fetching HAS, ledger data, buckets)
/// - A remote archive for writing (uploading via shell commands)
///
/// Archives with both capabilities can be used for both catchup and publishing.
#[derive(Debug, Clone)]
pub struct ArchiveEntry {
    /// Name of the archive (for identification and logging).
    pub name: String,
    /// HTTP archive for reading (optional - archive may be write-only).
    pub archive: Option<HistoryArchive>,
    /// Remote archive for writing via shell commands (optional - archive may be read-only).
    pub remote: Option<RemoteArchive>,
}

impl ArchiveEntry {
    /// Create a new archive entry with both read and write capabilities.
    pub fn new(
        name: String,
        archive: Option<HistoryArchive>,
        remote: Option<RemoteArchive>,
    ) -> Self {
        Self {
            name,
            archive,
            remote,
        }
    }

    /// Create a read-only archive entry (for catchup sources).
    pub fn read_only(name: String, archive: HistoryArchive) -> Self {
        Self {
            name,
            archive: Some(archive),
            remote: None,
        }
    }

    /// Create a write-only archive entry (for publishing destinations).
    pub fn write_only(name: String, remote: RemoteArchive) -> Self {
        Self {
            name,
            archive: None,
            remote: Some(remote),
        }
    }

    /// Check if this archive can be used for reading.
    pub fn can_read(&self) -> bool {
        self.archive.is_some() || self.remote.as_ref().is_some_and(|r| r.can_read())
    }

    /// Check if this archive can be used for writing.
    pub fn can_write(&self) -> bool {
        self.remote.as_ref().is_some_and(|r| r.can_write())
    }

    /// Check if this archive is fully configured (both read and write).
    pub fn is_fully_configured(&self) -> bool {
        self.can_read() && self.can_write()
    }
}

/// Manager for history archives with support for reading and writing.
///
/// This is the Rust equivalent of stellar-core `HistoryArchiveManager`. It manages
/// multiple history archives and provides:
///
/// - **Writable archive detection**: Determine which archives can be used for publishing
/// - **Archive initialization**: Create new archives with empty HAS
/// - **Configuration validation**: Check for sensible archive configuration
///
/// # Example
///
/// ```no_run
/// use henyey_history::{HistoryArchiveManager, ArchiveEntry, HistoryArchive};
/// use henyey_history::remote_archive::{RemoteArchive, RemoteArchiveConfig};
///
/// # async fn example() -> Result<(), henyey_history::HistoryError> {
/// // Create archive manager with both read and write archives
/// let mut manager = HistoryArchiveManager::new("Test SDF Network ; September 2015".to_string());
///
/// // Add a read-only archive (for catchup)
/// let archive = HistoryArchive::new("https://history.stellar.org/prd/core-testnet/core_testnet_001")?;
/// manager.add_archive(ArchiveEntry::read_only("sdf-testnet".to_string(), archive));
///
/// // Add a read-write archive (for publishing)
/// let read_archive = HistoryArchive::new("https://my-archive.example.com")?;
/// let remote = RemoteArchive::new(RemoteArchiveConfig {
///     name: "my-archive".to_string(),
///     get_cmd: Some("curl -sf {0} -o {1}".to_string()),
///     put_cmd: Some("aws s3 cp {0} s3://my-bucket{1}".to_string()),
///     mkdir_cmd: Some("aws s3api put-object --bucket my-bucket --key {0}/".to_string()),
/// });
/// manager.add_archive(ArchiveEntry::new("my-archive".to_string(), Some(read_archive), Some(remote)));
///
/// // Check if publishing is enabled
/// if manager.publish_enabled() {
///     println!("Publishing enabled with {} writable archives", manager.get_writable_archives().len());
/// }
/// # Ok(())
/// # }
/// ```
pub struct HistoryArchiveManager {
    /// All configured archives.
    archives: Vec<ArchiveEntry>,
    /// Network passphrase for HAS files.
    network_passphrase: String,
}

impl HistoryArchiveManager {
    /// Create a new archive manager with the given network passphrase.
    pub fn new(network_passphrase: String) -> Self {
        Self {
            archives: Vec::new(),
            network_passphrase,
        }
    }

    /// Add an archive to the manager.
    pub fn add_archive(&mut self, entry: ArchiveEntry) {
        self.archives.push(entry);
    }

    /// Get the number of archives.
    pub fn archive_count(&self) -> usize {
        self.archives.len()
    }

    /// Get an archive entry by name.
    pub fn get_archive(&self, name: &str) -> Result<&ArchiveEntry> {
        self.archives
            .iter()
            .find(|a| a.name == name)
            .ok_or_else(|| HistoryError::ArchiveNotFound(name.to_string()))
    }

    /// Get a mutable reference to an archive entry by name.
    pub fn get_archive_mut(&mut self, name: &str) -> Result<&mut ArchiveEntry> {
        self.archives
            .iter_mut()
            .find(|a| a.name == name)
            .ok_or_else(|| HistoryError::ArchiveNotFound(name.to_string()))
    }

    /// Returns true if any archive has both read and write capabilities.
    ///
    /// This is used to determine if the node can publish checkpoints.
    /// Publishing requires both:
    /// - Read capability (to verify what's already published)
    /// - Write capability (to upload new checkpoints)
    pub fn publish_enabled(&self) -> bool {
        self.archives.iter().any(|a| a.is_fully_configured())
    }

    /// Get all archives that have both read and write capabilities.
    ///
    /// These are the archives that can be used as publishing destinations.
    pub fn get_writable_archives(&self) -> Vec<&ArchiveEntry> {
        self.archives
            .iter()
            .filter(|a| a.is_fully_configured())
            .collect()
    }

    /// Get all archives that can be used for reading (catchup sources).
    pub fn get_readable_archives(&self) -> Vec<&ArchiveEntry> {
        self.archives.iter().filter(|a| a.can_read()).collect()
    }

    /// Check that archives are sensibly configured.
    ///
    /// This logs warnings for potentially problematic configurations:
    /// - Archives with neither read nor write capability (inert)
    /// - Archives with write but no read capability (can't verify)
    /// - No readable archives configured (catchup will fail)
    ///
    /// Returns true if the configuration is sensible.
    pub fn check_sensible_config(&self) -> bool {
        let mut sensible = true;

        for entry in &self.archives {
            if !entry.can_read() && !entry.can_write() {
                tracing::warn!(
                    archive = %entry.name,
                    "Archive has neither get nor put configured (inert)"
                );
                sensible = false;
            } else if entry.can_write() && !entry.can_read() {
                tracing::warn!(
                    archive = %entry.name,
                    "Archive has put but no get configured (cannot verify uploads)"
                );
                // This is a warning but not fatal
            }
        }

        if self.get_readable_archives().is_empty() {
            tracing::error!("No readable archives configured - catchup will fail");
            sensible = false;
        }

        if self.get_writable_archives().is_empty() {
            tracing::info!("No writable archives configured - publishing disabled");
            // This is informational, not an error
        }

        sensible
    }

    /// Initialize a new history archive with an empty HAS.
    ///
    /// Mirrors stellar-core's `HistoryArchiveManager::initializeHistoryArchive`
    /// (and its `PutHistoryArchiveStateWork::spawnPublishWork` upload step):
    /// writes the same empty `HistoryArchiveState` JSON to **two** files in
    /// the archive:
    ///
    /// 1. The §4.3 ledger-zero pseudo-checkpoint at
    ///    `history/00/00/00/history-00000000.json` — the marker that signals
    ///    the archive has been initialized (see `HistoryManager.h` §4.3).
    /// 2. The well-known root HAS at `.well-known/stellar-history.json` —
    ///    the file `GetHistoryArchiveStateWork(seq=0)` discovers via
    ///    `isWellKnown(0)`.
    ///
    /// The empty HAS uses `version = 1` with `hot_archive_buckets = None`,
    /// matching stellar-core's default `HistoryArchiveState` constructor in
    /// `HistoryArchive.cpp`.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The archive is not found by name
    /// - The archive is already initialized (HAS exists)
    /// - The archive is not writable (no put command)
    /// - The upload fails
    ///
    /// # Partial failures and idempotency
    ///
    /// Initialization writes two files in sequence: the §4.3 pseudo-checkpoint
    /// first, then the well-known root HAS. If the first upload succeeds but
    /// the second fails (e.g. transient network error), the archive is left
    /// **half-initialized**: the pseudo-checkpoint exists, but the root HAS
    /// does not.
    ///
    /// The already-initialized probe at the top of this function checks only
    /// the well-known root HAS via `fetch_root_has`. On a half-initialized
    /// archive that probe returns `Err`, so a retry will **not** see
    /// `ArchiveAlreadyInitialized` — instead it will re-upload the
    /// pseudo-checkpoint (a byte-identical overwrite of the empty HAS) and
    /// then upload the root HAS, completing initialization. This behavior is
    /// **self-healing**: both writes are idempotent, and a retry recovers the
    /// archive without operator intervention.
    ///
    /// Probing only the root HAS matches stellar-core's
    /// `HistoryArchiveManager::initializeHistoryArchive`
    /// (`HistoryArchiveManager.cpp:213–220`), which calls
    /// `GetHistoryArchiveStateWork(seq=0, ...)` — `isWellKnown(0)`
    /// (`GetHistoryArchiveStateWork.cpp:33`) routes seq=0 to the well-known
    /// root HAS only, not the §4.3 pseudo-checkpoint. We deliberately do not
    /// add a second probe for the pseudo-checkpoint: that would diverge from
    /// upstream operator-facing behavior and offer no benefit, since the
    /// retry path already recovers safely.
    ///
    /// **Operator note:** self-healing is a safety net, not a license to
    /// ignore failures. The transient error that caused the half-init (e.g.
    /// a `cp`/`s3 put` failure on the second upload) should still be
    /// investigated — it may indicate an underlying problem with the remote
    /// (auth, quota, network) that will recur during normal publish.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use henyey_history::HistoryArchiveManager;
    /// # async fn example(manager: &HistoryArchiveManager) -> Result<(), henyey_history::HistoryError> {
    /// manager.initialize_history_archive("my-new-archive").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn initialize_history_archive(&self, name: &str) -> Result<()> {
        use crate::archive_state::{HASBucketLevel, HASBucketNext};

        let entry = self.get_archive(name)?;

        // Check if already initialized by trying to fetch the root HAS
        if let Some(ref archive) = entry.archive {
            if archive.fetch_root_has().await.is_ok() {
                return Err(HistoryError::ArchiveAlreadyInitialized(name.to_string()));
            }
        }

        // Get the remote archive for writing
        let remote = entry
            .remote
            .as_ref()
            .ok_or_else(|| HistoryError::ArchiveNotWritable(name.to_string()))?;

        if !remote.can_write() {
            return Err(HistoryError::ArchiveNotWritable(name.to_string()));
        }

        // Create an empty HAS matching stellar-core's default ctor
        // (HistoryArchive.cpp): version = 1, no hot-archive buckets,
        // all-zero curr/snap on every live-bucket level.
        let zero_hash = henyey_common::Hash256::from_bytes([0u8; 32]);
        let empty_level = HASBucketLevel {
            curr: zero_hash.to_hex(),
            snap: zero_hash.to_hex(),
            next: HASBucketNext::default(),
        };

        let has = HistoryArchiveState {
            version: 1,
            server: Some(henyey_common::version::build_version_string(env!(
                "CARGO_PKG_VERSION"
            ))),
            current_ledger: 0,
            network_passphrase: Some(self.network_passphrase.clone()),
            current_buckets: vec![empty_level; henyey_bucket::BUCKET_LIST_LEVELS],
            hot_archive_buckets: None,
        };

        // Serialize to JSON
        let json = serde_json::to_string_pretty(&has)?;

        // Write to a temp file once; `put_file_with_mkdir` only reads the
        // local path (it shells out via `execute_command`) and does not
        // consume the file, so we can upload the same tempfile twice.
        let temp_file = tempfile::NamedTempFile::new()?;
        std::fs::write(temp_file.path(), &json)?;

        // Upload to the §4.3 pseudo-checkpoint path first, then to the
        // well-known root HAS path. The two writes are independent and the
        // final archive state is identical regardless of order. Stellar-core
        // schedules these as two separate `WorkSequence`s in
        // `PutHistoryArchiveStateWork::spawnPublishWork`
        // (PutHistoryArchiveStateWork.cpp:69), which the scheduler may run
        // concurrently — so ordering here is not a parity-observable
        // property.
        remote
            .put_file_with_mkdir(temp_file.path(), paths::pseudo_checkpoint_has_path())
            .await?;

        // Upload to the well-known root HAS path.
        remote
            .put_file_with_mkdir(temp_file.path(), paths::root_has_path())
            .await?;

        tracing::info!(
            archive = %name,
            "Successfully initialized history archive"
        );

        Ok(())
    }

    /// Get the root HAS from any readable archive.
    ///
    /// Tries each readable archive in sequence until one succeeds.
    pub async fn fetch_root_has(&self) -> Result<HistoryArchiveState> {
        for entry in &self.archives {
            if let Some(ref archive) = entry.archive {
                match archive.fetch_root_has().await {
                    Ok(has) => return Ok(has),
                    Err(e) => {
                        tracing::warn!(
                            archive = %entry.name,
                            error = %e,
                            "Failed to get HAS from archive"
                        );
                        continue;
                    }
                }
            }
        }
        Err(HistoryError::NoArchiveAvailable)
    }
}

#[cfg(test)]
mod archive_manager_tests {
    use super::*;
    use remote_archive::RemoteArchiveConfig;

    #[test]
    fn test_archive_entry_read_only() {
        let archive = HistoryArchive::new("https://example.com").unwrap();
        let entry = ArchiveEntry::read_only("test".to_string(), archive);

        assert!(entry.can_read());
        assert!(!entry.can_write());
        assert!(!entry.is_fully_configured());
    }

    #[test]
    fn test_archive_entry_write_only() {
        let config = RemoteArchiveConfig {
            name: "test".to_string(),
            put_cmd: Some("echo {0} {1}".to_string()),
            mkdir_cmd: None,
            get_cmd: None,
        };
        let remote = RemoteArchive::new(config);
        let entry = ArchiveEntry::write_only("test".to_string(), remote);

        assert!(!entry.can_read());
        assert!(entry.can_write());
        assert!(!entry.is_fully_configured());
    }

    #[test]
    fn test_archive_entry_fully_configured() {
        let archive = HistoryArchive::new("https://example.com").unwrap();
        let config = RemoteArchiveConfig {
            name: "test".to_string(),
            put_cmd: Some("echo {0} {1}".to_string()),
            mkdir_cmd: None,
            get_cmd: Some("curl {0} -o {1}".to_string()),
        };
        let remote = RemoteArchive::new(config);
        let entry = ArchiveEntry::new("test".to_string(), Some(archive), Some(remote));

        assert!(entry.can_read());
        assert!(entry.can_write());
        assert!(entry.is_fully_configured());
    }

    #[test]
    fn test_manager_publish_enabled_no_archives() {
        let manager = HistoryArchiveManager::new("test passphrase".to_string());
        assert!(!manager.publish_enabled());
    }

    #[test]
    fn test_manager_publish_enabled_read_only() {
        let mut manager = HistoryArchiveManager::new("test passphrase".to_string());
        let archive = HistoryArchive::new("https://example.com").unwrap();
        manager.add_archive(ArchiveEntry::read_only("test".to_string(), archive));

        assert!(!manager.publish_enabled());
        assert_eq!(manager.get_writable_archives().len(), 0);
        assert_eq!(manager.get_readable_archives().len(), 1);
    }

    #[test]
    fn test_manager_publish_enabled_with_writable() {
        let mut manager = HistoryArchiveManager::new("test passphrase".to_string());

        // Add a read-only archive
        let archive1 = HistoryArchive::new("https://example1.com").unwrap();
        manager.add_archive(ArchiveEntry::read_only("read-only".to_string(), archive1));

        // Add a fully configured archive
        let archive2 = HistoryArchive::new("https://example2.com").unwrap();
        let config = RemoteArchiveConfig {
            name: "writable".to_string(),
            put_cmd: Some("echo {0} {1}".to_string()),
            mkdir_cmd: None,
            get_cmd: Some("curl {0} -o {1}".to_string()),
        };
        let remote = RemoteArchive::new(config);
        manager.add_archive(ArchiveEntry::new(
            "writable".to_string(),
            Some(archive2),
            Some(remote),
        ));

        assert!(manager.publish_enabled());
        assert_eq!(manager.get_writable_archives().len(), 1);
        assert_eq!(manager.get_readable_archives().len(), 2);
    }

    #[test]
    fn test_manager_get_archive() {
        let mut manager = HistoryArchiveManager::new("test passphrase".to_string());
        let archive = HistoryArchive::new("https://example.com").unwrap();
        manager.add_archive(ArchiveEntry::read_only("test-archive".to_string(), archive));

        assert!(manager.get_archive("test-archive").is_ok());
        assert!(manager.get_archive("nonexistent").is_err());
    }

    #[test]
    fn test_manager_check_sensible_config_empty() {
        let manager = HistoryArchiveManager::new("test passphrase".to_string());
        // No readable archives is not sensible
        assert!(!manager.check_sensible_config());
    }

    #[test]
    fn test_manager_check_sensible_config_read_only() {
        let mut manager = HistoryArchiveManager::new("test passphrase".to_string());
        let archive = HistoryArchive::new("https://example.com").unwrap();
        manager.add_archive(ArchiveEntry::read_only("test".to_string(), archive));

        // Read-only is sensible (can do catchup, just not publish)
        assert!(manager.check_sensible_config());
    }

    #[test]
    fn test_manager_check_sensible_config_inert() {
        let mut manager = HistoryArchiveManager::new("test passphrase".to_string());

        // Add an inert archive (no read or write)
        let entry = ArchiveEntry {
            name: "inert".to_string(),
            archive: None,
            remote: None,
        };
        manager.add_archive(entry);

        // Inert archive is not sensible
        assert!(!manager.check_sensible_config());
    }

    /// Build a write-only `HistoryArchiveManager` whose put/mkdir commands
    /// target a local filesystem tempdir. Returns the manager, the tempdir
    /// (kept alive by the caller), and the archive name.
    fn make_filesystem_manager(
        tmp: &tempfile::TempDir,
        passphrase: &str,
    ) -> (HistoryArchiveManager, &'static str) {
        let root = tmp.path().to_string_lossy().into_owned();
        let put_cmd = format!("cp {{0}} {root}/{{1}}");
        let mkdir_cmd = format!("mkdir -p {root}/{{0}}");

        let config = RemoteArchiveConfig {
            name: "test-fs".to_string(),
            put_cmd: Some(put_cmd),
            mkdir_cmd: Some(mkdir_cmd),
            get_cmd: None,
        };
        let remote = RemoteArchive::new(config);
        let mut manager = HistoryArchiveManager::new(passphrase.to_string());
        manager.add_archive(ArchiveEntry::write_only("test-fs".to_string(), remote));
        (manager, "test-fs")
    }

    #[tokio::test]
    async fn test_initialize_history_archive_writes_pseudo_checkpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let (manager, name) = make_filesystem_manager(&tmp, "test passphrase");

        manager.initialize_history_archive(name).await.unwrap();

        let pseudo = tmp.path().join("history/00/00/00/history-00000000.json");
        assert!(
            pseudo.exists(),
            "expected pseudo-checkpoint at {}",
            pseudo.display()
        );
    }

    #[tokio::test]
    async fn test_initialize_history_archive_writes_root_has() {
        let tmp = tempfile::tempdir().unwrap();
        let (manager, name) = make_filesystem_manager(&tmp, "test passphrase");

        manager.initialize_history_archive(name).await.unwrap();

        let root = tmp.path().join(".well-known/stellar-history.json");
        assert!(root.exists(), "expected root HAS at {}", root.display());
    }

    #[tokio::test]
    async fn test_initialize_history_archive_files_match() {
        let tmp = tempfile::tempdir().unwrap();
        let (manager, name) = make_filesystem_manager(&tmp, "test passphrase");

        manager.initialize_history_archive(name).await.unwrap();

        let pseudo_bytes =
            std::fs::read(tmp.path().join("history/00/00/00/history-00000000.json")).unwrap();
        let root_bytes =
            std::fs::read(tmp.path().join(".well-known/stellar-history.json")).unwrap();

        assert_eq!(
            pseudo_bytes, root_bytes,
            "pseudo-checkpoint and root HAS must contain identical bytes"
        );
    }

    #[tokio::test]
    async fn test_initialize_history_archive_pseudo_checkpoint_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let passphrase = "test passphrase";
        let (manager, name) = make_filesystem_manager(&tmp, passphrase);

        manager.initialize_history_archive(name).await.unwrap();

        let pseudo_path = tmp.path().join("history/00/00/00/history-00000000.json");
        let json = std::fs::read_to_string(&pseudo_path).unwrap();
        let has: HistoryArchiveState = serde_json::from_str(&json).unwrap();

        assert_eq!(has.version, 1, "empty HAS must use version 1");
        assert_eq!(has.current_ledger, 0);
        let expected_server =
            henyey_common::version::build_version_string(env!("CARGO_PKG_VERSION"));
        assert!(
            has.server.as_deref().unwrap().starts_with("henyey-v"),
            "server must start with henyey-v, got: {:?}",
            has.server
        );
        assert_eq!(has.server.as_deref(), Some(expected_server.as_str()));
        assert_eq!(has.network_passphrase.as_deref(), Some(passphrase));
        assert!(
            has.hot_archive_buckets.is_none(),
            "version-1 HAS must omit hot_archive_buckets"
        );

        // Confirm the field is omitted from the JSON entirely (not just null).
        assert!(
            !json.contains("hotArchiveBuckets"),
            "JSON must omit hotArchiveBuckets field, got: {json}"
        );

        assert_eq!(
            has.current_buckets.len(),
            henyey_bucket::BUCKET_LIST_LEVELS,
            "current_buckets must have one entry per live bucket level"
        );
        let zero_hex = henyey_common::Hash256::from_bytes([0u8; 32]).to_hex();
        for (i, level) in has.current_buckets.iter().enumerate() {
            assert_eq!(level.curr, zero_hex, "level {i} curr must be all-zero");
            assert_eq!(level.snap, zero_hex, "level {i} snap must be all-zero");
        }
    }

    #[tokio::test]
    async fn test_initialize_history_archive_already_initialized() {
        let tmp = tempfile::tempdir().unwrap();
        let passphrase = "test passphrase";

        // Pre-create the well-known root HAS so the fetch_root_has probe succeeds.
        let well_known_dir = tmp.path().join(".well-known");
        std::fs::create_dir_all(&well_known_dir).unwrap();
        let existing = b"{\"version\":1,\"server\":\"preexisting\",\"currentLedger\":0,\"networkPassphrase\":\"x\",\"currentBuckets\":[]}";
        std::fs::write(well_known_dir.join("stellar-history.json"), existing).unwrap();

        // Manager with both read (file://) and write (cp) configured.
        let root = tmp.path().to_string_lossy().into_owned();
        let put_cmd = format!("cp {{0}} {root}/{{1}}");
        let mkdir_cmd = format!("mkdir -p {root}/{{0}}");
        let get_cmd = format!("cp {root}/{{0}} {{1}}");

        let archive = HistoryArchive::new(&format!("file://{root}")).unwrap();
        let remote_config = RemoteArchiveConfig {
            name: "test-fs".to_string(),
            put_cmd: Some(put_cmd),
            mkdir_cmd: Some(mkdir_cmd),
            get_cmd: Some(get_cmd),
        };
        let remote = RemoteArchive::new(remote_config);

        let mut manager = HistoryArchiveManager::new(passphrase.to_string());
        manager.add_archive(ArchiveEntry::new(
            "test-fs".to_string(),
            Some(archive),
            Some(remote),
        ));

        let err = manager
            .initialize_history_archive("test-fs")
            .await
            .expect_err("should refuse to overwrite an initialized archive");
        assert!(
            matches!(err, HistoryError::ArchiveAlreadyInitialized(_)),
            "expected ArchiveAlreadyInitialized, got {err:?}"
        );

        // The pseudo-checkpoint must NOT have been created.
        let pseudo = tmp.path().join("history/00/00/00/history-00000000.json");
        assert!(
            !pseudo.exists(),
            "init must not write pseudo-checkpoint when archive is already initialized"
        );
    }

    #[tokio::test]
    async fn test_initialize_history_archive_recovers_from_half_initialized() {
        // Simulates a half-initialized archive: the §4.3 pseudo-checkpoint
        // exists (left over from a previous attempt that crashed between
        // the two uploads), but the well-known root HAS does not. A retry
        // must self-heal: it overwrites the pseudo-checkpoint with
        // byte-identical content and then writes the root HAS.
        let tmp = tempfile::tempdir().unwrap();
        let passphrase = "test passphrase";

        // Pre-create only the pseudo-checkpoint with arbitrary bytes — the
        // retry must overwrite this with the canonical empty HAS.
        let pseudo_dir = tmp.path().join("history/00/00/00");
        std::fs::create_dir_all(&pseudo_dir).unwrap();
        let pseudo_path = pseudo_dir.join("history-00000000.json");
        std::fs::write(&pseudo_path, b"{\"stale\":\"half-init leftover\"}").unwrap();

        // Manager with both read (file://) and write (cp) configured — required
        // so the `fetch_root_has` probe at the top of initialize_history_archive
        // actually runs. Without a read side, the probe is skipped entirely
        // and the retry path is not exercised.
        let root = tmp.path().to_string_lossy().into_owned();
        let put_cmd = format!("cp {{0}} {root}/{{1}}");
        let mkdir_cmd = format!("mkdir -p {root}/{{0}}");
        let get_cmd = format!("cp {root}/{{0}} {{1}}");

        let archive = HistoryArchive::new(&format!("file://{root}")).unwrap();
        let remote_config = RemoteArchiveConfig {
            name: "test-fs".to_string(),
            put_cmd: Some(put_cmd),
            mkdir_cmd: Some(mkdir_cmd),
            get_cmd: Some(get_cmd),
        };
        let remote = RemoteArchive::new(remote_config);

        let mut manager = HistoryArchiveManager::new(passphrase.to_string());
        manager.add_archive(ArchiveEntry::new(
            "test-fs".to_string(),
            Some(archive),
            Some(remote),
        ));

        // Sanity: the root HAS is missing, so fetch_root_has must fail and
        // the function must not short-circuit with ArchiveAlreadyInitialized.
        manager
            .initialize_history_archive("test-fs")
            .await
            .expect("retry must recover from a half-initialized archive");

        // Both files must now exist.
        let root_path = tmp.path().join(".well-known/stellar-history.json");
        assert!(
            pseudo_path.exists(),
            "pseudo-checkpoint must still exist after recovery"
        );
        assert!(
            root_path.exists(),
            "root HAS must be written by the recovery retry"
        );

        // Both files must be byte-identical (idempotent overwrite).
        let pseudo_bytes = std::fs::read(&pseudo_path).unwrap();
        let root_bytes = std::fs::read(&root_path).unwrap();
        assert_eq!(
            pseudo_bytes, root_bytes,
            "after recovery, pseudo-checkpoint and root HAS must contain identical bytes"
        );

        // The pre-existing stale bytes must have been overwritten with the
        // canonical empty HAS (version=1, server=henyey-v<version>, ...).
        let has: HistoryArchiveState = serde_json::from_slice(&pseudo_bytes).unwrap();
        assert_eq!(has.version, 1);
        assert_eq!(has.current_ledger, 0);
        let expected_server =
            henyey_common::version::build_version_string(env!("CARGO_PKG_VERSION"));
        assert!(
            has.server.as_deref().unwrap().starts_with("henyey-v"),
            "server must start with henyey-v, got: {:?}",
            has.server
        );
        assert_eq!(has.server.as_deref(), Some(expected_server.as_str()));
        assert_eq!(has.network_passphrase.as_deref(), Some(passphrase));
        assert!(has.hot_archive_buckets.is_none());
        assert_eq!(
            has.current_buckets.len(),
            henyey_bucket::BUCKET_LIST_LEVELS,
            "recovered HAS must have one entry per live bucket level"
        );
    }
}
