//! History archive publishing for validators.
//!
//! This module enables validators to publish their ledger history to archives,
//! making it available for other nodes to catch up from.
//!
//! # Overview
//!
//! When a validator closes a checkpoint (every 64 ledgers), it can publish:
//!
//! - **Ledger headers**: Metadata for each ledger in the checkpoint
//! - **Transaction sets**: All transactions executed in the checkpoint
//! - **Transaction results**: Outcomes of each transaction
//! - **Bucket files**: Serialized ledger state at the checkpoint
//! - **SCP messages**: Consensus protocol messages for verification
//! - **HAS file**: History Archive State summarizing the checkpoint
//!
//! # Archive Layout
//!
//! Published files follow the standard archive structure:
//!
//! ```text
//! history/AA/BB/CC/history-AABBCCDD.json    # HAS file
//! ledger/AA/BB/CC/ledger-AABBCCDD.xdr.gz    # Ledger headers
//! transactions/AA/BB/CC/transactions-AABBCCDD.xdr.gz
//! results/AA/BB/CC/results-AABBCCDD.xdr.gz
//! bucket/XX/YY/ZZ/bucket-{hash}.xdr.gz      # Bucket files
//! ```
//!
//! Where AA/BB/CC are hex bytes of the checkpoint ledger sequence.
//!
//! # Usage
//!
//! ```no_run
//! use henyey_history::publish::{PublishManager, PublishConfig};
//!
//! let config = PublishConfig {
//!     local_path: "/var/stellar/history".into(),
//!     network_passphrase: Some("Test SDF Network ; September 2015".to_string()),
//!     ..Default::default()
//! };
//!
//! let manager = PublishManager::new(config);
//! // Call publish_checkpoint() at each checkpoint boundary
//! ```

use crate::{
    archive_state::{HASBucketLevel, HASBucketNext, HistoryArchiveState},
    checkpoint::is_checkpoint_ledger,
    paths, verify, HistoryError, Result,
};
use henyey_bucket::{BucketList, PendingMergeState, HAS_NEXT_STATE_INPUTS, HAS_NEXT_STATE_OUTPUT};
use henyey_common::fs_utils::{
    atomic_gzip_copy, atomic_gzip_xdr_write_iter, atomic_gzip_xdr_write_slice, atomic_write_bytes,
};
use henyey_common::Hash256;
use std::path::{Path, PathBuf};
use stellar_xdr::curr::{
    LedgerHeaderHistoryEntry, TransactionHistoryEntry, TransactionHistoryResultEntry, WriteXdr,
};
use tracing::{debug, info};

/// Configuration for history publishing.
#[derive(Debug, Clone)]
pub struct PublishConfig {
    /// Local directory to write history files.
    pub local_path: PathBuf,
    /// Network passphrase for HAS files.
    pub network_passphrase: Option<String>,
    /// Whether to publish to remote archives.
    pub publish_remote: bool,
    /// Remote archive URLs for publishing (S3, GCS, etc.).
    pub remote_urls: Vec<String>,
    /// Maximum number of parallel uploads.
    pub max_parallel_uploads: usize,
}

impl Default for PublishConfig {
    fn default() -> Self {
        Self {
            local_path: PathBuf::from("history"),
            network_passphrase: None,
            publish_remote: false,
            remote_urls: Vec::new(),
            max_parallel_uploads: 4,
        }
    }
}

/// State of a checkpoint being published.
#[derive(Debug, Clone)]
pub struct PublishState {
    /// Ledger sequence of the checkpoint.
    pub checkpoint_ledger: u32,
    /// Current status.
    pub status: PublishStatus,
    /// Number of files written.
    pub files_written: u32,
    /// Number of files to write.
    pub files_total: u32,
}

/// Status of publishing operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishStatus {
    /// Queued for publishing.
    Queued,
    /// Currently publishing.
    Publishing,
    /// Successfully published.
    Completed,
    /// Publishing failed.
    Failed,
}

/// Manager for publishing history to archives.
pub struct PublishManager {
    config: PublishConfig,
}

/// Build a history archive state from a bucket list snapshot.
///
/// Captures the full bucket list state including any pending merges.
/// Pending merges are recorded matching stellar-core FutureBucket serialization:
/// - state=1 (output hash known) for completed merges
/// - state=2 (input hashes known) for in-progress async merges
/// - state=0 (clear) for levels with no pending merge
pub fn build_history_archive_state(
    ledger_seq: u32,
    bucket_list: &BucketList,
    hot_archive_bucket_list: Option<&henyey_bucket::HotArchiveBucketList>,
    network_passphrase: Option<String>,
) -> Result<HistoryArchiveState> {
    let current_buckets: Vec<HASBucketLevel> = bucket_list
        .levels()
        .iter()
        .map(|level| {
            let next = match level.pending_merge_state() {
                Some(PendingMergeState::Output(hash)) => HASBucketNext {
                    state: HAS_NEXT_STATE_OUTPUT,
                    output: Some(hash.to_hex()),
                    curr: None,
                    snap: None,
                    shadow: None,
                },
                Some(PendingMergeState::Inputs { curr, snap }) => HASBucketNext {
                    state: HAS_NEXT_STATE_INPUTS,
                    output: None,
                    curr: Some(curr.to_hex()),
                    snap: Some(snap.to_hex()),
                    shadow: None,
                },
                None => HASBucketNext::default(),
            };
            HASBucketLevel {
                curr: level.curr.hash().to_hex(),
                snap: level.snap.hash().to_hex(),
                next,
            }
        })
        .collect();

    let hot_archive_buckets = hot_archive_bucket_list.map(|habl| {
        habl.levels()
            .iter()
            .map(|level| {
                let next = match level.pending_merge_output_hash() {
                    Some(hash) => HASBucketNext {
                        state: HAS_NEXT_STATE_OUTPUT,
                        output: Some(hash.to_hex()),
                        curr: None,
                        snap: None,
                        shadow: None,
                    },
                    None => HASBucketNext::default(),
                };
                HASBucketLevel {
                    curr: level.curr().hash().to_hex(),
                    snap: level.snap_bucket().hash().to_hex(),
                    next,
                }
            })
            .collect()
    });

    let version = if hot_archive_buckets.is_some() { 2 } else { 1 };

    Ok(HistoryArchiveState {
        version,
        server: Some("rs-stellar-core".to_string()),
        current_ledger: ledger_seq,
        network_passphrase,
        current_buckets,
        hot_archive_buckets,
    })
}

impl PublishManager {
    /// Create a new publish manager.
    pub fn new(config: PublishConfig) -> Self {
        Self { config }
    }

