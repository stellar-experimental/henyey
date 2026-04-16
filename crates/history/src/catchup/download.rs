//! Download helpers for catchup: HAS, buckets, ledger data, and checkpoint headers.

use crate::{
    archive::HistoryArchive, archive_state::HistoryArchiveState, checkpoint, HistoryError, Result,
};
use henyey_bucket::canonical_bucket_filename;
use henyey_common::Hash256;
use std::collections::HashMap;
use std::sync::Arc;

use henyey_common::protocol::{protocol_version_starts_from, ProtocolVersion};
use henyey_ledger::TransactionSetVariant;
use stellar_xdr::curr::{
    GeneralizedTransactionSet, LedgerHeader, LedgerHeaderHistoryEntry, ScpHistoryEntry,
    TransactionHistoryEntry, TransactionHistoryResultEntry, TransactionSet, TransactionSetV1,
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
        let empty_bucket_hash = Hash256::empty_hash();

        // Filter out zero/empty hashes and already-downloaded buckets
        let to_download: Vec<_> = hashes
            .iter()
            .filter(|hash| {
                if hash.is_zero() || *hash == empty_bucket_hash {
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
                                // Save to disk
                                if let Err(e) = std::fs::write(&bucket_path, &data) {
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
    pub(super) async fn download_ledger_data(
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
                .map(|entry| super::tx_set_from_history_entry(entry))
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
        let headers = archive.fetch_ledger_headers(checkpoint).await?;
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
    /// remote HAS and local state. Returns `None` if no local HAS exists (fresh node).
    pub(super) fn load_local_has(&self) -> Option<HistoryArchiveState> {
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
    pub(super) fn compute_bucket_download_set(
        &self,
        remote_has: &HistoryArchiveState,
    ) -> Vec<Hash256> {
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
