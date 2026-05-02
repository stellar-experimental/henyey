//! Download helpers for catchup: HAS, buckets, ledger data, and checkpoint headers.

use crate::{
    archive::HistoryArchive, archive_state::HistoryArchiveState, checkpoint, verify, HistoryError,
    Result,
};
use henyey_bucket::canonical_bucket_filename;
use henyey_common::fs_utils::atomic_write_bytes;
use henyey_common::Hash256;
use std::collections::HashMap;
use std::sync::Arc;

use stellar_xdr::curr::{
    LedgerHeader, LedgerHeaderHistoryEntry, ScpHistoryEntry, TransactionHistoryEntry,
    TransactionHistoryResultEntry,
};
use tracing::{debug, info, warn};

use super::{CatchupManager, LedgerData};

/// Maximum number of concurrent bucket downloads, mirroring stellar-core's
/// `MAX_CONCURRENT_SUBPROCESSES`.
pub(super) const MAX_CONCURRENT_DOWNLOADS: usize = 16;

/// Log download progress every N items (and always on the last item).
const PROGRESS_REPORT_INTERVAL: u32 = 5;

/// Run a future to completion from a synchronous context.
///
/// Handles three cases:
/// 1. Inside a multi-threaded tokio runtime → `block_in_place` + `block_on`
/// 2. Inside a single-threaded tokio runtime → spawn a helper thread
/// 3. No runtime → create a temporary single-threaded runtime
pub(super) fn block_on_async<F, T>(future: F) -> std::result::Result<T, henyey_bucket::BucketError>
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
pub(super) async fn download_bucket_from_archives(
    archives: Vec<Arc<HistoryArchive>>,
    hash: Hash256,
) -> std::result::Result<Vec<u8>, henyey_bucket::BucketError> {
    for archive in &archives {
        match archive.fetch_bucket(&hash).await {
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

/// Data downloaded for a single checkpoint.
#[derive(Debug, Clone)]
pub(super) struct CheckpointLedgerData {
    pub(super) headers: Vec<LedgerHeaderHistoryEntry>,
    pub(super) tx_entries: Vec<TransactionHistoryEntry>,
    pub(super) result_entries: Vec<TransactionHistoryResultEntry>,
}

impl CatchupManager {
    /// Download the History Archive State for a checkpoint.
    ///
    /// Uses archive rotation: each attempt tries a different archive, cycling
    /// through them to provide failover when one archive is unavailable.
    pub(super) async fn download_has(&self, checkpoint_seq: u32) -> Result<HistoryArchiveState> {
        let num_archives = self.archives.len() as u32;
        for attempt in 0..num_archives {
            let archive = self.select_archive(attempt);
            match archive.fetch_checkpoint_has(checkpoint_seq).await {
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

    pub(super) async fn download_scp_history(
        &self,
        checkpoint_seq: u32,
    ) -> Result<Vec<ScpHistoryEntry>> {
        for archive in &self.archives {
            match archive.fetch_scp_history(checkpoint_seq).await {
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
    pub(super) async fn download_buckets(
        &mut self,
        hashes: &[Hash256],
    ) -> Result<Vec<(Hash256, Vec<u8>)>> {
        use futures::stream::{self, StreamExt};

        let bucket_dir = self.bucket_manager.bucket_dir().to_path_buf();

        // Filter out sentinel hashes and already-downloaded buckets
        let to_download: Vec<_> = hashes
            .iter()
            .filter(|hash| {
                if hash.is_empty_bucket_sentinel() {
                    return false;
                }
                let bucket_path = bucket_dir.join(canonical_bucket_filename(&hash));
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
        let results: Vec<Result<()>> = stream::iter(to_download)
            .map(|hash| {
                let archives = archives.clone();
                let bucket_dir = bucket_dir.clone();
                let downloaded = &downloaded;

                async move {
                let bucket_path = bucket_dir.join(canonical_bucket_filename(&hash));

                    // Try each archive until one succeeds
                    for archive in &archives {
                        match archive.fetch_bucket(&hash).await {
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
                                // Save to disk atomically
                                if let Err(e) = atomic_write_bytes(&bucket_path, &data) {
                                    warn!("Failed to save bucket {} to disk: {}", hash, e);
                                    continue;
                                }
                                let count = downloaded
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                                    + 1;
                                if count % PROGRESS_REPORT_INTERVAL == 0 || count == total_to_download as u32 {
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

        // Stage E: emit per-bucket-file terminal outcome.
        // `download_bucket_*` counts archive-rotation-final outcomes; archive
        // failures within a single bucket's retry loop are not counted.
        for result in &results {
            match result {
                Ok(()) => {
                    metrics::counter!("stellar_history_download_bucket_success_total").increment(1);
                }
                Err(_) => {
                    metrics::counter!("stellar_history_download_bucket_failure_total").increment(1);
                }
            }
        }

        // Check for any failures
        for result in results {
            result?;
        }

        self.progress.buckets_downloaded = hashes.len() as u32;
        info!("Pre-downloaded all {} buckets to disk", total_to_download);

        // Return empty - buckets are on disk, not in memory
        Ok(Vec::new())
    }

    /// Download ledger headers, transactions, and results for a range.
    ///
    /// # Arguments
    ///
    /// * `from_ledger` — sequence number of the Last Closed Ledger (the most
    ///   recently applied ledger). Download starts at `from_ledger + 1`.
    /// * `to_ledger` — inclusive upper bound of the range to download.
    /// * `lcl_protocol_version` — protocol version of the LCL at the start of
    ///   this replay batch. Used for empty tx set synthesis when archives omit
    ///   tx entries for ledgers with no transactions.
    pub(super) async fn download_ledger_data(
        &mut self,
        from_ledger: u32,
        to_ledger: u32,
        lcl_protocol_version: u32,
    ) -> Result<Vec<LedgerData>> {
        let mut data = Vec::new();
        let mut checkpoint_cache: HashMap<u32, CheckpointLedgerData> = HashMap::new();

        // We need to download data for ledgers (from_ledger+1) to to_ledger.
        // The from_ledger's state is already in the bucket list.
        let start = from_ledger + 1;

        if start > to_ledger {
            // No ledgers to replay, we're at the checkpoint
            return Ok(data);
        }

        // Track the LCL protocol version across the loop. For the first ledger,
        // this is the caller-provided value. For subsequent ledgers, it's the
        // prior header's ledger_version (since that header becomes the new LCL).
        let mut current_lcl_protocol = lcl_protocol_version;

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

            let tx_result_entry = cache
                .result_entries
                .iter()
                .find(|entry| entry.ledger_seq == seq)
                .cloned();

            data.push(LedgerData::new(
                header.clone(),
                tx_history_entry,
                tx_result_entry,
                current_lcl_protocol,
            )?);

            // This header becomes the LCL for the next iteration.
            current_lcl_protocol = header.ledger_version;
        }

        Ok(data)
    }

    /// Download ledger headers, transactions, and results for a checkpoint.
    ///
    /// Stage E instrumentation: emits
    /// `stellar_history_download_ledger_{success,failure}_total` once per
    /// checkpoint as a single "checkpoint data acquired" event. Per-archive
    /// rotation attempts within this method are not counted individually.
    async fn download_checkpoint_ledger_data(
        &self,
        checkpoint: u32,
    ) -> Result<CheckpointLedgerData> {
        // Try each archive until one succeeds
        for archive in &self.archives {
            match self.try_download_checkpoint(archive, checkpoint).await {
                Ok(data) => {
                    metrics::counter!("stellar_history_download_ledger_success_total").increment(1);
                    return Ok(data);
                }
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

        metrics::counter!("stellar_history_download_ledger_failure_total").increment(1);
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
        let headers = archive.fetch_ledger_headers(checkpoint).await?;
        verify::verify_header_chain_from_entries(&headers)?;
        let tx_entries = archive.fetch_transactions(checkpoint).await?;
        let result_entries = archive.fetch_results(checkpoint).await?;
        Ok(CheckpointLedgerData {
            headers,
            tx_entries,
            result_entries,
        })
    }

    /// Load the local History Archive State from the database, if available.
    ///
    /// This is used by `differing_bucket_hashes()` to compute the differential
    /// bucket download set — only downloading buckets that differ between the
    /// remote HAS and local state.
    ///
    /// Returns `Ok(None)` if no local HAS has been persisted (fresh node).
    /// Returns `Err` if the DB read fails or the stored JSON is corrupt.
    pub(super) fn load_local_has(&self) -> Result<Option<HistoryArchiveState>> {
        let json_opt = self.db.with_connection(|conn| {
            use henyey_db::queries::StateQueries;
            conn.get_state(henyey_db::schema::state_keys::HISTORY_ARCHIVE_STATE)
        })?;

        match json_opt {
            None => Ok(None),
            Some(json) => Ok(Some(HistoryArchiveState::from_json(&json)?)),
        }
    }

    /// Compute the bucket hashes to download from a remote HAS.
    ///
    /// If a local HAS is available in the database, computes the differential
    /// set (only buckets we don't already have). Otherwise falls back to all
    /// unique bucket hashes from the remote HAS.
    ///
    /// This mirrors stellar-core's use of `differingBuckets(mLocalState)` in
    /// `CatchupWork.cpp`.
    pub(super) fn compute_bucket_download_set(
        &self,
        remote_has: &HistoryArchiveState,
    ) -> Result<Vec<Hash256>> {
        match self.load_local_has()? {
            Some(local_has) => {
                let hashes = remote_has.all_differing_bucket_hashes(&local_has);
                info!(
                    "Computed differential bucket set: {} buckets to download \
                     (remote has {} unique, local has {} unique)",
                    hashes.len(),
                    remote_has.unique_bucket_hashes().len(),
                    local_has.unique_bucket_hashes().len(),
                );
                Ok(hashes)
            }
            None => {
                info!("No local HAS in database (fresh node), downloading all unique buckets");
                Ok(remote_has.unique_bucket_hashes())
            }
        }
    }

    /// Download the header for a specific ledger with its verified hash.
    ///
    /// The archive-advertised hash is accepted only after recomputing the
    /// header hash locally and checking that both values match.
    pub(super) async fn download_checkpoint_header(
        &self,
        ledger_seq: u32,
    ) -> Result<(LedgerHeader, Hash256)> {
        for archive in &self.archives {
            match archive.fetch_ledger_header_with_hash(ledger_seq).await {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use henyey_bucket::BucketManager;
    use henyey_db::{queries::StateQueries, Database};

    fn make_test_catchup_manager() -> CatchupManager {
        let db = Database::open_in_memory().expect("in-memory db");
        let tmp_dir = tempfile::tempdir().expect("temp dir");
        let bucket_manager = BucketManager::new(tmp_dir.keep()).expect("bucket manager");
        let archive = crate::HistoryArchive::new("https://example.com").expect("archive");
        super::super::CatchupManager::new(vec![archive], bucket_manager, db)
    }

    #[test]
    fn test_load_local_has_absent() {
        let mgr = make_test_catchup_manager();
        let result = mgr.load_local_has();
        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
        assert!(result.unwrap().is_none(), "expected None for fresh DB");
    }

    #[test]
    fn test_load_local_has_valid() {
        let mgr = make_test_catchup_manager();
        let has_json = r#"{"version":2,"currentLedger":100,"currentBuckets":[]}"#;
        mgr.db
            .with_connection(|conn| {
                conn.set_state(
                    henyey_db::schema::state_keys::HISTORY_ARCHIVE_STATE,
                    has_json,
                )
            })
            .expect("set_state");
        let result = mgr.load_local_has();
        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
        let has = result.unwrap().expect("expected Some");
        assert_eq!(has.current_ledger(), 100);
    }

    #[test]
    fn test_load_local_has_corrupt_json() {
        let mgr = make_test_catchup_manager();
        mgr.db
            .with_connection(|conn| {
                conn.set_state(
                    henyey_db::schema::state_keys::HISTORY_ARCHIVE_STATE,
                    "this is not valid json {{{",
                )
            })
            .expect("set_state");
        let result = mgr.load_local_has();
        assert!(result.is_err(), "expected Err for corrupt JSON");
        let err = result.unwrap_err();
        assert!(
            matches!(err, HistoryError::Json(_)),
            "expected HistoryError::Json, got: {:?}",
            err,
        );
    }

    /// Stage E: pin the metric literals emitted from this module so a typo
    /// can't silently detach this crate from the central catalog.
    #[test]
    fn test_stage_e_download_metric_literals_present() {
        let src = include_str!("download.rs");
        for literal in &[
            "\"stellar_history_download_bucket_success_total\"",
            "\"stellar_history_download_bucket_failure_total\"",
            "\"stellar_history_download_ledger_success_total\"",
            "\"stellar_history_download_ledger_failure_total\"",
        ] {
            assert!(
                src.contains(literal),
                "expected metric literal {literal} in catchup/download.rs",
            );
        }
    }
}
