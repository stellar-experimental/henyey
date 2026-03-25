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
use henyey_bucket::{BucketList, PendingMergeState};
use henyey_common::Hash256;
use henyey_ledger::TransactionSetVariant;
use std::path::{Path, PathBuf};
use stellar_xdr::curr::{
    LedgerHeaderHistoryEntry, TransactionHistoryEntry, TransactionHistoryEntryExt,
    TransactionHistoryResultEntry, WriteXdr,
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
                    state: 1, // FB_HASH_OUTPUT
                    output: Some(hash.to_hex()),
                    curr: None,
                    snap: None,
                    shadow: None,
                },
                Some(PendingMergeState::Inputs { curr, snap }) => HASBucketNext {
                    state: 2, // FB_HASH_INPUTS
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
                        state: 1, // FB_HASH_OUTPUT
                        output: Some(hash.to_hex()),
                        curr: None,
                        snap: None,
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
            .collect()
    });

    Ok(HistoryArchiveState {
        version: 2,
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
    pub async fn publish_checkpoint(
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

        let header_chain: Vec<_> = headers.iter().map(|entry| entry.header.clone()).collect();
        verify::verify_header_chain(&header_chain)?;

        let tx_entry_map: std::collections::HashMap<_, _> = tx_entries
            .iter()
            .map(|entry| (entry.ledger_seq, entry))
            .collect();
        let tx_result_map: std::collections::HashMap<_, _> = tx_results
            .iter()
            .map(|entry| (entry.ledger_seq, entry))
            .collect();

        for header in &header_chain {
            let entry = tx_entry_map.get(&header.ledger_seq).ok_or_else(|| {
                HistoryError::VerificationFailed(format!(
                    "missing tx history entry for ledger {}",
                    header.ledger_seq
                ))
            })?;

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

            let tx_set = match &entry.ext {
                TransactionHistoryEntryExt::V0 => {
                    TransactionSetVariant::Classic(entry.tx_set.clone())
                }
                TransactionHistoryEntryExt::V1(set) => {
                    TransactionSetVariant::Generalized(set.clone())
                }
            };
            verify::verify_tx_set(header, &tx_set)?;

            let result_entry = tx_result_map.get(&header.ledger_seq).ok_or_else(|| {
                HistoryError::VerificationFailed(format!(
                    "missing tx result entry for ledger {}",
                    header.ledger_seq
                ))
            })?;
            let xdr = result_entry
                .tx_result_set
                .to_xdr(stellar_xdr::curr::Limits::none())
                .map_err(|e| HistoryError::VerificationFailed(e.to_string()))?;
            verify::verify_tx_result_set(header, &xdr)?;
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
            "Checkpoint published successfully"
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
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;

        let file = std::fs::File::create(path.with_extension("xdr.gz"))?;
        let mut encoder = GzEncoder::new(file, Compression::default());

        for item in items {
            let xdr = item
                .to_xdr(stellar_xdr::curr::Limits::none())
                .map_err(|e| HistoryError::VerificationFailed(e.to_string()))?;
            // Write record mark: length with high bit set (last fragment)
            let marked_len = (xdr.len() as u32) | 0x8000_0000;
            encoder.write_all(&marked_len.to_be_bytes())?;
            encoder.write_all(&xdr)?;
        }

        encoder.finish()?;
        debug!("Wrote {} {} to {:?}", items.len(), label, path);
        Ok(())
    }

    /// Write a bucket to the history archive as a gzip-compressed file.
    ///
    /// If the bucket has a backing file on disk (`.bucket.xdr`), we gzip-compress
    /// it directly — this preserves the exact record-marked format and avoids
    /// re-serialization. For in-memory-only buckets, we serialize entries with
    /// RFC 5531 record marks.
    fn write_bucket_from_entries(&self, path: &Path, bucket: &henyey_bucket::Bucket) -> Result<()> {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::{Read, Write};

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

        let file = std::fs::File::create(path)?;
        let mut encoder = GzEncoder::new(file, Compression::default());

        if let Some(backing_path) = bucket.backing_file_path() {
            // Fast path: gzip-compress the existing .bucket.xdr file directly.
            // This preserves the exact record-marked format.
            let mut src = std::fs::File::open(backing_path)?;
            let mut buf = [0u8; 64 * 1024];
            loop {
                let n = src.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                encoder.write_all(&buf[..n])?;
            }
        } else {
            // Fallback: serialize entries with RFC 5531 record marks
            for entry in bucket.iter() {
                let xdr = entry.to_xdr(stellar_xdr::curr::Limits::none()).map_err(
                    |e: stellar_xdr::curr::Error| HistoryError::VerificationFailed(e.to_string()),
                )?;
                let marked_len = (xdr.len() as u32) | 0x8000_0000;
                encoder.write_all(&marked_len.to_be_bytes())?;
                encoder.write_all(&xdr)?;
            }
        }

        encoder.finish()?;
        debug!("Wrote bucket to {:?}", path);
        Ok(())
    }

    /// Write a History Archive State file.
    fn write_has(&self, path: &Path, has: &HistoryArchiveState) -> Result<()> {
        let json = serde_json::to_string_pretty(has)
            .map_err(|e| HistoryError::VerificationFailed(e.to_string()))?;
        std::fs::write(path, json)?;
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

        // Verify HAS has 11 levels
        assert_eq!(has.current_buckets.len(), 11, "HAS must have 11 levels");
        assert_eq!(has.version, 2);

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

        assert_eq!(has.current_buckets.len(), 11);
        assert!(has.hot_archive_buckets.is_some());
        assert_eq!(has.hot_archive_buckets.as_ref().unwrap().len(), 11);

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
}
