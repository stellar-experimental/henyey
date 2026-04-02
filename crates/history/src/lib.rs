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
//! let has = archive.get_root_has().await?;
//! println!("Current ledger: {}", has.current_ledger());
//!
//! // Get all bucket hashes needed for catchup
//! let buckets = has.all_bucket_hashes();
//! println!("Need {} buckets", buckets.len());
//!
//! // Download ledger headers for a checkpoint
//! let headers = archive.get_ledger_headers(63).await?;
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

// Publishing and external data sources
pub mod cdp;
pub mod checkpoint_builder;
pub mod publish;
pub mod publish_queue;
pub mod remote_archive;

// Error handling
pub mod error;

// Re-export main types at crate root
pub use archive::HistoryArchive;
pub use archive_state::HistoryArchiveState;
pub use archive_state::MAX_HISTORY_ARCHIVE_BUCKET_SIZE;
pub use catchup::{
    CatchupManager, CatchupOptions, CatchupProgress, CatchupStatus, CheckpointData,
    ExistingBucketState, LedgerData,
};
pub use catchup_range::{CatchupMode, CatchupRange, LedgerRange, GENESIS_LEDGER_SEQ};
pub use cdp::{
    extract_ledger_header, extract_transaction_envelopes, extract_transaction_metas, CacheStats,
    CachedCdpDataLake, CdpDataLake,
};
pub use checkpoint::{
    checkpoint_containing, checkpoint_frequency, first_ledger_in_checkpoint_containing,
    is_checkpoint_ledger, last_ledger_before_checkpoint_containing, latest_checkpoint_before_or_at,
    ledger_to_trigger_catchup, size_of_checkpoint_containing, CHECKPOINT_FREQUENCY,
};
pub use download::DownloadConfig;
pub use error::HistoryError;
pub use paths::{
    bucket_path, checkpoint_ledger, checkpoint_path, checkpoint_path_dirty, dirty_to_final_path,
    final_to_dirty_path, is_dirty_path,
};
pub use paths::{
    set_checkpoint_frequency, ACCELERATED_CHECKPOINT_FREQUENCY, DEFAULT_CHECKPOINT_FREQUENCY,
};
pub use publish::build_history_archive_state;
pub use publish_queue::{
    PublishQueue, PublishQueueStats, PUBLISH_QUEUE_MAX_SIZE, PUBLISH_QUEUE_UNBLOCK_APPLICATION,
};
pub use remote_archive::{RemoteArchive, RemoteArchiveConfig};
pub use replay::{ReplayConfig, ReplayedLedgerState};
pub use verify::{
    compute_header_hash, verify_bucket_hash, verify_chain_anchors, verify_has_passphrase,
    verify_header_chain, verify_tx_result_ordering, ChainTrustAnchors,
};

/// Result type for history operations.
pub type Result<T> = std::result::Result<T, HistoryError>;

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
/// let has = manager.get_root_has().await?;
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
    pub async fn get_root_has(&self) -> Result<HistoryArchiveState> {
        self.try_archives(
            |a| Box::pin(a.get_root_has()),
            "get HAS",
            HistoryError::NoArchiveAvailable,
        )
        .await
    }

    /// Get the checkpoint HAS from any available archive.
    pub async fn get_checkpoint_has(&self, ledger: u32) -> Result<HistoryArchiveState> {
        self.try_archives(
            |a| Box::pin(a.get_checkpoint_has(ledger)),
            "get checkpoint HAS",
            HistoryError::CheckpointNotFound(ledger),
        )
        .await
    }

    /// Download a bucket from any available archive.
    pub async fn get_bucket(&self, hash: &henyey_common::Hash256) -> Result<Vec<u8>> {
        let hash = *hash;
        self.try_archives(
            |a| Box::pin(a.get_bucket(&hash)),
            "get bucket",
            HistoryError::BucketNotFound(hash),
        )
        .await
    }

    /// Get ledger headers for a checkpoint from any available archive.
    pub async fn get_ledger_headers(
        &self,
        checkpoint: u32,
    ) -> Result<Vec<stellar_xdr::curr::LedgerHeaderHistoryEntry>> {
        self.try_archives(
            |a| Box::pin(a.get_ledger_headers(checkpoint)),
            "get ledger headers",
            HistoryError::CheckpointNotFound(checkpoint),
        )
        .await
    }

    /// Get transactions for a checkpoint from any available archive.
    pub async fn get_transactions(
        &self,
        checkpoint: u32,
    ) -> Result<Vec<stellar_xdr::curr::TransactionHistoryEntry>> {
        self.try_archives(
            |a| Box::pin(a.get_transactions(checkpoint)),
            "get transactions",
            HistoryError::CheckpointNotFound(checkpoint),
        )
        .await
    }

    /// Get transaction results for a checkpoint from any available archive.
    pub async fn get_results(
        &self,
        checkpoint: u32,
    ) -> Result<Vec<stellar_xdr::curr::TransactionHistoryResultEntry>> {
        self.try_archives(
            |a| Box::pin(a.get_results(checkpoint)),
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
    /// This creates a new archive by writing an empty `HistoryArchiveState`
    /// to `.well-known/stellar-history.json`.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The archive is not found by name
    /// - The archive is already initialized (HAS exists)
    /// - The archive is not writable (no put command)
    /// - The upload fails
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
            if archive.get_root_has().await.is_ok() {
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

        // Create an empty HAS with resolved (cleared) futures
        // All bucket levels have empty curr/snap and cleared next state
        let zero_hash = henyey_common::Hash256::from_bytes([0u8; 32]);
        let empty_level = HASBucketLevel {
            curr: zero_hash.to_hex(),
            snap: zero_hash.to_hex(),
            next: HASBucketNext {
                state: 0, // FB_CLEAR
                output: None,
                curr: None,
                snap: None,
                shadow: None,
            },
        };

        let has = HistoryArchiveState {
            version: 2,
            server: Some("rs-stellar-core".to_string()),
            current_ledger: 0,
            network_passphrase: Some(self.network_passphrase.clone()),
            current_buckets: vec![empty_level.clone(); 11],
            hot_archive_buckets: None,
        };

        // Serialize to JSON
        let json = serde_json::to_string_pretty(&has)?;

        // Write to a temp file
        let temp_file = tempfile::NamedTempFile::new()?;
        std::fs::write(temp_file.path(), &json)?;

        // Upload to .well-known/stellar-history.json
        remote
            .put_file_with_mkdir(temp_file.path(), ".well-known/stellar-history.json")
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
    pub async fn get_root_has(&self) -> Result<HistoryArchiveState> {
        for entry in &self.archives {
            if let Some(ref archive) = entry.archive {
                match archive.get_root_has().await {
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
}