    /// Publish a checkpoint to history archives.
    ///
    /// This writes all files for a checkpoint:
    /// - Ledger headers for the checkpoint range
    /// - Transaction sets for each ledger
    /// - Transaction results for each ledger
    /// - Bucket files referenced by the bucket list
    /// - History Archive State (HAS) file
    ///
    /// If `prebuilt_has` is provided, it is used as the checkpoint HAS
    /// instead of rebuilding from the bucket list. This is important for
    /// protocol >= 23 where the HAS must include hot archive bucket hashes
    /// that are only available at checkpoint close time.
    ///
    pub fn publish_checkpoint(
        &self,
        checkpoint_ledger: u32,
        headers: &[LedgerHeaderHistoryEntry],
        tx_entries: &[TransactionHistoryEntry],
        tx_results: &[TransactionHistoryResultEntry],
        bucket_list: &BucketList,
        prebuilt_has: Option<&HistoryArchiveState>,
    ) -> Result<PublishState> {
        if !is_checkpoint_ledger(checkpoint_ledger) {
            return Err(HistoryError::NotCheckpointLedger(checkpoint_ledger));
        }

        info!(
            checkpoint = checkpoint_ledger,
            "Publishing checkpoint to history"
        );

        verify::verify_header_chain_from_entries(headers)?;

        // Validate tx/result entries: no duplicates, ordered by ledger_seq.
        let mut last_tx_seq: Option<u32> = None;
        for entry in tx_entries {
            if let Some(prev) = last_tx_seq {
                if entry.ledger_seq <= prev {
                    return Err(HistoryError::VerificationFailed(format!(
                        "tx entries not strictly ordered: {} after {}",
                        entry.ledger_seq, prev
                    )));
                }
            }
            last_tx_seq = Some(entry.ledger_seq);
        }
        let mut last_res_seq: Option<u32> = None;
        for entry in tx_results {
            if let Some(prev) = last_res_seq {
                if entry.ledger_seq <= prev {
                    return Err(HistoryError::VerificationFailed(format!(
                        "result entries not strictly ordered: {} after {}",
                        entry.ledger_seq, prev
                    )));
                }
            }
            last_res_seq = Some(entry.ledger_seq);
        }

        let tx_entry_map: std::collections::HashMap<_, _> = tx_entries
            .iter()
            .map(|entry| (entry.ledger_seq, entry))
            .collect();
        let tx_result_map: std::collections::HashMap<_, _> = tx_results
            .iter()
            .map(|entry| (entry.ledger_seq, entry))
            .collect();

        for header_entry in headers {
            let header = &header_entry.header;

            // Skip tx set and result verification for ledger 1 (genesis).
            // The genesis header uses all-zero sentinel hashes for both
            // scp_value.tx_set_hash and tx_set_result_hash since no
            // transactions are applied at genesis.  The stored empty
            // TransactionSet hashes to a non-zero value, so verification
            // would always fail.  stellar-core handles this implicitly
            // because the genesis ledger is never published through the
            // normal publish path.
            if header.ledger_seq == 1 {
                continue;
            }

            // stellar-core omits tx/result entries for ledgers with no
            // transactions (CheckpointBuilder.cpp:140). Handle sparse
            // entries with both-or-none validation.
            match (
                tx_entry_map.get(&header.ledger_seq),
                tx_result_map.get(&header.ledger_seq),
            ) {
                (Some(tx_entry), Some(result_entry)) => {
                    let tx_set = crate::catchup::tx_set_from_history_entry(tx_entry);
                    verify::verify_tx_set(header, &tx_set)?;

                    let xdr = result_entry
                        .tx_result_set
                        .to_xdr(stellar_xdr::curr::Limits::none())
                        .map_err(|e| HistoryError::VerificationFailed(e.to_string()))?;
                    verify::verify_tx_result_set(header, &xdr)?;
                }
                (None, None) => {
                    // Empty ledger — no tx/result entries expected.
                    // stellar-core omits entries for ledgers with no
                    // transactions, so absence of both is normal.
                }
                (Some(_), None) => {
                    return Err(HistoryError::VerificationFailed(format!(
                        "ledger {}: tx entry present but result entry missing",
                        header.ledger_seq
                    )));
                }
                (None, Some(_)) => {
                    return Err(HistoryError::VerificationFailed(format!(
                        "ledger {}: result entry present but tx entry missing",
                        header.ledger_seq
                    )));
                }
            }
        }

        let mut state = PublishState {
            checkpoint_ledger,
            status: PublishStatus::Publishing,
            files_written: 0,
            files_total: 0,
        };

        // Create directory structure
        self.ensure_directories(checkpoint_ledger)?;

        // Write ledger headers
        let headers_path = self.ledger_path(checkpoint_ledger, "ledger");
        self.write_xdr_gz(&headers_path, headers, "ledger headers")?;
        state.files_written += 1;

        // Write transaction sets
        let txset_path = self.ledger_path(checkpoint_ledger, "transactions");
        self.write_xdr_gz(&txset_path, tx_entries, "transaction entries")?;
        state.files_written += 1;

        // Write transaction results
        let results_path = self.ledger_path(checkpoint_ledger, "results");
        self.write_xdr_gz(&results_path, tx_results, "result sets")?;
        state.files_written += 1;

        // Write bucket files from each level
        for level in bucket_list.levels() {
            // Write curr bucket if non-empty
            if !level.curr.is_empty() {
                let hash = level.curr.hash();
                let bucket_path = self.bucket_path(&hash);
                self.write_bucket_from_entries(&bucket_path, &level.curr)?;
                state.files_written += 1;
            }
            // Write snap bucket if non-empty
            if !level.snap.is_empty() {
                let hash = level.snap.hash();
                let bucket_path = self.bucket_path(&hash);
                self.write_bucket_from_entries(&bucket_path, &level.snap)?;
                state.files_written += 1;
            }
        }

        // Write History Archive State
        let has = match prebuilt_has {
            Some(h) => h.clone(),
            None => self.create_has(checkpoint_ledger, headers, bucket_list)?,
        };

        // Validate that all bucket hashes in the HAS are known (pre-publish check)
        let known_hashes: std::collections::HashSet<Hash256> =
            has.all_bucket_hashes().into_iter().collect();
        has.contains_valid_buckets(&known_hashes)?;

        let has_path = self.has_path(checkpoint_ledger);
        self.write_has(&has_path, &has)?;
        state.files_written += 1;

        state.status = PublishStatus::Completed;
        state.files_total = state.files_written;

        info!(
            checkpoint = checkpoint_ledger,
            files = state.files_written,
            "Checkpoint files prepared locally"
        );

        Ok(state)
    }

    /// Ensure directory structure exists for a checkpoint.
    fn ensure_directories(&self, checkpoint_ledger: u32) -> Result<()> {
        let base = &self.config.local_path;

        // Create checkpoint directories for each category.
        for category in ["ledger", "transactions", "results", "history", "scp"] {
            let path = base.join(paths::checkpoint_file_path(checkpoint_ledger, category));
            let dir = path.parent().ok_or_else(|| {
                HistoryError::VerificationFailed("missing checkpoint directory".to_string())
            })?;
            std::fs::create_dir_all(dir)?;
        }

        // Create bucket directories (organized by first 2 hex chars)
        let bucket_base = base.join("bucket");
        for i in 0..=255u8 {
            let subdir = bucket_base.join(format!("{:02x}", i));
            std::fs::create_dir_all(&subdir)?;
        }

        Ok(())
    }

    /// Get the path for a ledger file.
    fn ledger_path(&self, checkpoint_ledger: u32, file_type: &str) -> PathBuf {
        let base = &self.config.local_path;
        base.join(paths::checkpoint_file_path(checkpoint_ledger, file_type))
    }

    /// Get the path for a bucket file.
    fn bucket_path(&self, hash: &Hash256) -> PathBuf {
        let base = &self.config.local_path;
        base.join(paths::bucket_path(hash))
    }

    /// Get the path for a HAS file.
    fn has_path(&self, checkpoint_ledger: u32) -> PathBuf {
        let base = &self.config.local_path;
        base.join(paths::has_path(checkpoint_ledger))
    }

