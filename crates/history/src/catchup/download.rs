//! Download helpers for catchup: HAS, buckets, ledger data, and checkpoint headers.

use crate::{
    archive::HistoryArchive, archive_state::HistoryArchiveState, checkpoint, verify, HistoryError,
    Result,
};
use henyey_bucket::canonical_bucket_filename;
use henyey_common::fs_utils::atomic_write_bytes;
use henyey_common::history_download::{MAX_CONCURRENT_DOWNLOADS, PROGRESS_REPORT_INTERVAL};
use henyey_common::protocol::LclContext;
use henyey_common::Hash256;
use std::collections::HashMap;
use std::sync::Arc;

use stellar_xdr::{
    LedgerHeader, LedgerHeaderHistoryEntry, ScpHistoryEntry, TransactionHistoryEntry,
    TransactionHistoryResultEntry,
};
use tracing::{debug, info, warn};

use super::{CatchupManager, LedgerData};

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
/// Downloads a bucket from archives with rotation, returning the data and the
/// name of the archive that provided it.
pub(super) async fn download_bucket_from_archives(
    archives: Vec<Arc<HistoryArchive>>,
    hash: Hash256,
) -> std::result::Result<(Vec<u8>, String), henyey_bucket::BucketError> {
    let mut last_archive_name = String::new();
    for archive in &archives {
        last_archive_name = archive.name().to_owned();
        match archive.fetch_bucket(&hash).await {
            Ok(data) => return Ok((data, archive.name().to_owned())),
            Err(e) => {
                warn!("Failed to download bucket {} from archive: {}", hash, e);
                continue;
            }
        }
    }
    Err(henyey_bucket::BucketError::NotFound(format!(
        "Bucket {} not found in any archive (last: {})",
        hash, last_archive_name
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
    ///
    /// Returns the parsed HAS together with the `Arc<HistoryArchive>` that
    /// served it. Callers in the gated replay path (#2940) pin subsequent
    /// ledger-data downloads to this exact archive so the archive that passed
    /// the #2937 publication gate is the same one that supplies the ledger
    /// data — closing the cross-archive bypass where a second stale-but-serving
    /// archive could serve data that never satisfied the gate. The archive's
    /// name (for metric labels) is derived via `archive.name()` at call sites.
    pub(super) async fn download_has(
        &self,
        checkpoint_seq: u32,
    ) -> Result<(HistoryArchiveState, Arc<HistoryArchive>)> {
        let num_archives = self.archives.len() as u32;
        for attempt in 0..num_archives {
            let archive = self.select_archive(attempt);
            match archive.fetch_checkpoint_has(checkpoint_seq).await {
                Ok(has) => return Ok((has, Arc::clone(archive))),
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
        // Each result carries the archive name that served the bucket on success.
        let results: Vec<Result<String>> = stream::iter(to_download)
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
                                // Save to disk atomically, off the tokio worker
                                // thread. `atomic_write_bytes` issues blocking
                                // `fsync` calls; running them inline on the
                                // worker that polls this future starves the
                                // runtime when up to MAX_CONCURRENT_DOWNLOADS
                                // (16) of these futures are fanned out via
                                // `.buffer_unordered` under disk pressure
                                // (#3686). Capture `data.len()` BEFORE moving
                                // `data` into the blocking closure (the
                                // `debug!` below still needs it).
                                let nbytes = data.len();
                                let write_path = bucket_path.clone();
                                let write_res = match henyey_common::spawn_blocking_logged(
                                    "catchup-save-bucket",
                                    move || atomic_write_bytes(&write_path, &data),
                                )
                                .await
                                {
                                    Ok(inner) => inner.map_err(|e| e.to_string()),
                                    Err(join_err) => {
                                        Err(format!("persist task failed: {join_err}"))
                                    }
                                };
                                if let Err(e) = write_res {
                                    warn!("Failed to save bucket {} to disk: {}", hash, e);
                                    continue;
                                }
                                let count = downloaded
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                                    + 1;
                                if count % PROGRESS_REPORT_INTERVAL == 0 || count == total_to_download as u32 {
                                    info!("Downloaded {}/{} buckets", count, total_to_download);
                                }
                                debug!("Pre-downloaded bucket {} ({} bytes)", hash, nbytes);
                                return Ok(archive.name().to_owned());
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

        // Stage E: emit per-bucket-file terminal outcome with archive label.
        // `download_bucket_*` counts archive-rotation-final outcomes; archive
        // failures within a single bucket's retry loop are not counted.
        let last_archive_name = self
            .archives
            .last()
            .map(|a| a.name().to_owned())
            .unwrap_or_default();
        for result in &results {
            match result {
                Ok(archive_name) => {
                    metrics::counter!(
                        "stellar_history_download_bucket_success_total",
                        "archive" => archive_name.clone(),
                    )
                    .increment(1);
                }
                Err(_) => {
                    metrics::counter!(
                        "stellar_history_download_bucket_failure_total",
                        "archive" => last_archive_name.clone(),
                    )
                    .increment(1);
                }
            }
        }

        // Check for any failures
        for result in results {
            result.map(|_| ())?;
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
    ///   recently applied ledger). The knit-prefix entries (at LCL and,
    ///   when in the same checkpoint as LCL+1, at LCL-1) are extracted from
    ///   the same checkpoint file as LCL+1 and returned separately for the
    ///   §11.2 knit-to-LCL decision matrix. Apply entries cover
    ///   `[from_ledger + 1, to_ledger]`.
    /// * `to_ledger` — inclusive upper bound of the range to download.
    /// * `initial_lcl` — context from the LCL at the start of this replay batch.
    ///   Used for empty tx set synthesis when archives omit tx entries for
    ///   ledgers with no transactions.
    ///
    /// Returns `(apply_data, knit_entries, archive_name)`. `knit_entries`
    /// is empty when `from_ledger == 0` (genesis), when LCL sits on a
    /// checkpoint boundary (LCL-1 in a prior file, no overlap), or when
    /// there is no work to do.
    ///
    /// # Pinning (#2940)
    ///
    /// When `pinned` is `Some(archive)`, every download issued here (the LCL
    /// header via `download_checkpoint_header` and each checkpoint payload via
    /// `download_checkpoint_ledger_data`) is served from that archive **only**
    /// — no rotation, no cross-archive fallback. This is used by the gated
    /// replay path so the archive that passed the #2937 publication gate is the
    /// same archive that supplies the ledger data. When `pinned` is `None`
    /// (bucket-apply phase / non-gated callers), the existing fixed-order
    /// rotation over `&self.archives` is used.
    pub(super) async fn download_ledger_data(
        &mut self,
        from_ledger: u32,
        to_ledger: u32,
        initial_lcl: LclContext,
        pinned: Option<&Arc<HistoryArchive>>,
    ) -> Result<(Vec<LedgerData>, Vec<LedgerHeaderHistoryEntry>, String)> {
        let mut data = Vec::new();
        let mut knit_entries: Vec<LedgerHeaderHistoryEntry> = Vec::new();
        let mut checkpoint_cache: HashMap<u32, CheckpointLedgerData> = HashMap::new();
        // Track the last archive that served data; used for metric attribution.
        // Initialized to "none" — only set to an actual archive when a download occurs.
        let mut last_archive_name = "none".to_owned();

        // CATCHUP_SPEC §11.2: stellar-core checks knit-to-LCL against headers
        // already present in the current checkpoint file (the one containing
        // LCL+1). We mirror that by extending the iteration backwards to
        // include LCL and (when in the same checkpoint file as LCL+1) LCL-1.
        // We never fetch a prior checkpoint just to validate LCL-1 — it is
        // skipped in that case. For genesis (from_ledger == 0) there is no
        // LCL header to inspect; iteration starts at ledger 1.
        let apply_start = from_ledger.saturating_add(1);
        let knit_start = if from_ledger == 0 {
            apply_start
        } else {
            let apply_ckpt_start = crate::checkpoint::checkpoint_start(apply_start);
            std::cmp::max(apply_ckpt_start, from_ledger.saturating_sub(1))
        };

        if apply_start > to_ledger {
            // No ledgers to replay, we're at the checkpoint
            return Ok((data, knit_entries, last_archive_name));
        }

        // Resolve the LCL context from the archive when from_ledger > 0.
        // The caller-provided value may be stale (e.g., synthetic genesis at
        // version 0 when the actual network genesis is at version 25+).
        // We use download_checkpoint_header (lightweight single-header fetch)
        // rather than downloading the full checkpoint data.
        let mut current_lcl = if from_ledger > 0 {
            let (lcl_header, lcl_hash) =
                self.download_checkpoint_header(from_ledger, pinned).await?;
            LclContext::new(lcl_header.ledger_version, lcl_hash)
        } else {
            // from_ledger == 0 means "before genesis"; use caller-provided context.
            initial_lcl
        };

        for seq in knit_start..=to_ledger {
            self.progress.current_ledger = seq;
            let checkpoint = checkpoint::checkpoint_containing(seq);

            if let std::collections::hash_map::Entry::Vacant(e) = checkpoint_cache.entry(checkpoint)
            {
                let (downloaded, archive_name) = self
                    .download_checkpoint_ledger_data(checkpoint, pinned)
                    .await?;
                last_archive_name = archive_name;
                e.insert(downloaded);
            }

            let cache = checkpoint_cache.get(&checkpoint).ok_or_else(|| {
                HistoryError::CatchupFailed(format!("missing checkpoint cache for {}", checkpoint))
            })?;

            let header_entry_opt = cache.headers.iter().find(|h| h.header.ledger_seq == seq);

            // Knit-prefix entries (seq <= LCL): collected as raw archive
            // entries for the §11.2 decision matrix. They are NOT replayed
            // and must not feed into the chain-verify / tx-set pipeline,
            // which assumes a contiguous LCL → target chain.
            //
            // Stellar-core only consults headers already present in the
            // current checkpoint file (`ApplyCheckpointWork::getNextLedgerCloseData()`).
            // If a knit-prefix entry is absent from the archive, that's
            // expected — we silently skip it instead of erroring.
            if seq < apply_start {
                if let Some(entry) = header_entry_opt {
                    knit_entries.push(entry.clone());
                }
                continue;
            }

            let header_entry = header_entry_opt.ok_or_else(|| {
                HistoryError::CatchupFailed(format!(
                    "ledger {} not found in checkpoint headers",
                    seq
                ))
            })?;

            let header = header_entry.header.clone();
            let header_hash = Hash256(header_entry.hash.0);

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
                &current_lcl,
            )?);

            // This header becomes the LCL for the next iteration.
            current_lcl = LclContext::new(header.ledger_version, header_hash);
        }

        Ok((data, knit_entries, last_archive_name))
    }

    /// Download ONLY the ledger headers (plus knit-prefix entries) for the
    /// full `[from_ledger+1, to_ledger]` range, used by the up-front
    /// header-chain verification phase (#2901).
    ///
    /// Mirrors the header-extraction half of [`download_ledger_data`] but
    /// discards the per-checkpoint transaction/result bodies as soon as the
    /// headers have been copied out, so peak memory is bounded to ~one
    /// checkpoint of body data rather than the whole gap. The headers
    /// themselves (small, fixed-size records) are retained for the full
    /// range so [`verify::verify_reverse_walk`] can thread trust top-down
    /// across all checkpoints exactly as before.
    ///
    /// Returns `(headers, knit_entries, archive_name)`. `headers` covers the
    /// apply range `[from_ledger+1, to_ledger]` in ascending order;
    /// `knit_entries` covers `[knit_start, from_ledger]` (the §11.2 knit
    /// prefix). This matches the apply/knit split produced by
    /// [`download_ledger_data`] so the two phases stay in lockstep.
    pub(super) async fn download_ledger_headers(
        &mut self,
        from_ledger: u32,
        to_ledger: u32,
        pinned: Option<&Arc<HistoryArchive>>,
    ) -> Result<(Vec<LedgerHeader>, Vec<LedgerHeaderHistoryEntry>, String)> {
        let mut headers: Vec<LedgerHeader> = Vec::new();
        let mut knit_entries: Vec<LedgerHeaderHistoryEntry> = Vec::new();
        let mut last_archive_name = "none".to_owned();

        let apply_start = from_ledger.saturating_add(1);
        let knit_start = if from_ledger == 0 {
            apply_start
        } else {
            let apply_ckpt_start = crate::checkpoint::checkpoint_start(apply_start);
            std::cmp::max(apply_ckpt_start, from_ledger.saturating_sub(1))
        };

        if apply_start > to_ledger {
            return Ok((headers, knit_entries, last_archive_name));
        }

        // Walk checkpoint-by-checkpoint. Each checkpoint's full data (headers
        // + tx/result bodies) is downloaded, the relevant headers are copied
        // out, and the bodies are dropped at the end of the checkpoint block —
        // bounding peak body memory to a single checkpoint.
        let mut seq = knit_start;
        while seq <= to_ledger {
            let checkpoint = checkpoint::checkpoint_containing(seq);
            let (cache, archive_name) = self
                .download_checkpoint_ledger_data(checkpoint, pinned)
                .await?;
            last_archive_name = archive_name;

            let checkpoint_end = std::cmp::min(checkpoint, to_ledger);
            for s in seq..=checkpoint_end {
                self.progress.current_ledger = s;
                let header_entry_opt = cache.headers.iter().find(|h| h.header.ledger_seq == s);
                if s < apply_start {
                    // Knit-prefix entry: collected if present, silently skipped
                    // otherwise (same policy as `download_ledger_data`).
                    if let Some(entry) = header_entry_opt {
                        knit_entries.push(entry.clone());
                    }
                    continue;
                }
                let header_entry = header_entry_opt.ok_or_else(|| {
                    HistoryError::CatchupFailed(format!(
                        "ledger {} not found in checkpoint headers",
                        s
                    ))
                })?;
                headers.push(header_entry.header.clone());
            }

            // Free this checkpoint's bodies before fetching the next checkpoint.
            drop(cache);
            seq = checkpoint_end.saturating_add(1);
        }

        Ok((headers, knit_entries, last_archive_name))
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
        pinned: Option<&Arc<HistoryArchive>>,
    ) -> Result<(CheckpointLedgerData, String)> {
        // #2940: when pinned, serve from that archive only (no rotation). The
        // single-element slice keeps the rotation loop below uniform.
        let pinned_slice;
        let archives: &[Arc<HistoryArchive>] = match pinned {
            Some(archive) => {
                pinned_slice = [Arc::clone(archive)];
                &pinned_slice
            }
            None => &self.archives,
        };

        // Try each archive until one succeeds
        let mut last_archive_name = String::new();
        for archive in archives {
            last_archive_name = archive.name().to_owned();
            match self.try_download_checkpoint(archive, checkpoint).await {
                Ok(data) => {
                    let archive_name = archive.name().to_owned();
                    metrics::counter!(
                        "stellar_history_download_ledger_success_total",
                        "archive" => archive_name.clone(),
                    )
                    .increment(1);
                    return Ok((data, archive_name));
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

        metrics::counter!(
            "stellar_history_download_ledger_failure_total",
            "archive" => last_archive_name.clone(),
        )
        .increment(1);
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
        pinned: Option<&Arc<HistoryArchive>>,
    ) -> Result<(LedgerHeader, Hash256)> {
        // #2940: when pinned, serve from that archive only (no rotation).
        let pinned_slice;
        let archives: &[Arc<HistoryArchive>] = match pinned {
            Some(archive) => {
                pinned_slice = [Arc::clone(archive)];
                &pinned_slice
            }
            None => &self.archives,
        };

        for archive in archives {
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

    /// Returns the `TempDir` guard backing the `BucketManager` together
    /// with the manager. The `TempDir` is returned **first** so callers
    /// destructure as `let (_tmp_dir, manager) = ...`: Rust drops local
    /// bindings in reverse declaration order, so binding `manager`
    /// second guarantees it (and any file handles its `BucketManager`
    /// holds) is dropped before the `TempDir` removes the directory.
    /// The caller must keep `_tmp_dir` alive for the duration of the
    /// test so the bucket directory is deleted on drop rather than
    /// leaked under the system temp location.
    fn make_test_catchup_manager() -> (tempfile::TempDir, CatchupManager) {
        let db = Database::open_in_memory().expect("in-memory db");
        let tmp_dir = tempfile::tempdir().expect("temp dir");
        let bucket_manager =
            BucketManager::new(tmp_dir.path().to_path_buf()).expect("bucket manager");
        let archive = crate::HistoryArchive::new("https://example.com").expect("archive");
        (
            tmp_dir,
            super::super::CatchupManager::new(vec![archive], bucket_manager, db),
        )
    }

    #[test]
    fn test_load_local_has_absent() {
        let (_tmp_dir, mgr) = make_test_catchup_manager();
        let result = mgr.load_local_has();
        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
        assert!(result.unwrap().is_none(), "expected None for fresh DB");
    }

    #[test]
    fn test_load_local_has_valid() {
        let (_tmp_dir, mgr) = make_test_catchup_manager();
        // Structurally valid v1 HAS with BUCKET_LIST_LEVELS zero-hash levels.
        let zero = "0".repeat(64);
        let zero_level = format!(
            r#"{{"curr":"{}","snap":"{}","next":{{"state":0}}}}"#,
            zero, zero
        );
        let levels: Vec<_> = (0..henyey_bucket::BUCKET_LIST_LEVELS)
            .map(|_| zero_level.clone())
            .collect();
        let has_json = format!(
            r#"{{"version":1,"currentLedger":100,"currentBuckets":[{}]}}"#,
            levels.join(",")
        );
        mgr.db
            .with_connection(|conn| {
                conn.set_state(
                    henyey_db::schema::state_keys::HISTORY_ARCHIVE_STATE,
                    &has_json,
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
        let (_tmp_dir, mgr) = make_test_catchup_manager();
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

    /// Build a `CatchupManager` over the given archive URLs (in order), backed
    /// by a temp bucket dir + in-memory DB. Returns the `TempDir` guard first so
    /// callers destructure `let (_tmp, mgr) = ...` (drop order: mgr before tmp).
    fn make_manager_with_archives(urls: &[&str]) -> (tempfile::TempDir, CatchupManager) {
        let db = Database::open_in_memory().expect("in-memory db");
        let tmp_dir = tempfile::tempdir().expect("temp dir");
        let bucket_manager =
            BucketManager::new(tmp_dir.path().to_path_buf()).expect("bucket manager");
        let archives: Vec<_> = urls
            .iter()
            .map(|u| crate::HistoryArchive::new(u).expect("archive"))
            .collect();
        (
            tmp_dir,
            super::super::CatchupManager::new(archives, bucket_manager, db),
        )
    }

    /// #2940: `download_has` returns the `Arc<HistoryArchive>` that actually
    /// served the HAS — not merely the first archive in the list. Here the
    /// first archive is dead (bad host) and the second serves the checkpoint,
    /// so the returned archive's `base_url` must be the second fixture's.
    #[tokio::test]
    async fn test_download_has_returns_serving_archive() {
        let checkpoint = 63u32;
        let fixture = match crate::test_utils::build_single_checkpoint_archive(checkpoint).await {
            Ok(f) => f,
            Err(e) => {
                eprintln!("skipping test: {e}");
                return;
            }
        };
        // First archive: an unreachable host (HAS fetch fails); second: the
        // live fixture. download_has must rotate to and return the live one.
        let dead_url = "http://127.0.0.1:1/";
        let (_tmp, mgr) = make_manager_with_archives(&[dead_url, &fixture.base_url]);

        let (has, archive) = mgr.download_has(checkpoint).await.expect("download_has");
        assert_eq!(has.current_ledger(), checkpoint);
        assert_eq!(
            archive.base_url().as_str(),
            fixture.base_url.as_str(),
            "returned archive must be the one that actually served the HAS"
        );
    }

    /// #2940: a pinned `download_checkpoint_header` issues the request against
    /// the pinned archive ONLY. Pinning to the live fixture succeeds even when
    /// it is not first in the list; pinning to a dead archive fails without
    /// falling back; and `None` preserves rotation (finds the live archive).
    #[tokio::test]
    async fn test_download_checkpoint_header_pin_targets_only_pinned() {
        let checkpoint = 63u32;
        let fixture = match crate::test_utils::build_single_checkpoint_archive(checkpoint).await {
            Ok(f) => f,
            Err(e) => {
                eprintln!("skipping test: {e}");
                return;
            }
        };
        let dead_url = "http://127.0.0.1:1/";
        // Order [live, dead]: the live archive is index 0.
        let (_tmp, mgr) = make_manager_with_archives(&[&fixture.base_url, dead_url]);
        let live = Arc::clone(&mgr.archives[0]);
        let dead = Arc::clone(&mgr.archives[1]);

        // Pinned to the live archive → succeeds.
        let (header, _hash) = mgr
            .download_checkpoint_header(checkpoint, Some(&live))
            .await
            .expect("pinned-to-live header download");
        assert_eq!(header.ledger_seq, checkpoint);

        // Pinned to the dead archive → fails, with NO fallback to the live one.
        let err = mgr
            .download_checkpoint_header(checkpoint, Some(&dead))
            .await
            .expect_err("pinned-to-dead header download must fail without fallback");
        assert!(
            matches!(err, HistoryError::CatchupFailed(_)),
            "expected CatchupFailed, got: {err:?}"
        );

        // None → rotation finds the live archive (index 0) and succeeds.
        let (header, _hash) = mgr
            .download_checkpoint_header(checkpoint, None)
            .await
            .expect("unpinned header download rotates to live archive");
        assert_eq!(header.ledger_seq, checkpoint);
    }

    /// Stage E: download counters in this module must carry the `"archive"` label.
    #[test]
    fn test_stage_e_download_archive_label_present() {
        let src = include_str!("download.rs");
        for metric in &[
            "stellar_history_download_bucket_success_total",
            "stellar_history_download_bucket_failure_total",
            "stellar_history_download_ledger_success_total",
            "stellar_history_download_ledger_failure_total",
        ] {
            let idx = src
                .find(metric)
                .unwrap_or_else(|| panic!("metric {metric} not found in catchup/download.rs"));
            let window = &src[idx..std::cmp::min(idx + 200, src.len())];
            assert!(
                window.contains("\"archive\""),
                "metric {metric} missing \"archive\" label in catchup/download.rs",
            );
        }
    }
}