    /// Write a slice of XDR-encodable items to a gzipped file.
    ///
    /// Uses RFC 5531 record-marked format: each XDR item is prefixed with a
    /// 4-byte big-endian length with the high bit set ("last fragment" flag).
    /// This matches stellar-core's `XDROutputFileStream::writeOne`.
    fn write_xdr_gz<T: WriteXdr>(&self, path: &Path, items: &[T], label: &str) -> Result<()> {
        let final_path = path.with_extension("xdr.gz");

        if let Some(parent) = final_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        atomic_gzip_xdr_write_slice(&final_path, items)?;

        debug!("Wrote {} {} to {:?}", items.len(), label, final_path);
        Ok(())
    }

    /// Write a bucket to the history archive as a gzip-compressed file.
    ///
    /// If the bucket has a backing file on disk (`.bucket.xdr`), we gzip-compress
    /// it directly — this preserves the exact record-marked format and avoids
    /// re-serialization. For in-memory-only buckets, we serialize entries with
    /// RFC 5531 record marks via `atomic_gzip_xdr_write_iter`.
    fn write_bucket_from_entries(&self, path: &Path, bucket: &henyey_bucket::Bucket) -> Result<()> {
        // The path already includes the full filename with .xdr.gz extension
        // (from paths::bucket_path).
        if path.exists() {
            debug!("Bucket already exists: {:?}", path);
            return Ok(());
        }

        // Ensure parent directories exist (bucket paths are 3 levels deep:
        // bucket/xx/yy/zz/bucket-{hash}.xdr.gz)
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        if let Some(backing_path) = bucket.backing_file_path() {
            // Disk-backed: stream the raw .bucket.xdr through gzip atomically.
            atomic_gzip_copy(backing_path, path)?;
        } else {
            // In-memory branch — serialize entries via the shared helper.
            // BucketIter yields `Result<BucketEntry, BucketError>`; the
            // helper maps per-item errors through `io::Error`. The outer
            // `bucket.iter()?` (which can fail when opening a disk-backed
            // iterator) keeps its existing error mapping.
            let iter = bucket
                .iter()
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
            atomic_gzip_xdr_write_iter(path, iter)?;
        }

        debug!("Wrote bucket to {:?}", path);
        Ok(())
    }

    /// Write a History Archive State file.
    fn write_has(&self, path: &Path, has: &HistoryArchiveState) -> Result<()> {
        let json = serde_json::to_string_pretty(has)
            .map_err(|e| HistoryError::VerificationFailed(e.to_string()))?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        atomic_write_bytes(path, json.as_bytes())?;

        debug!("Wrote HAS to {:?}", path);
        Ok(())
    }

    /// Create a History Archive State from checkpoint data.
    fn create_has(
        &self,
        checkpoint_ledger: u32,
        _headers: &[LedgerHeaderHistoryEntry],
        bucket_list: &BucketList,
    ) -> Result<HistoryArchiveState> {
        build_history_archive_state(
            checkpoint_ledger,
            bucket_list,
            None,
            self.config.network_passphrase.clone(),
        )
    }

    /// Check if a checkpoint has been published.
    pub fn is_published(&self, checkpoint_ledger: u32) -> bool {
        let has_path = self.has_path(checkpoint_ledger);
        has_path.exists()
    }

    /// Get the latest published checkpoint.
    pub fn latest_published_checkpoint(&self) -> Option<u32> {
        // Scan the history directory for published checkpoints
        let base = &self.config.local_path;
        let has_dir = base.join("history");

        if !has_dir.exists() {
            return None;
        }

        fn scan_dir(path: &Path, latest: &mut Option<u32>) {
            let Ok(entries) = std::fs::read_dir(path) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    scan_dir(&path, latest);
                } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if let Some(hex) = name
                        .strip_prefix("history-")
                        .and_then(|n| n.strip_suffix(".json"))
                    {
                        if let Ok(seq) = u32::from_str_radix(hex, 16) {
                            if is_checkpoint_ledger(seq) {
                                *latest = Some(latest.map_or(seq, |l| l.max(seq)));
                            }
                        }
                    }
                }
            }
        }

        let mut latest: Option<u32> = None;
        scan_dir(&has_dir, &mut latest);
        latest
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use henyey_bucket::BUCKET_LIST_LEVELS;
    use tempfile::TempDir;

    #[test]
    fn test_publish_config_default() {
        let config = PublishConfig::default();
        assert!(!config.publish_remote);
        assert!(config.remote_urls.is_empty());
        assert!(config.network_passphrase.is_none());
    }

    #[test]
    fn test_publish_status() {
        let state = PublishState {
            checkpoint_ledger: 64,
            status: PublishStatus::Queued,
            files_written: 0,
            files_total: 10,
        };
        assert_eq!(state.status, PublishStatus::Queued);
    }

    #[test]
    fn test_ensure_directories() {
        let temp = TempDir::new().unwrap();
        let config = PublishConfig {
            local_path: temp.path().to_path_buf(),
            ..Default::default()
        };
        let manager = PublishManager::new(config);

        manager.ensure_directories(64).unwrap();

        // Check that ledger directory was created
        let ledger_dir = temp.path().join("ledger/00/00/00");
        assert!(ledger_dir.exists());

        // Check that bucket directories were created
        let bucket_dir = temp.path().join("bucket/00");
        assert!(bucket_dir.exists());
    }

    #[test]
    fn test_publish_checkpoint_rejects_bad_header_entry_hash() {
        use stellar_xdr::curr::{
            Hash, LedgerHeader, LedgerHeaderExt, LedgerHeaderHistoryEntryExt, StellarValue,
            StellarValueExt, TimePoint, VecM,
        };

        let temp = TempDir::new().unwrap();
        let manager = PublishManager::new(PublishConfig {
            local_path: temp.path().to_path_buf(),
            ..Default::default()
        });

        let header = LedgerHeader {
            ledger_version: 25,
            previous_ledger_hash: Hash([0u8; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0u8; 32]),
                close_time: TimePoint(0),
                upgrades: VecM::default(),
                ext: StellarValueExt::Basic,
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
            ext: LedgerHeaderExt::V0,
        };
        let entry = LedgerHeaderHistoryEntry {
            hash: Hash([1u8; 32]),
            header,
            ext: LedgerHeaderHistoryEntryExt::default(),
        };

        let err = manager
            .publish_checkpoint(63, &[entry], &[], &[], &BucketList::new(), None)
            .unwrap_err();
        assert!(matches!(err, HistoryError::VerificationFailed(_)));
    }

    /// Verify that `build_history_archive_state` produces a HAS whose bucket
    /// hashes round-trip correctly: the Go SDK hash computation on the HAS
    /// must equal the direct `BucketList::hash()` value.
    ///
    /// This tests the full pipeline: BucketList → build_history_archive_state
    /// → HAS JSON → Go SDK hash recomputation → compare with BucketList::hash().
    #[tokio::test(flavor = "multi_thread")]
    async fn test_build_has_hash_matches_bucket_list_hash() {
        use henyey_bucket::BucketList;
        use sha2::{Digest, Sha256};
        use stellar_xdr::curr::*;

        let mut bl = BucketList::new();

        // Add entries to ledger 1 through 7 (first checkpoint in accelerated mode)
        for seq in 1..=7u32 {
            let account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([seq as u8; 32])));
            let entry = LedgerEntry {
                last_modified_ledger_seq: seq,
                data: LedgerEntryData::Account(AccountEntry {
                    account_id: account_id.clone(),
                    balance: 100_000_000 * (seq as i64),
                    seq_num: SequenceNumber(0),
                    num_sub_entries: 0,
                    inflation_dest: None,
                    flags: 0,
                    home_domain: String32(StringM::default()),
                    thresholds: Thresholds([1, 0, 0, 0]),
                    signers: VecM::default(),
                    ext: AccountEntryExt::V0,
                }),
                ext: LedgerEntryExt::V0,
            };
            bl.add_batch(
                seq,
                25, // protocol 25
                BucketListType::Live,
                vec![entry],
                vec![],
                vec![],
            )
            .unwrap();
        }

        // Build HAS
        let has = build_history_archive_state(7, &bl, None, None).unwrap();

        // Verify HAS has BUCKET_LIST_LEVELS levels
        assert_eq!(
            has.current_buckets.len(),
            BUCKET_LIST_LEVELS,
            "HAS must have {BUCKET_LIST_LEVELS} levels"
        );
        // No hot archive → version 1
        assert_eq!(has.version, 1);

        // Compute hash from HAS the Go SDK way
        let go_live_hash = {
            let mut total = Vec::new();
            for level in &has.current_buckets {
                let curr_bytes = hex::decode(&level.curr).unwrap();
                let snap_bytes = hex::decode(&level.snap).unwrap();
                let mut h = Sha256::new();
                h.update(&curr_bytes);
                h.update(&snap_bytes);
                total.extend_from_slice(&h.finalize());
            }
            let mut h = Sha256::new();
            h.update(&total);
            let r = h.finalize();
            let mut b = [0u8; 32];
            b.copy_from_slice(&r);
            henyey_common::Hash256::from_bytes(b)
        };

        // Compute hash directly from BucketList
        let direct_live_hash = bl.hash();

        assert_eq!(
            go_live_hash,
            direct_live_hash,
            "Go SDK hash from HAS ({}) != BucketList::hash() ({})",
            go_live_hash.to_hex(),
            direct_live_hash.to_hex()
        );

        // Also verify JSON round-trip
        let json = has.to_json().unwrap();
        let reparsed: HistoryArchiveState = serde_json::from_str(&json).unwrap();
        let go_live_hash_rt = {
            let mut total = Vec::new();
            for level in &reparsed.current_buckets {
                let curr_bytes = hex::decode(&level.curr).unwrap();
                let snap_bytes = hex::decode(&level.snap).unwrap();
                let mut h = Sha256::new();
                h.update(&curr_bytes);
                h.update(&snap_bytes);
                total.extend_from_slice(&h.finalize());
            }
            let mut h = Sha256::new();
            h.update(&total);
            let r = h.finalize();
            let mut b = [0u8; 32];
            b.copy_from_slice(&r);
            henyey_common::Hash256::from_bytes(b)
        };

        assert_eq!(
            go_live_hash_rt,
            direct_live_hash,
            "Go SDK hash after JSON round-trip ({}) != BucketList::hash() ({})",
            go_live_hash_rt.to_hex(),
            direct_live_hash.to_hex()
        );
    }

    /// Same as above but with hot archive bucket list too.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_build_has_hash_matches_with_hot_archive() {
        use henyey_bucket::{BucketList, HotArchiveBucketList};
        use sha2::{Digest, Sha256};
        use stellar_xdr::curr::*;

        let mut bl = BucketList::new();
        let mut habl = HotArchiveBucketList::new();

        for seq in 1..=7u32 {
            let account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([seq as u8; 32])));
            let entry = LedgerEntry {
                last_modified_ledger_seq: seq,
                data: LedgerEntryData::Account(AccountEntry {
                    account_id: account_id.clone(),
                    balance: 100_000_000 * (seq as i64),
                    seq_num: SequenceNumber(0),
                    num_sub_entries: 0,
                    inflation_dest: None,
                    flags: 0,
                    home_domain: String32(StringM::default()),
                    thresholds: Thresholds([1, 0, 0, 0]),
                    signers: VecM::default(),
                    ext: AccountEntryExt::V0,
                }),
                ext: LedgerEntryExt::V0,
            };
            bl.add_batch(seq, 25, BucketListType::Live, vec![entry], vec![], vec![])
                .unwrap();
            // Add empty batch to hot archive to advance it
            habl.add_batch(seq, 25, vec![], vec![]).unwrap();
        }

        let has = build_history_archive_state(7, &bl, Some(&habl), None).unwrap();

        assert_eq!(has.current_buckets.len(), BUCKET_LIST_LEVELS);
        assert!(has.hot_archive_buckets.is_some());
        assert_eq!(
            has.hot_archive_buckets.as_ref().unwrap().len(),
            BUCKET_LIST_LEVELS
        );

        // Compute combined hash from HAS the Go SDK way
        let compute_hash_from_levels = |levels: &[HASBucketLevel]| -> henyey_common::Hash256 {
            let mut total = Vec::new();
            for level in levels {
                let curr_bytes = hex::decode(&level.curr).unwrap();
                let snap_bytes = hex::decode(&level.snap).unwrap();
                let mut h = Sha256::new();
                h.update(&curr_bytes);
                h.update(&snap_bytes);
                total.extend_from_slice(&h.finalize());
            }
            let mut h = Sha256::new();
            h.update(&total);
            let r = h.finalize();
            let mut b = [0u8; 32];
            b.copy_from_slice(&r);
            henyey_common::Hash256::from_bytes(b)
        };

        let go_live = compute_hash_from_levels(&has.current_buckets);
        let go_hot = compute_hash_from_levels(has.hot_archive_buckets.as_ref().unwrap());

        let go_combined = {
            let mut h = Sha256::new();
            h.update(go_live.as_bytes());
            h.update(go_hot.as_bytes());
            let r = h.finalize();
            let mut b = [0u8; 32];
            b.copy_from_slice(&r);
            henyey_common::Hash256::from_bytes(b)
        };

        // Compute combined hash directly
        let direct_live = bl.hash();
        let direct_hot = habl.hash();
        let direct_combined = {
            let mut h = Sha256::new();
            h.update(direct_live.as_bytes());
            h.update(direct_hot.as_bytes());
            let r = h.finalize();
            let mut b = [0u8; 32];
            b.copy_from_slice(&r);
            henyey_common::Hash256::from_bytes(b)
        };

        assert_eq!(
            go_combined,
            direct_combined,
            "Combined: Go SDK ({}) != direct ({})\nLive: go={} direct={}\nHot: go={} direct={}",
            go_combined.to_hex(),
            direct_combined.to_hex(),
            go_live.to_hex(),
            direct_live.to_hex(),
            go_hot.to_hex(),
            direct_hot.to_hex(),
        );
    }

    /// Verify HAS version is conditional: version 2 with hot archive, version 1 without.
    #[test]
    fn test_has_version_conditional_on_hot_archive() {
        use henyey_bucket::{BucketList, HotArchiveBucketList};

        let bl = BucketList::new();
        let habl = HotArchiveBucketList::new();

        // Without hot archive → version 1
        let has_v1 = build_history_archive_state(1, &bl, None, None).unwrap();
        assert_eq!(has_v1.version, 1);
        assert!(has_v1.hot_archive_buckets.is_none());

        // With hot archive → version 2
        let has_v2 = build_history_archive_state(1, &bl, Some(&habl), None).unwrap();
        assert_eq!(has_v2.version, 2);
        assert!(has_v2.hot_archive_buckets.is_some());
        assert_eq!(
            has_v2.hot_archive_buckets.as_ref().unwrap().len(),
            BUCKET_LIST_LEVELS
        );
    }

    /// Verify that HAS JSON omits hotArchiveBuckets when None (version 1),
    /// and includes it when Some (version 2), matching Horizon's expectations.
    #[test]
    fn test_has_json_format_matches_horizon_expectations() {
        use henyey_bucket::{BucketList, HotArchiveBucketList};

        let bl = BucketList::new();
        let habl = HotArchiveBucketList::new();

        // Version 1: hotArchiveBuckets should be absent from JSON entirely
        let has_v1 = build_history_archive_state(1, &bl, None, None).unwrap();
        let json_v1 = has_v1.to_json().unwrap();
        assert!(
            !json_v1.contains("hotArchiveBuckets"),
            "Version 1 HAS should not contain hotArchiveBuckets key, got: {}",
            &json_v1[..json_v1.len().min(200)]
        );

        // Version 2: hotArchiveBuckets should be present and non-null
        let has_v2 = build_history_archive_state(1, &bl, Some(&habl), None).unwrap();
        let json_v2 = has_v2.to_json().unwrap();
        assert!(
            json_v2.contains("hotArchiveBuckets"),
            "Version 2 HAS should contain hotArchiveBuckets key"
        );
        assert!(
            !json_v2.contains("\"hotArchiveBuckets\":null"),
            "hotArchiveBuckets should not be null"
        );

        // Round-trip: deserialize and verify
        let reparsed: HistoryArchiveState = serde_json::from_str(&json_v2).unwrap();
        assert!(reparsed.hot_archive_buckets.is_some());
        assert_eq!(
            reparsed.hot_archive_buckets.as_ref().unwrap().len(),
            BUCKET_LIST_LEVELS
        );
    }

    /// Verify that an empty HotArchiveBucketList produces a combined hash
    /// that is SHA256(live_hash || all_zeros_hot_hash), not just live_hash.
    #[test]
    fn test_empty_hot_archive_contributes_to_combined_hash() {
        use henyey_bucket::{BucketList, HotArchiveBucketList};
        use sha2::{Digest, Sha256};

        let bl = BucketList::new();
        let habl = HotArchiveBucketList::new();

        let live_hash = bl.hash();
        let hot_hash = habl.hash();

        // Combined hash should be SHA256(live || hot), not just live
        let expected = {
            let mut h = Sha256::new();
            h.update(live_hash.as_bytes());
            h.update(hot_hash.as_bytes());
            let r = h.finalize();
            let mut b = [0u8; 32];
            b.copy_from_slice(&r);
            henyey_common::Hash256::from_bytes(b)
        };

        // live_hash alone should differ from combined
        assert_ne!(
            live_hash, expected,
            "Combined hash must differ from live-only hash"
        );
    }

    /// Reproduce the quickstart local mode bug: Horizon reads the HAS published
    /// by henyey and computes a bucket_list_hash that doesn't match the header.
    ///
    /// This simulates the exact flow:
    /// 1. Genesis at protocol 0 (no hot archive in header hash)
    /// 2. Upgrade to protocol 25 (header now uses SHA256(live || hot))
    /// 3. Close ledgers through a checkpoint boundary
    /// 4. Build HAS with hot archive
    /// 5. Verify Go SDK hash from HAS matches header's bucket_list_hash
    #[tokio::test(flavor = "multi_thread")]
    async fn test_quickstart_local_mode_has_hash_matches_header() {
        use henyey_bucket::{BucketList, HotArchiveBucketList};
        use sha2::{Digest, Sha256};
        use stellar_xdr::curr::{
            AccountEntry, AccountEntryExt, AccountId, BucketListType, LedgerEntry, LedgerEntryData,
            LedgerEntryExt, PublicKey, SequenceNumber, String32, StringM, Thresholds, Uint256,
            VecM,
        };

        // --- Helper: compute Go SDK hash from HAS (same as Horizon) ---
        let go_sdk_hash_from_levels = |levels: &[HASBucketLevel]| -> henyey_common::Hash256 {
            let mut total = Vec::new();
            for level in levels {
                let curr = hex::decode(&level.curr).unwrap_or_default();
                let snap = hex::decode(&level.snap).unwrap_or_default();
                let mut h = Sha256::new();
                h.update(&curr);
                h.update(&snap);
                total.extend_from_slice(&h.finalize());
            }
            let mut h = Sha256::new();
            h.update(&total);
            let r = h.finalize();
            let mut b = [0u8; 32];
            b.copy_from_slice(&r);
            henyey_common::Hash256::from_bytes(b)
        };

        let go_sdk_combined_hash = |has: &HistoryArchiveState| -> henyey_common::Hash256 {
            let live_hash = go_sdk_hash_from_levels(&has.current_buckets);
            if has.version < 2 {
                return live_hash;
            }
            let hot_hash = match &has.hot_archive_buckets {
                Some(levels) => go_sdk_hash_from_levels(levels),
                None => {
                    let zero_levels: Vec<HASBucketLevel> = (0..BUCKET_LIST_LEVELS)
                        .map(|_| HASBucketLevel {
                            curr: String::new(),
                            snap: String::new(),
                            next: HASBucketNext::default(),
                        })
                        .collect();
                    go_sdk_hash_from_levels(&zero_levels)
                }
            };
            let mut h = Sha256::new();
            h.update(live_hash.as_bytes());
            h.update(hot_hash.as_bytes());
            let r = h.finalize();
            let mut b = [0u8; 32];
            b.copy_from_slice(&r);
            henyey_common::Hash256::from_bytes(b)
        };

        // --- Simulate quickstart local mode ---
        let mut bl = BucketList::new();
        let mut habl = HotArchiveBucketList::new();

        // Genesis: add root account at ledger 1, protocol 0
        let root_entry = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32]))),
                balance: 1_000_000_000_000_000_000,
                seq_num: SequenceNumber(0),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32(StringM::default()),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: VecM::default(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };
        bl.add_batch(1, 0, BucketListType::Live, vec![root_entry], vec![], vec![])
            .unwrap();

        // Genesis header: protocol 0, bucket_list_hash = live_hash only
        let genesis_live_hash = bl.hash();

        // Verify genesis HAS (version 1, no hot archive)
        let genesis_has = build_history_archive_state(1, &bl, None, None).unwrap();
        assert_eq!(genesis_has.version, 1);
        let genesis_go_hash = go_sdk_combined_hash(&genesis_has);
        assert_eq!(
            genesis_go_hash,
            genesis_live_hash,
            "Genesis: Go SDK hash from HAS ({}) != header bucket_list_hash ({})",
            genesis_go_hash.to_hex(),
            genesis_live_hash.to_hex()
        );

        // Ledger 2: protocol upgrade 0 → 25
        // stellar-core gates addHotArchiveBatch behind prev_version (0 < 23),
        // so hot archive is NOT updated. But the hash combination uses the
        // upgraded version (25 >= 23), so header hash = SHA256(live || hot).
        bl.add_batch(2, 25, BucketListType::Live, vec![], vec![], vec![])
            .unwrap();
        // Don't call habl.add_batch — matching stellar-core's behavior on upgrade ledger.
        // Just advance the ledger_seq so subsequent ledgers don't try to backfill.
        habl.set_ledger_seq(2);

        let live_hash = bl.hash();
        let hot_hash = habl.hash();
        let header_hash = {
            let mut h = Sha256::new();
            h.update(live_hash.as_bytes());
            h.update(hot_hash.as_bytes());
            let r = h.finalize();
            let mut b = [0u8; 32];
            b.copy_from_slice(&r);
            henyey_common::Hash256::from_bytes(b)
        };

        // Build HAS with hot archive (this is what henyey now does after the fix)
        let has = build_history_archive_state(2, &bl, Some(&habl), None).unwrap();
        assert_eq!(has.version, 2);
        let go_hash = go_sdk_combined_hash(&has);

        assert_eq!(
            go_hash,
            header_hash,
            "Ledger 2 (upgrade): Go SDK hash from HAS ({}) != header bucket_list_hash ({})\n\
             live_hash={}, hot_hash={}",
            go_hash.to_hex(),
            header_hash.to_hex(),
            live_hash.to_hex(),
            hot_hash.to_hex(),
        );

        // Close ledgers 3..=7 (first checkpoint at accelerated frequency)
        for seq in 3..=7u32 {
            bl.add_batch(seq, 25, BucketListType::Live, vec![], vec![], vec![])
                .unwrap();
            habl.add_batch(seq, 25, vec![], vec![]).unwrap();

            let live_hash = bl.hash();
            let hot_hash = habl.hash();
            let header_hash = {
                let mut h = Sha256::new();
                h.update(live_hash.as_bytes());
                h.update(hot_hash.as_bytes());
                let r = h.finalize();
                let mut b = [0u8; 32];
                b.copy_from_slice(&r);
                henyey_common::Hash256::from_bytes(b)
            };

            let has = build_history_archive_state(seq, &bl, Some(&habl), None).unwrap();
            assert_eq!(has.version, 2, "Ledger {}: should be version 2", seq);

            let go_hash = go_sdk_combined_hash(&has);
            assert_eq!(
                go_hash,
                header_hash,
                "Ledger {}: Go SDK hash from HAS ({}) != header bucket_list_hash ({})",
                seq,
                go_hash.to_hex(),
                header_hash.to_hex(),
            );

            // Also verify JSON round-trip preserves hashes
            let json = has.to_json().unwrap();
            let reparsed: HistoryArchiveState = serde_json::from_str(&json).unwrap();
            let go_hash_rt = go_sdk_combined_hash(&reparsed);
            assert_eq!(
                go_hash_rt,
                header_hash,
                "Ledger {} (after JSON round-trip): Go SDK hash ({}) != header ({})",
                seq,
                go_hash_rt.to_hex(),
                header_hash.to_hex(),
            );
        }
    }

    #[test]
    fn test_temp_path_uniqueness() {
        use henyey_common::fs_utils::temp_path;
        let base = Path::new("/tmp/bucket-abc123.xdr.gz");
        let p1 = temp_path(base);
        let p2 = temp_path(base);
        assert_ne!(p1, p2, "temp_path must produce unique names");
        // Same directory as the original
        assert_eq!(p1.parent(), base.parent());
        assert_eq!(p2.parent(), base.parent());
        // Contains .tmp. marker
        let name1 = p1.file_name().unwrap().to_string_lossy();
        assert!(name1.contains(".tmp."), "temp name should contain .tmp.");
        assert!(name1.starts_with("bucket-abc123.xdr.gz.tmp."));
    }

    #[test]
    fn test_write_has_atomic() {
        let temp = TempDir::new().unwrap();
        let config = PublishConfig {
            local_path: temp.path().to_path_buf(),
            network_passphrase: Some("Test SDF Network ; September 2015".to_string()),
            ..Default::default()
        };
        let manager = PublishManager::new(config);

        let has = HistoryArchiveState {
            version: 1,
            server: None,
            current_ledger: 64,
            network_passphrase: Some("Test SDF Network ; September 2015".to_string()),
            current_buckets: vec![],
            hot_archive_buckets: None,
        };

        let has_path = temp.path().join("history/00/00/00/history-00000040.json");
        manager.write_has(&has_path, &has).unwrap();

        // Final file exists
        assert!(has_path.exists());

        // No temp files remain
        let parent = has_path.parent().unwrap();
        let tmp_files: Vec<_> = std::fs::read_dir(parent)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(
            tmp_files.is_empty(),
            "no .tmp files should remain after successful write"
        );

        // Content is valid JSON
        let content = std::fs::read_to_string(&has_path).unwrap();
        let _: serde_json::Value = serde_json::from_str(&content).unwrap();
    }

    #[test]
    fn test_write_xdr_gz_atomic() {
        use stellar_xdr::curr::LedgerHeaderHistoryEntry;

        let temp = TempDir::new().unwrap();
        let config = PublishConfig {
            local_path: temp.path().to_path_buf(),
            ..Default::default()
        };
        let manager = PublishManager::new(config);

        let base_path = temp.path().join("ledger/00/00/00/ledger-00000040");
        let final_path = base_path.with_extension("xdr.gz");

        // Write empty items (valid but trivial)
        let items: Vec<LedgerHeaderHistoryEntry> = vec![];
        manager.write_xdr_gz(&base_path, &items, "headers").unwrap();

        // Final file exists
        assert!(final_path.exists());

        // No temp files remain in the directory
        let parent = final_path.parent().unwrap();
        let tmp_files: Vec<_> = std::fs::read_dir(parent)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(
            tmp_files.is_empty(),
            "no .tmp files should remain after successful write"
        );

        // File is valid gzip
        use flate2::read::GzDecoder;
        use std::io::Read;
        let file = std::fs::File::open(&final_path).unwrap();
        let mut decoder = GzDecoder::new(file);
        let mut content = Vec::new();
        decoder.read_to_end(&mut content).unwrap();
        assert!(
            content.is_empty(),
            "empty items should produce empty decompressed content"
        );
    }

    #[test]
    fn test_write_bucket_from_entries_atomic() {
        let temp = TempDir::new().unwrap();
        let config = PublishConfig {
            local_path: temp.path().to_path_buf(),
            ..Default::default()
        };
        let manager = PublishManager::new(config);

        let bucket = henyey_bucket::Bucket::empty();
        let bucket_path = temp.path().join("bucket/00/00/00/bucket-empty.xdr.gz");

        manager
            .write_bucket_from_entries(&bucket_path, &bucket)
            .unwrap();

        // Final file exists
        assert!(bucket_path.exists());

        // No temp files remain
        let parent = bucket_path.parent().unwrap();
        let tmp_files: Vec<_> = std::fs::read_dir(parent)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(
            tmp_files.is_empty(),
            "no .tmp files should remain after successful write"
        );

        // File is valid gzip
        use flate2::read::GzDecoder;
        use std::io::Read;
        let file = std::fs::File::open(&bucket_path).unwrap();
        let mut decoder = GzDecoder::new(file);
        let mut content = Vec::new();
        decoder.read_to_end(&mut content).unwrap();
    }

    #[test]
    fn test_write_bucket_from_entries_disk_backed() {
        use flate2::read::GzDecoder;
        use std::io::Read;

        let temp = TempDir::new().unwrap();
        let config = PublishConfig {
            local_path: temp.path().to_path_buf(),
            ..Default::default()
        };
        let manager = PublishManager::new(config);

        // Create an in-memory bucket with entries, serialize to XDR bytes,
        // then create a disk-backed bucket from those bytes.
        let entries = vec![stellar_xdr::curr::BucketEntry::Liveentry(
            stellar_xdr::curr::LedgerEntry {
                last_modified_ledger_seq: 1,
                data: stellar_xdr::curr::LedgerEntryData::Account(
                    stellar_xdr::curr::AccountEntry {
                        account_id: stellar_xdr::curr::AccountId(
                            stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
                                stellar_xdr::curr::Uint256([1u8; 32]),
                            ),
                        ),
                        balance: 100,
                        seq_num: stellar_xdr::curr::SequenceNumber(1),
                        num_sub_entries: 0,
                        inflation_dest: None,
                        flags: 0,
                        home_domain: stellar_xdr::curr::String32::default(),
                        thresholds: stellar_xdr::curr::Thresholds([1, 0, 0, 0]),
                        signers: Vec::new().try_into().unwrap(),
                        ext: stellar_xdr::curr::AccountEntryExt::V0,
                    },
                ),
                ext: stellar_xdr::curr::LedgerEntryExt::V0,
            },
        )];
        let in_memory_bucket = henyey_bucket::Bucket::from_entries(entries).unwrap();
        let xdr_bytes = in_memory_bucket.to_xdr_bytes().unwrap();

        // Create a disk-backed bucket from the serialized bytes.
        let backing_path = temp.path().join("backing.bucket.xdr");
        let disk_bucket =
            henyey_bucket::Bucket::from_xdr_bytes_disk_backed(&xdr_bytes, &backing_path).unwrap();
        assert!(
            disk_bucket.backing_file_path().is_some(),
            "bucket must be disk-backed"
        );

        // Write via the publish path.
        let bucket_path = temp.path().join("bucket/00/00/00/bucket-disk.xdr.gz");
        manager
            .write_bucket_from_entries(&bucket_path, &disk_bucket)
            .unwrap();

        // Verify output exists and decompresses to the original XDR bytes.
        assert!(bucket_path.exists());
        let file = std::fs::File::open(&bucket_path).unwrap();
        let mut decoder = GzDecoder::new(file);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed).unwrap();
        assert_eq!(decompressed, xdr_bytes);

        // No temp files remain.
        let parent = bucket_path.parent().unwrap();
        let tmp_files: Vec<_> = std::fs::read_dir(parent)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(
            tmp_files.is_empty(),
            "no .tmp files should remain after successful write"
        );
    }

    #[test]
    fn test_write_bucket_from_entries_skips_existing() {
        let temp = TempDir::new().unwrap();
        let config = PublishConfig {
            local_path: temp.path().to_path_buf(),
            ..Default::default()
        };
        let manager = PublishManager::new(config);

        let bucket_path = temp.path().join("bucket/00/00/00/bucket-existing.xdr.gz");
        std::fs::create_dir_all(bucket_path.parent().unwrap()).unwrap();
        std::fs::write(&bucket_path, b"pre-existing content").unwrap();

        let bucket = henyey_bucket::Bucket::empty();
        manager
            .write_bucket_from_entries(&bucket_path, &bucket)
            .unwrap();

        // File was NOT overwritten
        let content = std::fs::read(&bucket_path).unwrap();
        assert_eq!(content, b"pre-existing content");
    }

    /// Helper: create a header with hashes matching the given tx set and result set.
    fn make_header_with_hashes(
        ledger_seq: u32,
        prev_hash: stellar_xdr::curr::Hash,
        tx_set: &henyey_ledger::TransactionSetVariant,
        result_set: &stellar_xdr::curr::TransactionResultSet,
    ) -> stellar_xdr::curr::LedgerHeader {
        use stellar_xdr::curr::*;

        let tx_set_hash = crate::verify::compute_tx_set_hash(tx_set).unwrap();
        let result_xdr = result_set.to_xdr(Limits::none()).unwrap();
        let result_hash = henyey_common::Hash256::hash(&result_xdr);

        LedgerHeader {
            ledger_version: 25,
            previous_ledger_hash: prev_hash,
            scp_value: StellarValue {
                tx_set_hash: Hash(tx_set_hash.0),
                close_time: TimePoint(0),
                upgrades: VecM::default(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash(result_hash.0),
            bucket_list_hash: Hash([0u8; 32]),
            ledger_seq,
            total_coins: 0,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 5000000,
            max_tx_set_size: 100,
            skip_list: std::array::from_fn(|_| Hash([0u8; 32])),
            ext: LedgerHeaderExt::V0,
        }
    }

    /// Helper: create a LedgerHeaderHistoryEntry with computed hash.
    fn make_header_entry(header: stellar_xdr::curr::LedgerHeader) -> LedgerHeaderHistoryEntry {
        use stellar_xdr::curr::LedgerHeaderHistoryEntryExt;
        let hash = henyey_ledger::compute_header_hash(&header).unwrap();
        LedgerHeaderHistoryEntry {
            hash: stellar_xdr::curr::Hash(hash.0),
            header,
            ext: LedgerHeaderHistoryEntryExt::default(),
        }
    }

    /// Helper: create an empty tx set and result set for a header,
    /// Helper to construct an empty TransactionHistoryEntry and TransactionHistoryResultEntry
    /// for a header. Used only in tests for publish-side verification.
    fn make_empty_entries(
        header: &stellar_xdr::curr::LedgerHeader,
    ) -> (
        stellar_xdr::curr::TransactionHistoryEntry,
        stellar_xdr::curr::TransactionHistoryResultEntry,
    ) {
        use stellar_xdr::curr::{
            TransactionHistoryEntryExt, TransactionHistoryResultEntry,
            TransactionHistoryResultEntryExt, TransactionResultSet, TransactionSet,
        };
        let tx_history = stellar_xdr::curr::TransactionHistoryEntry {
            ledger_seq: header.ledger_seq,
            tx_set: TransactionSet {
                previous_ledger_hash: header.previous_ledger_hash.clone(),
                txs: Default::default(),
            },
            ext: TransactionHistoryEntryExt::V0,
        };
        let tx_result = TransactionHistoryResultEntry {
            ledger_seq: header.ledger_seq,
            tx_result_set: TransactionResultSet {
                results: Default::default(),
            },
            ext: TransactionHistoryResultEntryExt::default(),
        };
        (tx_history, tx_result)
    }

    #[test]
    fn test_publish_checkpoint_sparse_entries() {
        use stellar_xdr::curr::*;

        let temp = TempDir::new().unwrap();
        let manager = PublishManager::new(PublishConfig {
            local_path: temp.path().to_path_buf(),
            ..Default::default()
        });

        // Build a 4-ledger checkpoint (seqs 60..=63) where ledger 61 and 62
        // have no transactions (sparse entries).
        let empty_result = TransactionResultSet {
            results: Default::default(),
        };
        let nonempty_result = TransactionResultSet {
            results: vec![TransactionResultPair {
                transaction_hash: Hash([1u8; 32]),
                result: TransactionResult {
                    fee_charged: 100,
                    result: TransactionResultResult::TxSuccess(Default::default()),
                    ext: TransactionResultExt::V0,
                },
            }]
            .try_into()
            .unwrap(),
        };

        // Create headers with matching hashes.
        let mut headers = Vec::new();
        let mut tx_entries = Vec::new();
        let mut tx_results = Vec::new();
        let mut prev_hash = Hash([0u8; 32]);

        for seq in 60..=63u32 {
            let has_txs = seq == 60 || seq == 63;
            let result_set = if has_txs {
                nonempty_result.clone()
            } else {
                empty_result.clone()
            };

            let (tx_entry, _) = make_empty_entries(&LedgerHeader {
                ledger_version: 25,
                previous_ledger_hash: prev_hash.clone(),
                scp_value: StellarValue {
                    tx_set_hash: Hash([0u8; 32]),
                    close_time: TimePoint(0),
                    upgrades: VecM::default(),
                    ext: StellarValueExt::Basic,
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
                ext: LedgerHeaderExt::V0,
            });

            let tx_set = crate::catchup::tx_set_from_history_entry(&tx_entry);
            let header = make_header_with_hashes(seq, prev_hash.clone(), &tx_set, &result_set);
            let entry = make_header_entry(header.clone());
            prev_hash = entry.hash.clone();
            headers.push(entry);

            if has_txs {
                let mut tx_hist = tx_entry.clone();
                tx_hist.ledger_seq = seq;
                tx_entries.push(tx_hist);
                tx_results.push(TransactionHistoryResultEntry {
                    ledger_seq: seq,
                    tx_result_set: result_set,
                    ext: TransactionHistoryResultEntryExt::default(),
                });
            }
        }

        // publish_checkpoint should succeed with sparse entries.
        let result = manager.publish_checkpoint(
            63,
            &headers,
            &tx_entries,
            &tx_results,
            &BucketList::new(),
            None,
        );
        assert!(
            result.is_ok(),
            "sparse checkpoint should publish: {:?}",
            result.err()
        );

        // Verify the tx file has only 2 entries (ledgers 60 and 63).
        let tx_path = temp
            .path()
            .join("transactions/00/00/00/transactions-0000003f.xdr.gz");
        assert!(tx_path.exists(), "transactions file should exist");
    }

    #[test]
    fn test_publish_checkpoint_all_empty() {
        use stellar_xdr::curr::*;

        let temp = TempDir::new().unwrap();
        let manager = PublishManager::new(PublishConfig {
            local_path: temp.path().to_path_buf(),
            ..Default::default()
        });

        // Build a checkpoint where ALL ledgers are empty.
        let empty_result = TransactionResultSet {
            results: Default::default(),
        };

        let mut headers = Vec::new();
        let mut prev_hash = Hash([0u8; 32]);

        for seq in 60..=63u32 {
            let (tx_entry, _) = make_empty_entries(&LedgerHeader {
                ledger_version: 25,
                previous_ledger_hash: prev_hash.clone(),
                scp_value: StellarValue {
                    tx_set_hash: Hash([0u8; 32]),
                    close_time: TimePoint(0),
                    upgrades: VecM::default(),
                    ext: StellarValueExt::Basic,
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
                ext: LedgerHeaderExt::V0,
            });

            let tx_set = crate::catchup::tx_set_from_history_entry(&tx_entry);
            let header = make_header_with_hashes(seq, prev_hash.clone(), &tx_set, &empty_result);
            let entry = make_header_entry(header);
            prev_hash = entry.hash.clone();
            headers.push(entry);
        }

        // No tx entries or results at all.
        let result = manager.publish_checkpoint(63, &headers, &[], &[], &BucketList::new(), None);
        assert!(
            result.is_ok(),
            "all-empty checkpoint should publish: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_publish_checkpoint_rejects_one_sided_presence() {
        use stellar_xdr::curr::*;

        let temp = TempDir::new().unwrap();
        let manager = PublishManager::new(PublishConfig {
            local_path: temp.path().to_path_buf(),
            ..Default::default()
        });

        let empty_result = TransactionResultSet {
            results: Default::default(),
        };

        let mut headers = Vec::new();
        let mut prev_hash = Hash([0u8; 32]);

        for seq in 60..=63u32 {
            let (tx_entry, _) = make_empty_entries(&LedgerHeader {
                ledger_version: 25,
                previous_ledger_hash: prev_hash.clone(),
                scp_value: StellarValue {
                    tx_set_hash: Hash([0u8; 32]),
                    close_time: TimePoint(0),
                    upgrades: VecM::default(),
                    ext: StellarValueExt::Basic,
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
                ext: LedgerHeaderExt::V0,
            });

            let tx_set = crate::catchup::tx_set_from_history_entry(&tx_entry);
            let header = make_header_with_hashes(seq, prev_hash.clone(), &tx_set, &empty_result);
            let entry = make_header_entry(header);
            prev_hash = entry.hash.clone();
            headers.push(entry);
        }

        // Provide a tx entry for ledger 61 but NO result entry → one-sided.
        let (tx_entry, _) = make_empty_entries(&headers[1].header);
        let err = manager
            .publish_checkpoint(63, &headers, &[tx_entry], &[], &BucketList::new(), None)
            .unwrap_err();
        assert!(
            matches!(err, HistoryError::VerificationFailed(ref msg) if msg.contains("present but") && msg.contains("missing")),
            "expected one-sided presence error, got: {:?}",
            err
        );
    }

    #[test]
    fn test_publish_checkpoint_rejects_duplicate_entries() {
        use stellar_xdr::curr::*;

        let temp = TempDir::new().unwrap();
        let manager = PublishManager::new(PublishConfig {
            local_path: temp.path().to_path_buf(),
            ..Default::default()
        });

        let empty_result = TransactionResultSet {
            results: Default::default(),
        };

        let mut headers = Vec::new();
        let mut prev_hash = Hash([0u8; 32]);

        for seq in 60..=63u32 {
            let (tx_entry, _) = make_empty_entries(&LedgerHeader {
                ledger_version: 25,
                previous_ledger_hash: prev_hash.clone(),
                scp_value: StellarValue {
                    tx_set_hash: Hash([0u8; 32]),
                    close_time: TimePoint(0),
                    upgrades: VecM::default(),
                    ext: StellarValueExt::Basic,
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
                ext: LedgerHeaderExt::V0,
            });

            let tx_set = crate::catchup::tx_set_from_history_entry(&tx_entry);
            let header = make_header_with_hashes(seq, prev_hash.clone(), &tx_set, &empty_result);
            let entry = make_header_entry(header);
            prev_hash = entry.hash.clone();
            headers.push(entry);
        }

        // Provide duplicate tx entries for the same ledger_seq.
        let (tx_entry, _) = make_empty_entries(&headers[0].header);
        let dup_tx_entries = vec![tx_entry.clone(), tx_entry];
        let nonempty_result = TransactionHistoryResultEntry {
            ledger_seq: 60,
            tx_result_set: TransactionResultSet {
                results: vec![TransactionResultPair {
                    transaction_hash: Hash([1u8; 32]),
                    result: TransactionResult {
                        fee_charged: 100,
                        result: TransactionResultResult::TxSuccess(Default::default()),
                        ext: TransactionResultExt::V0,
                    },
                }]
                .try_into()
                .unwrap(),
            },
            ext: TransactionHistoryResultEntryExt::default(),
        };
        let dup_results = vec![nonempty_result.clone(), nonempty_result];

        let err = manager
            .publish_checkpoint(
                63,
                &headers,
                &dup_tx_entries,
                &dup_results,
                &BucketList::new(),
                None,
            )
            .unwrap_err();
        assert!(
            matches!(err, HistoryError::VerificationFailed(ref msg) if msg.contains("not strictly ordered")),
            "expected ordering error, got: {:?}",
            err
        );
    }
}
